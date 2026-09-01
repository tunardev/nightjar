use anyhow::{Context, Result, bail};
use jiff::Timestamp;
use nightjar_config::OnFailure;
use nightjar_config::redact::redact_text;
use nightjar_config::secrets::SecretValue;
use nightjar_core::paths::Paths;
use nightjar_core::process::{own_process_group, signal_group};
use nightjar_store::Store;
use std::fs::{File, OpenOptions};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Condvar, Mutex};
use std::time::{Duration, Instant};

#[derive(Debug, Clone, PartialEq)]
pub enum Alert {
    Failed { job: String, exit_code: Option<i32> },
    TimedOut { job: String },
    LimitExceeded { job: String },
    Overdue { job: String, since: jiff::Timestamp },
}

impl Alert {
    pub(crate) fn job(&self) -> &str {
        match self {
            Alert::Failed { job, .. }
            | Alert::TimedOut { job }
            | Alert::LimitExceeded { job }
            | Alert::Overdue { job, .. } => job,
        }
    }

    fn kind(&self) -> &'static str {
        match self {
            Alert::Failed { .. } => "failed",
            Alert::TimedOut { .. } => "timed_out",
            Alert::LimitExceeded { .. } => "limit_exceeded",
            Alert::Overdue { .. } => "overdue",
        }
    }

    pub fn summary(&self) -> String {
        match self {
            Alert::Failed {
                job,
                exit_code: Some(code),
            } => format!("nightjar: {job:?} failed (exit {code})"),
            Alert::Failed {
                job,
                exit_code: None,
            } => format!("nightjar: {job:?} failed (no exit code)"),
            Alert::TimedOut { job } => format!("nightjar: {job:?} timed out"),
            Alert::LimitExceeded { job } => {
                format!("nightjar: {job:?} exceeded a resource limit")
            }
            Alert::Overdue { job, since } => {
                format!("nightjar: {job:?} is overdue (expected at {since})")
            }
        }
    }
}

#[derive(Debug)]
pub struct NotifyOutcome {
    pub channel: &'static str,
    pub result: Result<()>,
}

pub trait Notifier: Send + Sync {
    fn send(
        &self,
        alert: &Alert,
        on_failure: &OnFailure,
        redact: &[SecretValue],
    ) -> Vec<NotifyOutcome>;
}

pub struct RealNotifier;

impl Notifier for RealNotifier {
    fn send(
        &self,
        alert: &Alert,
        on_failure: &OnFailure,
        redact: &[SecretValue],
    ) -> Vec<NotifyOutcome> {
        let mut outcomes = Vec::new();
        if on_failure.notify {
            outcomes.push(NotifyOutcome {
                channel: "desktop",
                result: send_desktop(alert, redact),
            });
        }
        if let Some(run) = &on_failure.run {
            outcomes.push(NotifyOutcome {
                channel: "command",
                result: send_command(run),
            });
        }
        if let Some(url) = &on_failure.webhook {
            outcomes.push(NotifyOutcome {
                channel: "webhook",
                result: send_webhook(url, alert, redact),
            });
        }
        outcomes
    }
}

/// Sends through `notifier` and logs any channel that failed. Stamps the
/// cooldown once at least one channel gets through. Shared by two callers,
/// so "a real success stamps the cooldown" has one definition.
pub fn send_and_stamp_cooldown(
    alert: &Alert,
    on_failure: &OnFailure,
    store: &Store,
    now: Timestamp,
    notifier: &dyn Notifier,
    redact: &[SecretValue],
) {
    let outcomes = notifier.send(alert, on_failure, redact);
    for outcome in &outcomes {
        if let Err(e) = &outcome.result {
            eprintln!(
                "nightjar: {} alert failed for job {:?}: {e:#}",
                outcome.channel,
                alert.job()
            );
        }
    }
    if outcomes.iter().any(|o| o.result.is_ok()) {
        let _ = store.set_last_notified_at(alert.job(), now);
    }
}

/// Dispatches through a detached `nightjar notify` child, not inline.
/// `nightjar exec` exits once its run is recorded, so only a separate
/// process can outlive it for as long as a slow channel takes.
///
/// Always reports no outcomes. Only the child knows whether the send
/// succeeded, and only it can stamp the cooldown truthfully.
pub struct DetachedNotifier;

impl Notifier for DetachedNotifier {
    // The detached child re-resolves and receives no secret of its own, so
    // `redact` exists only to satisfy the shared `Notifier` trait.
    fn send(
        &self,
        alert: &Alert,
        on_failure: &OnFailure,
        _redact: &[SecretValue],
    ) -> Vec<NotifyOutcome> {
        if on_failure.has_channel()
            && let Err(e) = spawn_detached(alert, on_failure)
        {
            eprintln!("nightjar: could not start detached notifier: {e:#}");
        }
        Vec::new()
    }
}

/// Environment, not argv, carries the failure command and webhook URL to
/// the detached child. A Slack or Discord webhook URL is a bearer secret,
/// and `on_failure.run` often embeds one; argv is world-readable through
/// `ps` and `/proc/<pid>/cmdline` for as long as the channel takes.
pub const RUN_CMD_ENV: &str = "NIGHTJAR_NOTIFY_RUN_CMD";
pub const WEBHOOK_ENV: &str = "NIGHTJAR_NOTIFY_WEBHOOK";

/// Builds the `nightjar notify` invocation for `alert`/`on_failure`. Split
/// from spawning so a test can inspect the arguments without starting a
/// child process.
///
/// `--job` carries the true job name, never a redacted one. `cmd_notify`
/// uses it as the store key, so substituting a marker would stamp the
/// cooldown against a job that does not exist.
fn notify_command(exe: &std::path::Path, alert: &Alert, on_failure: &OnFailure) -> Result<Command> {
    let (job, kind, exit_code) = match alert {
        Alert::Failed { job, exit_code } => (job.as_str(), "failed", *exit_code),
        Alert::TimedOut { job } => (job.as_str(), "timed_out", None),
        Alert::LimitExceeded { job } => (job.as_str(), "limit_exceeded", None),
        Alert::Overdue { .. } => bail!(
            "overdue alerts are dispatched by the daemon's own background thread, not DetachedNotifier"
        ),
    };

    let mut cmd = Command::new(exe);
    cmd.arg("notify")
        .arg(format!("--job={job}"))
        .arg(format!("--kind={kind}"));
    if let Some(code) = exit_code {
        cmd.arg(format!("--exit-code={code}"));
    }
    if on_failure.notify {
        cmd.arg("--notify");
    }
    // Cleared, not merely unset, so a stale value in this process's own
    // environment can never reach the child as a channel it didn't ask for.
    cmd.env_remove(RUN_CMD_ENV).env_remove(WEBHOOK_ENV);
    if let Some(run) = &on_failure.run {
        cmd.env(RUN_CMD_ENV, run);
    }
    if let Some(url) = &on_failure.webhook {
        cmd.env(WEBHOOK_ENV, url);
    }
    Ok(cmd)
}

/// Opens (creating if needed) the file every detached notify child's own
/// stdout/stderr are redirected to. Never inherited from the spawning
/// process. A caller capturing `nightjar run`'s output (shell `$(...)`, CI
/// log capture) would otherwise hand this child a duplicate of that pipe.
/// The reader on the other end blocks on EOF until every holder closes it,
/// including this child, for as long as its channel timeout takes.
fn notify_log_file() -> Result<File> {
    use std::os::unix::fs::OpenOptionsExt;

    let paths = Paths::resolve()?;
    paths.ensure_dirs()?;
    // A failed channel's error can quote the webhook host or the failure
    // command, so the log is the owner's alone.
    OpenOptions::new()
        .create(true)
        .append(true)
        .mode(0o600)
        .open(paths.data_dir.join("notify.log"))
        .context("opening notify.log")
}

fn spawn_detached(alert: &Alert, on_failure: &OnFailure) -> Result<()> {
    let exe = std::env::current_exe().context("locating own executable")?;
    let mut cmd = notify_command(&exe, alert, on_failure)?;
    cmd.stdin(Stdio::null());
    match notify_log_file() {
        Ok(log) => {
            let stderr_log = log.try_clone().context("cloning notify.log handle")?;
            cmd.stdout(log).stderr(stderr_log);
        }
        Err(e) => {
            eprintln!(
                "nightjar: could not open notify.log, discarding the detached notifier's own output: {e:#}"
            );
            cmd.stdout(Stdio::null()).stderr(Stdio::null());
        }
    }
    // Its own group, not the wrapper's. A terminal signal, or a service
    // manager stopping the wrapper's group, must not also cut off a channel
    // still within its own timeout.
    own_process_group(&mut cmd);

    // The orphan reparents to init once this process exits, and is reaped
    // there, like any other detached Unix child.
    drop(cmd.spawn().context("spawning nightjar notify")?);
    Ok(())
}

fn desktop_command(os: &str, summary: &str) -> Command {
    if os == "macos" {
        let script = format!(
            "display notification {} with title \"nightjar\"",
            applescript_string_literal(summary)
        );
        let mut cmd = Command::new("osascript");
        cmd.arg("-e").arg(script);
        cmd
    } else {
        let mut cmd = Command::new("notify-send");
        cmd.arg("nightjar").arg(summary);
        cmd
    }
}

fn applescript_string_literal(s: &str) -> String {
    let escaped = s.replace('\\', "\\\\").replace('"', "\\\"");
    format!("\"{escaped}\"")
}

fn send_desktop(alert: &Alert, redact: &[SecretValue]) -> Result<()> {
    let summary = redact_text(redact, &alert.summary());
    run_with_timeout(
        desktop_command(std::env::consts::OS, &summary),
        PROCESS_TIMEOUT,
        PROCESS_GRACE,
        "desktop notifier",
    )
}

const PROCESS_TIMEOUT: Duration = Duration::from_secs(10);
const PROCESS_GRACE: Duration = Duration::from_secs(10);
// Bounds the post-SIGKILL wait, deliberately shorter than a full grace.
// Same shape as PUMP_DRAIN in `exec.rs`.
const KILL_DRAIN: Duration = Duration::from_secs(5);
const POLL: Duration = Duration::from_millis(20);

fn send_command(command: &str) -> Result<()> {
    run_shell_command(command, true, PROCESS_TIMEOUT, PROCESS_GRACE)
}

fn run_shell_command(
    command: &str,
    login_shell: bool,
    timeout: Duration,
    grace: Duration,
) -> Result<()> {
    let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".to_string());
    let mut cmd = Command::new(&shell);
    cmd.arg(if login_shell { "-lc" } else { "-c" }).arg(command);
    run_with_timeout(cmd, timeout, grace, "notify command")
}

fn deadline_after(budget: Duration, label: &str) -> Result<Instant> {
    Instant::now()
        .checked_add(budget)
        .with_context(|| format!("{label}: duration too large to represent as a deadline"))
}

/// Shared by the desktop and command channels. Both need to run an
/// external program without letting it block the wrapper forever.
fn run_with_timeout(
    mut cmd: Command,
    timeout: Duration,
    grace: Duration,
    label: &str,
) -> Result<()> {
    cmd.stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    own_process_group(&mut cmd);

    let mut child = cmd.spawn().with_context(|| format!("spawning {label}"))?;
    let pid = child.id();
    let mut guard = ChildGuard::new(&mut child, pid);
    let deadline = deadline_after(timeout, label)?;
    let mut exited: Option<ExitStatus> = None;

    // The process exiting isn't enough on its own. A backgrounded descendant
    // (`sleep 5 &`) can keep the group alive after the top-level one exits.
    loop {
        if exited.is_none() {
            exited = guard
                .child
                .try_wait()
                .with_context(|| format!("waiting for {label}"))?;
        }
        if let Some(status) = exited
            && group_is_empty(pid)
        {
            guard.mark_done();
            return exit_result(status, label);
        }
        if Instant::now() >= deadline {
            let result = escalate(guard.child, pid, grace, label);
            guard.mark_done();
            return result;
        }
        std::thread::sleep(POLL);
    }
}

/// Guards against an early `?` return leaving the group running and the
/// child unreaped. `Child::drop` does neither on its own.
struct ChildGuard<'a> {
    child: &'a mut Child,
    pid: u32,
    done: bool,
}

impl<'a> ChildGuard<'a> {
    fn new(child: &'a mut Child, pid: u32) -> Self {
        Self {
            child,
            pid,
            done: false,
        }
    }

    fn mark_done(&mut self) {
        self.done = true;
    }
}

impl Drop for ChildGuard<'_> {
    fn drop(&mut self) {
        if self.done {
            return;
        }
        signal_group(self.pid, libc::SIGKILL);
        let _ = self.child.wait();
    }
}

#[allow(clippy::similar_names)] // pid/pgid is standard POSIX terminology, not a typo risk
fn group_is_empty(pid: u32) -> bool {
    let Ok(pgid) = libc::pid_t::try_from(pid) else {
        return true;
    };
    if pgid <= 1 {
        return true;
    }
    unsafe { libc::kill(-pgid, 0) != 0 }
}

/// Gates the grace window on group liveness, not `Child::try_wait`, which
/// would replay a cached exit and fire the bail before SIGKILL ever ran.
/// Grace is spent once, on SIGTERM. The post-SIGKILL wait uses `reap`'s
/// own shorter budget, since SIGKILL, unlike SIGTERM, cannot be ignored.
fn escalate(child: &mut Child, pid: u32, grace: Duration, label: &str) -> Result<()> {
    signal_group(pid, libc::SIGTERM);
    wait_while_group_alive(child, pid, grace, label)?;
    if !group_is_empty(pid) {
        signal_group(pid, libc::SIGKILL);
    }
    reap(child, KILL_DRAIN, label)?;
    bail!("{label} timed out")
}

/// Reaps the shell each pass before testing the group. On Linux, a zombie
/// still answers `kill(-pgid, 0)`, so leaving it unreaped would burn the
/// whole grace window on every timeout. macOS doesn't have this quirk.
fn wait_while_group_alive(
    child: &mut Child,
    pid: u32,
    budget: Duration,
    label: &str,
) -> Result<()> {
    let deadline = deadline_after(budget, label)?;
    loop {
        let _ = child.try_wait();
        if group_is_empty(pid) || Instant::now() >= deadline {
            return Ok(());
        }
        std::thread::sleep(POLL);
    }
}

/// Bounded even though SIGKILL should make this prompt. A process stuck
/// in uninterruptible sleep can't be forced to exit by any signal. The
/// caller must not block on one that never yields.
fn reap(child: &mut Child, budget: Duration, label: &str) -> Result<()> {
    let deadline = deadline_after(budget, label)?;
    loop {
        if child
            .try_wait()
            .with_context(|| format!("waiting for {label}"))?
            .is_some()
        {
            return Ok(());
        }
        if Instant::now() >= deadline {
            return Ok(());
        }
        std::thread::sleep(POLL);
    }
}

fn exit_result(status: ExitStatus, label: &str) -> Result<()> {
    if status.success() {
        Ok(())
    } else {
        bail!("{label} exited with {status}")
    }
}

const WEBHOOK_TIMEOUT: Duration = Duration::from_secs(10);

fn send_webhook(url: &str, alert: &Alert, redact: &[SecretValue]) -> Result<()> {
    post_webhook(url, alert, redact, WEBHOOK_TIMEOUT)
}

fn post_webhook(url: &str, alert: &Alert, redact: &[SecretValue], timeout: Duration) -> Result<()> {
    let body = webhook_body(alert, redact);
    let response = ureq::post(url)
        .header("Content-Type", "application/json")
        .config()
        .timeout_global(Some(timeout))
        .http_status_as_error(false)
        .build()
        .send(body.as_str())
        .context("sending webhook")?;

    if response.status().is_success() {
        Ok(())
    } else {
        bail!("webhook returned {}", response.status())
    }
}

/// Redacts the job name and summary before JSON-escaping, not after. A
/// secret containing a quote or backslash comes out of the encoder
/// different from its raw form. Matching only works before that runs.
fn webhook_body(alert: &Alert, redact: &[SecretValue]) -> String {
    serde_json::json!({
        "job": redact_text(redact, alert.job()),
        "kind": alert.kind(),
        "summary": redact_text(redact, &alert.summary()),
    })
    .to_string()
}

#[derive(Debug, Clone, PartialEq)]
pub struct Call {
    pub channel: &'static str,
    pub alert: Alert,
    pub observed_at: Instant,
}

pub struct RecordingNotifier {
    calls: Mutex<Vec<Call>>,
    /// Paired with `calls`. A waiter locks `calls` and blocks on this, so a
    /// `notify_all` after unlocking can't land in the gap before the wait
    /// begins — the standard `Condvar` pattern.
    calls_changed: Condvar,
    fail_channels: Vec<&'static str>,
    send_count: AtomicUsize,
}

impl Default for RecordingNotifier {
    fn default() -> Self {
        Self {
            calls: Mutex::new(Vec::new()),
            calls_changed: Condvar::new(),
            fail_channels: Vec::new(),
            send_count: AtomicUsize::new(0),
        }
    }
}

impl RecordingNotifier {
    pub fn failing_on(channel: &'static str) -> Self {
        Self::failing_on_channels(&[channel])
    }

    /// Lets a test fail every configured channel at once, not just one.
    pub fn failing_on_channels(channels: &[&'static str]) -> Self {
        Self {
            calls: Mutex::new(Vec::new()),
            calls_changed: Condvar::new(),
            fail_channels: channels.to_vec(),
            send_count: AtomicUsize::new(0),
        }
    }

    pub fn calls(&self) -> Vec<Call> {
        self.calls.lock().unwrap().clone()
    }

    /// Blocks until at least `n` calls are recorded or `timeout` elapses.
    /// Lets a test synchronize with a detached dispatch thread it cannot
    /// join (the daemon deliberately doesn't). No sleeping and hoping a
    /// fixed duration was long enough.
    pub fn wait_for_calls(&self, n: usize, timeout: Duration) -> Vec<Call> {
        let guard = self.calls.lock().unwrap();
        let (guard, _) = self
            .calls_changed
            .wait_timeout_while(guard, timeout, |calls| calls.len() < n)
            .unwrap();
        guard.clone()
    }

    /// Counts every `send`, even with nothing configured. Unlike
    /// `calls().is_empty()`, this distinguishes "never called" from "called
    /// with nothing configured".
    pub fn send_count(&self) -> usize {
        self.send_count.load(Ordering::SeqCst)
    }

    fn record(&self, channel: &'static str, alert: &Alert) -> NotifyOutcome {
        let mut calls = self.calls.lock().unwrap();
        calls.push(Call {
            channel,
            alert: alert.clone(),
            observed_at: Instant::now(),
        });
        drop(calls);
        self.calls_changed.notify_all();
        let result = if self.fail_channels.contains(&channel) {
            Err(anyhow::anyhow!("{channel} configured to fail"))
        } else {
            Ok(())
        };
        NotifyOutcome { channel, result }
    }
}

impl Notifier for RecordingNotifier {
    fn send(
        &self,
        alert: &Alert,
        on_failure: &OnFailure,
        _redact: &[SecretValue],
    ) -> Vec<NotifyOutcome> {
        self.send_count.fetch_add(1, Ordering::SeqCst);
        let mut outcomes = Vec::new();
        if on_failure.notify {
            outcomes.push(self.record("desktop", alert));
        }
        if on_failure.run.is_some() {
            outcomes.push(self.record("command", alert));
        }
        if on_failure.webhook.is_some() {
            outcomes.push(self.record("webhook", alert));
        }
        outcomes
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::{TcpListener, TcpStream};
    use std::path::Path;

    fn on_failure(notify: bool, run: Option<&str>, webhook: Option<&str>) -> OnFailure {
        OnFailure {
            notify,
            run: run.map(str::to_string),
            webhook: webhook.map(str::to_string),
        }
    }

    #[test]
    fn summary_names_the_job_and_what_happened() {
        let a = Alert::Failed {
            job: "backup".into(),
            exit_code: Some(3),
        };
        let s = a.summary();
        assert!(s.contains("backup"), "summary must name the job: {s}");
        assert!(s.contains('3'), "summary must carry the exit code: {s}");

        let t = Alert::TimedOut { job: "slow".into() };
        assert!(t.summary().contains("slow"));
        assert!(
            t.summary().to_lowercase().contains("time"),
            "got: {}",
            t.summary()
        );
    }

    #[test]
    fn overdue_summary_names_the_job_and_says_overdue() {
        let since: jiff::Timestamp = "2026-08-24T02:00:00Z".parse().unwrap();
        let a = Alert::Overdue {
            job: "fetch-rates".into(),
            since,
        };
        let s = a.summary();
        assert!(s.contains("fetch-rates"), "got: {s}");
        assert!(s.to_lowercase().contains("overdue"), "got: {s}");
    }

    #[test]
    fn channel_is_not_attempted_when_it_is_not_configured() {
        let n = RecordingNotifier::default();
        let a = Alert::Failed {
            job: "j".into(),
            exit_code: Some(1),
        };
        n.send(&a, &on_failure(false, None, None), &[]);
        assert!(
            n.calls().is_empty(),
            "nothing configured means nothing sent"
        );
    }

    #[test]
    fn every_configured_channel_is_attempted_exactly_once() {
        let n = RecordingNotifier::default();
        let a = Alert::Failed {
            job: "j".into(),
            exit_code: Some(1),
        };
        n.send(
            &a,
            &on_failure(true, Some("true"), Some("https://example.invalid/hook")),
            &[],
        );

        let mut chans: Vec<&str> = n.calls().iter().map(|c| c.channel).collect();
        chans.sort_unstable();
        assert_eq!(chans, vec!["command", "desktop", "webhook"]);
    }

    #[test]
    fn other_channels_are_still_attempted_when_one_channel_fails() {
        let n = RecordingNotifier::failing_on("command");
        let a = Alert::Failed {
            job: "j".into(),
            exit_code: Some(1),
        };
        let outcomes = n.send(
            &a,
            &on_failure(true, Some("false"), Some("https://example.invalid/hook")),
            &[],
        );

        assert_eq!(outcomes.len(), 3, "all three still attempted");
        assert_eq!(outcomes.iter().filter(|o| o.result.is_err()).count(), 1);
        assert_eq!(outcomes.iter().filter(|o| o.result.is_ok()).count(), 2);
    }

    #[test]
    fn recording_notifier_captures_the_alert_itself_not_just_the_channel() {
        let n = RecordingNotifier::default();
        let timed_out = Alert::TimedOut { job: "slow".into() };
        n.send(&timed_out, &on_failure(true, None, None), &[]);

        let calls = n.calls();
        assert_eq!(calls.len(), 1);
        assert_eq!(
            calls[0].alert, timed_out,
            "a test asserting only on channel/count would miss the wrong Alert variant being dispatched"
        );
    }

    #[test]
    fn send_count_distinguishes_never_called_from_called_with_nothing_configured() {
        let n = RecordingNotifier::default();
        assert_eq!(n.send_count(), 0);

        let a = Alert::Failed {
            job: "j".into(),
            exit_code: Some(1),
        };
        n.send(&a, &on_failure(false, None, None), &[]);

        assert_eq!(
            n.send_count(),
            1,
            "send_count must increment even when nothing was configured"
        );
        assert!(n.calls().is_empty(), "still nothing configured to call");
    }

    #[test]
    fn failing_on_channels_can_fail_every_channel_at_once() {
        let n = RecordingNotifier::failing_on_channels(&["desktop", "command", "webhook"]);
        let a = Alert::Failed {
            job: "j".into(),
            exit_code: Some(1),
        };
        let outcomes = n.send(
            &a,
            &on_failure(true, Some("true"), Some("https://example.invalid/hook")),
            &[],
        );

        assert_eq!(outcomes.len(), 3);
        assert!(
            outcomes.iter().all(|o| o.result.is_err()),
            "every channel must be failable at once, not just one"
        );
    }

    #[test]
    fn recorded_calls_carry_an_observed_at_a_caller_can_order_against() {
        let n = RecordingNotifier::default();
        let before = Instant::now();
        let a = Alert::Failed {
            job: "j".into(),
            exit_code: Some(1),
        };
        n.send(&a, &on_failure(true, None, None), &[]);
        let after = Instant::now();

        let calls = n.calls();
        assert_eq!(calls.len(), 1);
        assert!(
            calls[0].observed_at >= before && calls[0].observed_at <= after,
            "observed_at must fall within the call's before/after window"
        );
    }

    #[test]
    fn notify_command_carries_the_job_kind_and_exit_code() {
        let alert = Alert::Failed {
            job: "backup".into(),
            exit_code: Some(3),
        };
        let cmd = notify_command(
            Path::new("/usr/local/bin/nightjar"),
            &alert,
            &on_failure(false, None, None),
        )
        .unwrap();
        let args: Vec<&str> = cmd.get_args().map(|a| a.to_str().unwrap()).collect();
        assert_eq!(
            args,
            vec!["notify", "--job=backup", "--kind=failed", "--exit-code=3"]
        );
    }

    #[test]
    fn notify_command_omits_exit_code_when_the_alert_is_a_timeout() {
        let alert = Alert::TimedOut { job: "slow".into() };
        let cmd = notify_command(
            Path::new("/usr/local/bin/nightjar"),
            &alert,
            &on_failure(false, None, None),
        )
        .unwrap();
        let args: Vec<&str> = cmd.get_args().map(|a| a.to_str().unwrap()).collect();
        assert_eq!(args, vec!["notify", "--job=slow", "--kind=timed_out"]);
    }

    #[test]
    fn notify_command_carries_every_configured_channel() {
        let alert = Alert::Failed {
            job: "j".into(),
            exit_code: None,
        };
        let cmd = notify_command(
            Path::new("/usr/local/bin/nightjar"),
            &alert,
            &on_failure(true, Some("echo hi"), Some("https://example.invalid/hook")),
        )
        .unwrap();
        let args: Vec<&str> = cmd.get_args().map(|a| a.to_str().unwrap()).collect();
        assert_eq!(args, vec!["notify", "--job=j", "--kind=failed", "--notify"]);

        let envs: Vec<(String, Option<String>)> = cmd
            .get_envs()
            .map(|(k, v)| {
                (
                    k.to_string_lossy().into_owned(),
                    v.map(|v| v.to_string_lossy().into_owned()),
                )
            })
            .collect();
        assert!(
            envs.contains(&(RUN_CMD_ENV.to_string(), Some("echo hi".to_string()))),
            "got: {envs:?}"
        );
        assert!(
            envs.contains(&(
                WEBHOOK_ENV.to_string(),
                Some("https://example.invalid/hook".to_string())
            )),
            "got: {envs:?}"
        );
    }

    #[test]
    fn webhook_url_and_failure_command_never_appear_in_the_child_argv() {
        let alert = Alert::Failed {
            job: "j".into(),
            exit_code: Some(1),
        };
        let secret_url = "https://hooks.example.invalid/services/T0/B0/s3cr3t-token";
        let secret_cmd = "curl -H 'Authorization: Bearer s3cr3t-bearer' https://x";
        let cmd = notify_command(
            Path::new("/usr/local/bin/nightjar"),
            &alert,
            &on_failure(false, Some(secret_cmd), Some(secret_url)),
        )
        .unwrap();
        let argv = cmd
            .get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect::<Vec<_>>()
            .join(" ");
        assert!(!argv.contains("s3cr3t"), "argv is world-readable: {argv}");
    }

    #[test]
    fn notify_command_clears_a_stale_channel_from_its_own_environment() {
        let alert = Alert::Failed {
            job: "j".into(),
            exit_code: Some(1),
        };
        let cmd = notify_command(
            Path::new("/usr/local/bin/nightjar"),
            &alert,
            &on_failure(true, None, None),
        )
        .unwrap();
        let cleared: Vec<String> = cmd
            .get_envs()
            .filter(|(_, v)| v.is_none())
            .map(|(k, _)| k.to_string_lossy().into_owned())
            .collect();
        assert!(
            cleared.contains(&RUN_CMD_ENV.to_string()),
            "got: {cleared:?}"
        );
        assert!(
            cleared.contains(&WEBHOOK_ENV.to_string()),
            "got: {cleared:?}"
        );
    }

    #[test]
    fn notify_command_refuses_the_alert_when_it_is_overdue() {
        let alert = Alert::Overdue {
            job: "j".into(),
            since: "2026-08-24T02:00:00Z".parse().unwrap(),
        };
        assert!(
            notify_command(
                Path::new("/usr/local/bin/nightjar"),
                &alert,
                &on_failure(false, None, None)
            )
            .is_err(),
            "the daemon dispatches its own overdue alerts on a background thread"
        );
    }

    #[test]
    fn detached_notifier_never_reports_an_outcome_of_its_own() {
        let alert = Alert::Failed {
            job: "j".into(),
            exit_code: Some(1),
        };
        let outcomes = DetachedNotifier.send(&alert, &on_failure(true, None, None), &[]);
        assert!(
            outcomes.is_empty(),
            "only the detached child knows the real outcome, so this call must never claim one"
        );
    }

    #[test]
    fn detached_notifier_spawns_nothing_when_no_channel_is_configured() {
        let alert = Alert::Failed {
            job: "j".into(),
            exit_code: Some(1),
        };
        let outcomes = DetachedNotifier.send(&alert, &on_failure(false, None, None), &[]);
        assert!(outcomes.is_empty());
    }

    #[test]
    fn send_and_stamp_cooldown_stamps_only_when_a_send_actually_succeeds() {
        let store = Store::open_in_memory().unwrap();
        let now: Timestamp = "2026-08-24T02:00:00Z".parse().unwrap();
        let alert = Alert::Failed {
            job: "j".into(),
            exit_code: Some(1),
        };

        let failing = RecordingNotifier::failing_on("command");
        send_and_stamp_cooldown(
            &alert,
            &on_failure(false, Some("false"), None),
            &store,
            now,
            &failing,
            &[],
        );
        assert_eq!(
            store.last_notified_at("j").unwrap(),
            None,
            "every configured channel failed; the cooldown must not stamp"
        );

        let succeeding = RecordingNotifier::default();
        send_and_stamp_cooldown(
            &alert,
            &on_failure(false, Some("true"), None),
            &store,
            now,
            &succeeding,
            &[],
        );
        assert_eq!(store.last_notified_at("j").unwrap(), Some(now));
    }

    #[test]
    fn desktop_command_targets_osascript_when_the_os_is_macos() {
        let cmd = desktop_command("macos", "backup failed");
        assert_eq!(cmd.get_program().to_str(), Some("osascript"));
    }

    #[test]
    fn desktop_command_targets_notify_send_when_the_os_is_not_macos() {
        let cmd = desktop_command("linux", "backup failed");
        assert_eq!(cmd.get_program().to_str(), Some("notify-send"));
    }

    #[test]
    fn applescript_string_literal_escapes_embedded_quotes() {
        assert_eq!(applescript_string_literal(r#"say "hi""#), r#""say \"hi\"""#);
    }

    #[test]
    fn webhook_body_is_valid_json_when_the_job_name_needs_escaping() {
        let alert = Alert::Failed {
            job: "we\"ird\\name\n".into(),
            exit_code: Some(1),
        };
        let body: serde_json::Value = serde_json::from_str(&webhook_body(&alert, &[])).unwrap();
        assert_eq!(body["job"], "we\"ird\\name\n");
        assert_eq!(body["kind"], "failed");
    }

    #[test]
    fn command_channel_succeeds_when_exit_code_is_zero() {
        assert!(run_shell_command("true", false, PROCESS_TIMEOUT, PROCESS_GRACE).is_ok());
    }

    #[test]
    fn command_channel_fails_when_exit_code_is_nonzero() {
        assert!(run_shell_command("false", false, PROCESS_TIMEOUT, PROCESS_GRACE).is_err());
    }

    #[test]
    fn command_channel_kills_the_command_instead_of_waiting_it_out_when_it_hangs() {
        let started = Instant::now();
        let result = run_shell_command(
            "sleep 60; true",
            false,
            Duration::from_millis(100),
            PROCESS_GRACE,
        );
        assert!(result.is_err(), "a command past its deadline is an error");
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "must not wait out the sleep; took {:?}",
            started.elapsed()
        );
    }

    #[test]
    fn command_channel_kills_descendants_the_shell_forked() {
        let started = Instant::now();
        let result = run_shell_command(
            "sleep 60 & echo started",
            false,
            Duration::from_millis(100),
            PROCESS_GRACE,
        );
        assert!(result.is_err());
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "the backgrounded sleep must die with the shell; took {:?}",
            started.elapsed()
        );
    }

    #[test]
    fn command_channel_sigkills_a_descendant_when_it_ignores_sigterm() {
        let started = Instant::now();
        let result = run_shell_command(
            "trap '' TERM; sleep 30",
            false,
            Duration::from_millis(100),
            Duration::from_millis(150),
        );
        assert!(result.is_err(), "must still be reported as timed out");
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "a SIGTERM-ignoring process must be SIGKILLed within its grace window, not outlive it; took {:?}",
            started.elapsed()
        );
    }

    #[test]
    fn run_with_timeout_bounds_any_command_not_only_shell_ones() {
        let mut cmd = Command::new("sleep");
        cmd.arg("30");
        let started = Instant::now();
        let result = run_with_timeout(
            cmd,
            Duration::from_millis(100),
            Duration::from_millis(150),
            "probe",
        );
        assert!(result.is_err());
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "the desktop channel reuses this same engine; took {:?}",
            started.elapsed()
        );
    }

    struct TestServer {
        addr: std::net::SocketAddr,
    }

    fn read_full_request(stream: &mut TcpStream) -> Vec<u8> {
        let mut data = Vec::new();
        let mut buf = [0u8; 4096];
        loop {
            match stream.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    data.extend_from_slice(&buf[..n]);
                    if let Some(header_end) = find_subslice(&data, b"\r\n\r\n") {
                        let body_len = content_length(&data[..header_end]).unwrap_or(0);
                        if data.len() >= header_end + 4 + body_len {
                            break;
                        }
                    }
                }
                Err(e) => panic!(
                    "reading test request failed after {} bytes: {e}",
                    data.len()
                ),
            }
        }
        data
    }

    fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
        haystack.windows(needle.len()).position(|w| w == needle)
    }

    fn content_length(headers: &[u8]) -> Option<usize> {
        let text = String::from_utf8_lossy(headers);
        for line in text.lines() {
            let lower = line.to_ascii_lowercase();
            if let Some(v) = lower.strip_prefix("content-length:") {
                return v.trim().parse().ok();
            }
        }
        None
    }

    fn serve_once(response: &'static str) -> (TestServer, std::sync::mpsc::Receiver<String>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            if let Ok((mut stream, _)) = listener.accept() {
                stream
                    .set_read_timeout(Some(Duration::from_secs(2)))
                    .unwrap();
                let request = read_full_request(&mut stream);
                let _ = tx.send(String::from_utf8_lossy(&request).to_string());
                let _ = stream.write_all(response.as_bytes());
            }
        });
        (TestServer { addr }, rx)
    }

    #[test]
    fn webhook_channel_sends_job_kind_and_summary_as_json() {
        let (server, rx) =
            serve_once("HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
        let alert = Alert::Failed {
            job: "backup".into(),
            exit_code: Some(1),
        };
        let url = format!("http://{}/hook", server.addr);

        let result = post_webhook(&url, &alert, &[], Duration::from_secs(2));
        assert!(result.is_ok(), "{result:?}");

        let request = rx.recv_timeout(Duration::from_secs(2)).unwrap();
        assert!(request.contains(r#""job":"backup""#), "got: {request}");
        assert!(request.contains(r#""kind":"failed""#), "got: {request}");
        assert!(request.contains("\"summary\":"), "got: {request}");
    }

    #[test]
    fn webhook_channel_treats_the_response_as_an_error_when_the_status_is_not_2xx() {
        let (server, _rx) = serve_once(
            "HTTP/1.1 500 Internal Server Error\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
        );
        let alert = Alert::TimedOut { job: "slow".into() };
        let url = format!("http://{}/hook", server.addr);

        let result = post_webhook(&url, &alert, &[], Duration::from_secs(2));
        assert!(result.is_err(), "a 500 must be a channel error");
    }

    #[test]
    fn webhook_channel_reports_the_status_itself_not_ureqs_generic_error() {
        let (server, _rx) = serve_once(
            "HTTP/1.1 500 Internal Server Error\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
        );
        let alert = Alert::TimedOut { job: "slow".into() };
        let url = format!("http://{}/hook", server.addr);

        let err = post_webhook(&url, &alert, &[], Duration::from_secs(2)).unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("webhook returned"),
            "expected our own status check to fire, not ureq's default status-as-error; got: {msg}"
        );
    }

    #[test]
    fn webhook_channel_times_out_instead_of_hanging_forever_when_the_server_never_responds() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let alert = Alert::TimedOut { job: "slow".into() };
        let url = format!("http://{addr}/hook");

        let started = Instant::now();
        let result = post_webhook(&url, &alert, &[], Duration::from_millis(200));
        assert!(result.is_err(), "a hung server must be a channel error");
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "must not hang past its timeout; took {:?}",
            started.elapsed()
        );
    }

    #[test]
    fn real_notifier_dispatches_through_the_real_webhook_channel() {
        let (server, _rx) =
            serve_once("HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
        let alert = Alert::Failed {
            job: "j".into(),
            exit_code: Some(1),
        };
        let webhook = format!("http://{}/hook", server.addr);
        let cfg = on_failure(false, None, Some(&webhook));

        let outcomes = RealNotifier.send(&alert, &cfg, &[]);

        assert_eq!(outcomes.len(), 1);
        assert_eq!(outcomes[0].channel, "webhook");
        assert!(outcomes[0].result.is_ok(), "{:?}", outcomes[0].result);
    }

    #[test]
    fn real_notifier_surfaces_a_real_webhook_failure() {
        let (server, _rx) = serve_once(
            "HTTP/1.1 500 Internal Server Error\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
        );
        let alert = Alert::TimedOut { job: "slow".into() };
        let webhook = format!("http://{}/hook", server.addr);
        let cfg = on_failure(false, None, Some(&webhook));

        let outcomes = RealNotifier.send(&alert, &cfg, &[]);

        assert_eq!(outcomes.len(), 1);
        assert!(outcomes[0].result.is_err());
    }

    #[test]
    fn webhook_body_redacts_a_secret_when_it_appears_in_the_alert_text() {
        let secret = zeroize::Zeroizing::new("s3cr3t-payload-91a".to_string());
        let alert = Alert::Failed {
            job: secret.as_str().to_string(),
            exit_code: Some(1),
        };

        let body = webhook_body(&alert, std::slice::from_ref(&secret));

        assert!(!body.contains(secret.as_str()), "got: {body}");
        assert!(body.contains("[nightjar:redacted]"), "got: {body}");
    }

    #[test]
    fn real_webhook_channel_never_sends_a_configured_secret() {
        let (server, rx) =
            serve_once("HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
        let secret = zeroize::Zeroizing::new("s3cr3t-wire-4b2".to_string());
        let alert = Alert::Failed {
            job: secret.as_str().to_string(),
            exit_code: Some(1),
        };
        let webhook = format!("http://{}/hook", server.addr);
        let cfg = on_failure(false, None, Some(&webhook));

        let outcomes = RealNotifier.send(&alert, &cfg, std::slice::from_ref(&secret));
        assert!(outcomes[0].result.is_ok(), "{:?}", outcomes[0].result);

        let request = rx.recv_timeout(Duration::from_secs(2)).unwrap();
        assert!(!request.contains(secret.as_str()), "got: {request}");
        assert!(request.contains("[nightjar:redacted]"), "got: {request}");
    }
}
