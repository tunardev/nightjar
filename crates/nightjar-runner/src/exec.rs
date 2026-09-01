use crate::notify::{Alert, Notifier};
use anyhow::{Context, Result};
use jiff::Timestamp;
use nightjar_config::Job;
use nightjar_config::secrets::SecretValue;
use nightjar_core::clock::Clock;
use nightjar_core::paths::Paths;
use nightjar_core::process::{own_process_group, signal_group};
use nightjar_store::Store;
use nightjar_store::run::{RunStatus, Trigger};
use std::fs::File;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::Mutex;
use std::sync::atomic::{AtomicI32, Ordering};
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::time::{Duration, Instant};

pub const DEFAULT_OUTPUT_CAP: u64 = 10 * 1024 * 1024;
const GRACE: Duration = Duration::from_secs(10);
const POLL: Duration = Duration::from_millis(50);

/// Long enough that a job scheduled every few minutes gets at most a
/// handful of alerts a day, not one per run. Short enough that a
/// still-broken job is re-announced within a business day.
pub(crate) const ALERT_COOLDOWN: Duration = Duration::from_secs(60 * 60);

/// `None` means no alert has gone out yet. This covers both a first alert
/// and a streak just cleared by success. A negative elapsed time is an NTP
/// correction, not a recent alert. It never suppresses.
pub fn cooldown_expired(last_notified: Option<Timestamp>, now: Timestamp) -> bool {
    match last_notified {
        None => true,
        Some(last) => {
            let elapsed_ms = now.as_millisecond() - last.as_millisecond();
            let cooldown_ms =
                i64::try_from(ALERT_COOLDOWN.as_millis()).expect("cooldown fits in i64 millis");
            elapsed_ms < 0 || elapsed_ms >= cooldown_ms
        }
    }
}

/// An atomic store is the only async-signal-safe thing `signal_handler` can
/// do. `wait_for_run`'s poll loop does the real work, off signal context.
static PENDING_SIGNAL: AtomicI32 = AtomicI32::new(0);

extern "C" fn signal_handler(sig: libc::c_int) {
    PENDING_SIGNAL.store(sig, Ordering::SeqCst);
}

/// Lets the wrapper record a terminal outcome instead of dying on default
/// disposition and leaving the `running` row stale. SIGKILL is not caught
/// here. `Daemon::reconcile` covers that case. Call only from
/// `cli::run::cmd_exec`. `libc::signal` is process-wide, and tests run
/// concurrent `execute()` calls that need the default SIGINT.
pub fn install_signal_handlers() {
    unsafe {
        libc::signal(libc::SIGINT, signal_handler as *const () as usize);
        libc::signal(libc::SIGTERM, signal_handler as *const () as usize);
        libc::signal(libc::SIGHUP, signal_handler as *const () as usize);
    }
}

// Both macOS libc and glibc give `signal()` BSD semantics. The handler
// stays installed and never needs re-arming after firing.

/// Terminates via `sig` at its default disposition, so a waiting parent's
/// `wait()` reports `WIFSIGNALED`. A plain `exit(128+sig)` would look like
/// an ordinary exit code instead. The daemon relies on this to reap and
/// classify each wrapper. No service manager sees this path. Units run
/// `nightjar daemon`, never `nightjar exec`.
pub fn reraise(sig: libc::c_int) -> ! {
    unsafe {
        libc::signal(sig, libc::SIG_DFL);
        libc::raise(sig);
    }
    unreachable!("default disposition for {sig} terminates the process");
}

/// Swap here, not load-then-store. A signal arriving between the two would
/// be lost.
fn take_pending_signal() -> Option<libc::c_int> {
    match PENDING_SIGNAL.swap(0, Ordering::SeqCst) {
        0 => None,
        sig => Some(sig),
    }
}

/// Bounds the post-exit pipe drain. An escaped descendant can hold the
/// write end open forever, but the outcome is already known by then.
const PUMP_DRAIN: Duration = Duration::from_secs(5);

use super::capture::pump;

/// What the run did. `status` is the authority, not `exit_code`. A run
/// killed at its timeout can still show `exit_code: Some(0)`, since the
/// job's shell may exit cleanly while a forked descendant keeps running.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Outcome {
    pub status: RunStatus,
    pub exit_code: Option<i32>,
    /// `Some` only when a signal to the wrapper's own process, not the job's,
    /// ended the run. Never set for a timeout: that is the wrapper's own
    /// kill decision, not something it caught. The caller must re-raise this
    /// so its wait status shows `WIFSIGNALED`, not a plain exit code.
    pub caught_signal: Option<libc::c_int>,
}

/// Output can hold whatever the job printed, redacted only for declared
/// secrets, so the capture file is readable by its owner alone.
fn create_private(path: &Path) -> Result<File> {
    use std::os::unix::fs::OpenOptionsExt;
    std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)
        .with_context(|| format!("creating {}", path.display()))
}

fn shell_for(job: &Job) -> String {
    job.shell
        .clone()
        .or_else(|| std::env::var("SHELL").ok())
        .unwrap_or_else(|| "/bin/sh".to_string())
}

fn login_shell_for(job: &Job) -> bool {
    job.login_shell
        .unwrap_or_else(|| nightjar_config::Config::default().login_shell)
}

/// `Child` doesn't reap its process on drop. An early return from `execute`
/// would otherwise leak a zombie and leave the job's group running.
struct ChildReaper<'a> {
    child: &'a mut Child,
    done: bool,
}

impl<'a> ChildReaper<'a> {
    fn new(child: &'a mut Child) -> Self {
        Self { child, done: false }
    }

    fn mark_done(&mut self) {
        self.done = true;
    }
}

impl Drop for ChildReaper<'_> {
    fn drop(&mut self) {
        if self.done {
            return;
        }
        // An empty group is a no-op (ESRCH).
        signal_group(self.child.id(), libc::SIGKILL);
        let _ = self.child.wait();
    }
}

/// A run ends once the child exits *and* its pipes close, unlike
/// `timeout(1)`: `sleep 15 & echo started` counts as a `timeout` here.
#[allow(clippy::too_many_arguments)]
pub fn execute(
    job: &Job,
    run_id: &str,
    trigger: Trigger,
    paths: &Paths,
    store: &Store,
    clock: &dyn Clock,
    output_cap: u64,
    notifier: &dyn Notifier,
    secrets_resolver: Option<&str>,
) -> Result<Outcome> {
    let (out_path, err_path) = paths.run_output(&job.name, run_id);
    if let Some(dir) = out_path.parent() {
        std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    }
    let out_file = create_private(&out_path)?;
    let err_file = create_private(&err_path)?;

    let shell = shell_for(job);
    let mut cmd = Command::new(&shell);
    if login_shell_for(job) {
        cmd.arg("-lc");
    } else {
        cmd.arg("-c");
    }
    cmd.arg(&job.command)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    if let Some(dir) = &job.workdir {
        cmd.current_dir(dir);
    }
    for (k, v) in &job.env {
        cmd.env(k, v);
    }

    own_process_group(&mut cmd);
    nightjar_core::process::apply_limits(&mut cmd, &job.limits);

    let started = clock.now();

    // Written before the spawn, so a dead wrapper reconciles to `unknown`
    // instead of vanishing, and an unstarted run stays visible.
    store.start_run(run_id, &job.name, trigger, started, &out_path, &err_path)?;

    // Resolved as late as possible, only here. Nothing upstream has seen a
    // secret. Nothing downstream can run with a failed secret left unset.
    let mut resolved = match nightjar_config::secrets::resolve(&job.secrets, secrets_resolver) {
        Ok(r) => r,
        Err(e) => {
            let e = e.context(format!("job {:?}: resolving secrets", job.name));
            // A store error here must not replace the real diagnostic
            // (which secret failed) with one that names nothing useful.
            let _ = store.finish_run(run_id, RunStatus::Failure, None, clock.now(), 0);
            let _ = store.set_run_message(run_id, &format!("{e:#}"));
            // No secret was ever resolved on this path, so there is nothing
            // for a notification channel to leak yet.
            dispatch_alert(job, RunStatus::Failure, None, store, clock, notifier, &[]);
            return Err(e);
        }
    };
    for (k, v) in &resolved.env {
        cmd.env(k, v.as_str());
    }

    let mut child = match cmd.spawn() {
        Ok(child) => child,
        Err(e) => {
            store.finish_run(run_id, RunStatus::Failure, None, clock.now(), 0)?;
            // The only place this reason survives: the wrapper's own stderr
            // is /dev/null when the daemon spawned it. An OS error names no
            // secret, so it is safe to show.
            let _ = store.set_run_message(run_id, &format!("cannot start {shell}: {e}"));
            dispatch_alert(
                job,
                RunStatus::Failure,
                None,
                store,
                clock,
                notifier,
                &resolved.redact,
            );
            return Err(e).with_context(|| format!("spawning {shell} for job {:?}", job.name));
        }
    };
    // `resolved.env` did its one job reaching the child's environment, so it
    // is cleared now. `resolved.redact` stays alive until this function
    // returns. It is the only record of which byte sequences in this run's
    // output are secret.
    resolved.env.clear();

    let mut reaper = ChildReaper::new(&mut child);
    // `Daemon::reconcile` checks whether the process owning this row is
    // alive. The wrapper outlives the job's own process, since descendants
    // can still hold the capture pipes open.
    if let Err(e) = store.set_run_pid(run_id, std::process::id()) {
        let _ = store.finish_run(run_id, RunStatus::Unknown, None, clock.now(), 0);
        return Err(e);
    }

    let stdout = reaper.child.stdout.take().expect("stdout was piped");
    let stderr = reaper.child.stderr.take().expect("stderr was piped");
    let mut pumps = Pumps::spawn(
        stdout,
        out_file,
        out_path.clone(),
        stderr,
        err_file,
        err_path.clone(),
        output_cap,
        &resolved.redact,
    );

    let (exit_code, status, finished_at, caught_signal) =
        match wait_for_run(reaper.child, job.timeout, &mut pumps, clock) {
            Ok(v) => v,
            Err(e) => {
                // Neither `success` nor `failure` is provable here. Leaving the row
                // `running` would hide the run from every downstream reader.
                let _ = store.finish_run(run_id, RunStatus::Unknown, None, clock.now(), 0);
                let _ = store.set_run_message(run_id, &format!("{e:#}"));
                return Err(e);
            }
        };
    reaper.mark_done();

    store.finish_run(run_id, status, exit_code, finished_at, pumps.bytes())?;
    dispatch_alert(
        job,
        status,
        exit_code,
        store,
        clock,
        notifier,
        &resolved.redact,
    );

    Ok(Outcome {
        status,
        exit_code,
        caught_signal,
    })
}

/// Runs only after the row above is already terminal. A store hiccup or a
/// dead notifier here costs only an alert, never the run's
/// already-recorded outcome. Notifications are best-effort.
fn dispatch_alert(
    job: &Job,
    status: RunStatus,
    exit_code: Option<i32>,
    store: &Store,
    clock: &dyn Clock,
    notifier: &dyn Notifier,
    redact: &[SecretValue],
) {
    let alert = match status {
        RunStatus::Failure => Alert::Failed {
            job: job.name.clone(),
            exit_code,
        },
        RunStatus::Timeout => Alert::TimedOut {
            job: job.name.clone(),
        },
        RunStatus::Limit => Alert::LimitExceeded {
            job: job.name.clone(),
        },
        RunStatus::Success => {
            let _ = store.clear_failure_count(&job.name);
            return;
        }
        RunStatus::Running | RunStatus::Unknown | RunStatus::Missed => return,
    };

    let now = clock.now();
    if store.record_failure_and_count(&job.name, now).is_err() {
        return;
    }

    let should_alert = match store.last_notified_at(&job.name) {
        Ok(last) => cooldown_expired(last, now),
        Err(_) => false,
    };
    if !should_alert {
        return;
    }

    // The row is already terminal, so nothing is lost by returning signals to
    // default here. Otherwise a service manager's `stop` could not reclaim
    // the wrapper while a slow channel runs. This is bracketed, not
    // reset-and-forget, so a caller's own disposition, like a test's
    // SIG_IGN, is restored exactly, not clobbered to SIG_DFL.
    with_default_signal_disposition(|| {
        crate::notify::send_and_stamp_cooldown(
            &alert,
            &job.on_failure,
            store,
            now,
            notifier,
            redact,
        );
    });
}

/// Runs `f` with SIGINT/SIGTERM/SIGHUP at default disposition, then
/// restores exactly what was there before. It never assumes
/// `install_signal_handlers`'s handler, since a plain test binary may not
/// have installed one. The lock makes nested save-then-restore pairs safe.
const RESET_SIGNALS: [libc::c_int; 3] = [libc::SIGINT, libc::SIGTERM, libc::SIGHUP];

/// Restores on unwind too. A notifier can panic, and leaving `SIG_DFL`
/// installed would strip the wrapper of the handlers it needs to record a
/// terminal outcome for the rest of its life.
struct RestoreDisposition([libc::sighandler_t; RESET_SIGNALS.len()]);

impl Drop for RestoreDisposition {
    fn drop(&mut self) {
        for (&sig, &handler) in RESET_SIGNALS.iter().zip(self.0.iter()) {
            unsafe { libc::signal(sig, handler) };
        }
    }
}

fn with_default_signal_disposition<T>(f: impl FnOnce() -> T) -> T {
    static LOCK: Mutex<()> = Mutex::new(());
    // A panic under this lock poisons it, so every later run dies on
    // `unwrap`. `RestoreDisposition` restores the disposition either way.
    let _guard = LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);

    let mut previous = [0 as libc::sighandler_t; RESET_SIGNALS.len()];
    for (slot, &sig) in previous.iter_mut().zip(RESET_SIGNALS.iter()) {
        *slot = unsafe { libc::signal(sig, libc::SIG_DFL) };
    }
    let _restore = RestoreDisposition(previous);

    f()
}

/// The two capture threads, polled without blocking so the wait loop can keep
/// enforcing the job's timeout while output is still arriving.
struct Pumps {
    out: Option<Receiver<u64>>,
    err: Option<Receiver<u64>>,
    out_path: PathBuf,
    err_path: PathBuf,
    bytes: u64,
}

impl Pumps {
    #[allow(clippy::too_many_arguments)]
    fn spawn<O, E>(
        stdout: O,
        out_file: File,
        out_path: PathBuf,
        stderr: E,
        err_file: File,
        err_path: PathBuf,
        cap: u64,
        redact: &[SecretValue],
    ) -> Self
    where
        O: Read + Send + 'static,
        E: Read + Send + 'static,
    {
        Self {
            // Each pump thread needs its own `Redactor`, so each gets its own
            // clone of the secrets to match against.
            out: Some(spawn_pump(stdout, out_file, cap, redact.to_vec())),
            err: Some(spawn_pump(stderr, err_file, cap, redact.to_vec())),
            out_path,
            err_path,
            bytes: 0,
        }
    }

    /// True once both streams hit EOF, meaning every writer dropped its end of the pipe.
    fn finished(&mut self) -> bool {
        let out = collect(&mut self.out, &mut self.bytes);
        let err = collect(&mut self.err, &mut self.bytes);
        out && err
    }

    /// For a stream that never reported, the capture file's length is a
    /// floor for its byte count, since a descendant may still be writing to
    /// it. A no-op for streams that already reported.
    fn finalize_with_fallback(&mut self) {
        if self.out.take().is_some() {
            self.bytes = self.bytes.saturating_add(file_len(&self.out_path));
        }
        if self.err.take().is_some() {
            self.bytes = self.bytes.saturating_add(file_len(&self.err_path));
        }
    }

    fn bytes(&self) -> u64 {
        self.bytes
    }
}

fn file_len(path: &Path) -> u64 {
    std::fs::metadata(path).map_or(0, |m| m.len())
}

fn spawn_pump<R: Read + Send + 'static>(
    src: R,
    dst: File,
    cap: u64,
    redact: Vec<SecretValue>,
) -> Receiver<u64> {
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(pump(src, dst, cap, &redact).unwrap_or(0));
    });
    rx
}

/// `Disconnected` counts as finished. A pump that panicked degrades that
/// stream's byte count to 0, rather than costing the run its row.
fn collect(slot: &mut Option<Receiver<u64>>, total: &mut u64) -> bool {
    let Some(rx) = slot.as_ref() else {
        return true;
    };
    match rx.try_recv() {
        Ok(n) => {
            *total = total.saturating_add(n);
            *slot = None;
            true
        }
        Err(TryRecvError::Disconnected) => {
            *slot = None;
            true
        }
        Err(TryRecvError::Empty) => false,
    }
}

/// The timeout covers the whole process group, not just the child. A
/// descendant holding a capture pipe keeps the run open until it closes
/// its pipe too. The returned timestamp is when the run ended, never when
/// the last writer let go, since a survivor can delay that forever.
///
/// Whether this exit is the kernel enforcing `RLIMIT_CPU`. This is the
/// only limit with a signal of its own. Other breaches surface as an
/// ordinary allocation, fork, or open failure, and record `failure`
/// instead (see the README). There are two shapes here, depending on
/// whether the shell is still in the picture. `sh -c "cmd"` execs `cmd` in
/// place, so the shell itself is signalled. A compound command leaves the
/// shell forked, so it reports its child's death as `128 + signo` instead.
fn cpu_limit_killed(status: ExitStatus) -> bool {
    use std::os::unix::process::ExitStatusExt;
    status.signal() == Some(libc::SIGXCPU) || status.code() == Some(128 + libc::SIGXCPU)
}

fn wait_for_run(
    child: &mut Child,
    timeout: Option<Duration>,
    pumps: &mut Pumps,
    clock: &dyn Clock,
) -> Result<(Option<i32>, RunStatus, Timestamp, Option<libc::c_int>)> {
    let deadline = match timeout {
        Some(t) => Some(
            Instant::now()
                .checked_add(t)
                .context("job timeout is too large to represent as a deadline")?,
        ),
        None => None,
    };

    let mut exit: Option<(Option<i32>, bool, Timestamp)> = None;
    let mut give_up: Option<Instant> = None;

    loop {
        if exit.is_none() {
            if let Some(exited) = child.try_wait()? {
                exit = Some((exited.code(), cpu_limit_killed(exited), clock.now()));
                give_up = Instant::now().checked_add(PUMP_DRAIN);
            }
        }
        let drained = pumps.finished();

        if let Some((code, hit_cpu_limit, exited_at)) = exit {
            if drained || give_up.is_some_and(|t| Instant::now() >= t) {
                pumps.finalize_with_fallback();
                let status = match (code, hit_cpu_limit) {
                    (_, true) => RunStatus::Limit,
                    (Some(0), _) => RunStatus::Success,
                    _ => RunStatus::Failure,
                };
                return Ok((code, status, exited_at, None));
            }
        }

        if deadline.is_some_and(|t| Instant::now() >= t) {
            return escalate(
                child,
                pumps,
                exit.map(|(code, _, _)| code),
                clock,
                libc::SIGTERM,
                RunStatus::Timeout,
                None,
            );
        }

        if let Some(sig) = take_pending_signal() {
            return escalate(
                child,
                pumps,
                exit.map(|(code, _, _)| code),
                clock,
                sig,
                RunStatus::Failure,
                Some(sig),
            );
        }

        std::thread::sleep(POLL);
    }
}

/// Signals the job's process group with `first_signal`, then SIGKILL after
/// `GRACE`. `already_exited` carries the child's code when the job's own
/// process was already reaped and only a descendant keeps the run alive.
///
/// The nesting isn't redundant. The outer `Option` asks whether we've
/// reaped yet. The inner is `ExitStatus::code()`'s "killed by a signal?".
#[allow(clippy::option_option)]
fn escalate(
    child: &mut Child,
    pumps: &mut Pumps,
    already_exited: Option<Option<i32>>,
    clock: &dyn Clock,
    first_signal: i32,
    status_on_stop: RunStatus,
    caught_signal: Option<libc::c_int>,
) -> Result<(Option<i32>, RunStatus, Timestamp, Option<libc::c_int>)> {
    let pid = child.id();
    let mut code = already_exited;
    signal_group(pid, first_signal);

    let hard_deadline = Instant::now()
        .checked_add(GRACE)
        .context("kill grace period is too large to represent as a deadline")?;
    loop {
        if code.is_none() {
            if let Some(exited) = child.try_wait()? {
                code = Some(exited.code());
            }
        }
        if code.is_some() && pumps.finished() {
            return Ok((code.flatten(), status_on_stop, clock.now(), caught_signal));
        }
        // A second stop signal means the caller has already waited once and
        // is not willing to wait out the rest of the grace period too.
        if Instant::now() >= hard_deadline || take_pending_signal().is_some() {
            break;
        }
        std::thread::sleep(POLL);
    }

    signal_group(pid, libc::SIGKILL);
    if code.is_none() {
        code = Some(child.wait()?.code());
    }
    let drain_deadline = Instant::now()
        .checked_add(PUMP_DRAIN)
        .context("pump drain window is too large to represent as a deadline")?;
    while !pumps.finished() && Instant::now() < drain_deadline && take_pending_signal().is_none() {
        std::thread::sleep(POLL);
    }
    pumps.finalize_with_fallback();
    Ok((code.flatten(), status_on_stop, clock.now(), caught_signal))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::notify::{NotifyOutcome, RecordingNotifier};
    use nightjar_config::OnFailure;
    use nightjar_core::clock::SystemClock;

    struct Fixture {
        _tmp: tempfile::TempDir,
        paths: Paths,
        store: Store,
    }

    fn fixture() -> Fixture {
        let tmp = tempfile::tempdir().unwrap();
        let paths = Paths::for_root(tmp.path());
        paths.ensure_dirs().unwrap();
        let store = Store::open(&paths.db_path).unwrap();
        Fixture {
            _tmp: tmp,
            paths,
            store,
        }
    }

    fn job(name: &str, command: &str, timeout: Option<Duration>) -> Job {
        Job {
            name: name.into(),
            command: command.into(),
            schedule: Some(nightjar_schedule::Schedule::parse("hourly").unwrap()),
            after: None,
            timeout,
            limits: nightjar_config::Limits::default(),
            catchup: nightjar_config::Catchup::Once,
            overlap: nightjar_config::Overlap::Skip,
            workdir: None,
            enabled: true,
            shell: None,
            login_shell: Some(false),
            env: std::collections::BTreeMap::new(),
            secrets: std::collections::BTreeMap::new(),
            on_failure: OnFailure::default(),
            warnings: Vec::new(),
        }
    }

    fn job_with(name: &str, command: &str, on_failure: OnFailure) -> Job {
        Job {
            on_failure,
            ..job(name, command, None)
        }
    }

    fn job_with_secrets(name: &str, command: &str, secrets: &[(&str, &str)]) -> Job {
        Job {
            secrets: secrets
                .iter()
                .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
                .collect(),
            ..job(name, command, None)
        }
    }

    fn execute_with_notifier(
        job: &Job,
        run_id: &str,
        f: &Fixture,
        notifier: &dyn Notifier,
    ) -> Result<RunStatus> {
        execute(
            job,
            run_id,
            Trigger::Manual,
            &f.paths,
            &f.store,
            &SystemClock,
            DEFAULT_OUTPUT_CAP,
            notifier,
            None,
        )
        .map(|o| o.status)
    }

    fn read(p: &std::path::Path) -> String {
        std::fs::read_to_string(p).unwrap_or_default()
    }

    #[test]
    fn job_records_success_and_captures_stdout_when_it_succeeds() {
        let f = fixture();
        let j = job("ok", "echo hello", None);

        let outcome = execute(
            &j,
            "run1",
            Trigger::Manual,
            &f.paths,
            &f.store,
            &SystemClock,
            DEFAULT_OUTPUT_CAP,
            &RecordingNotifier::default(),
            None,
        )
        .unwrap();
        assert_eq!(outcome.status, RunStatus::Success);

        let run = f.store.last_run("ok").unwrap().unwrap();
        assert_eq!(run.status, RunStatus::Success);
        assert_eq!(run.exit_code, Some(0));
        assert!(run.duration_ms.unwrap() >= 0);
        assert_eq!(read(&run.stdout_path.unwrap()).trim(), "hello");
    }

    #[test]
    fn capture_files_are_readable_by_the_owner_alone() {
        use std::os::unix::fs::PermissionsExt;
        let f = fixture();
        let j = job("private", "echo out; echo err 1>&2", None);
        execute_with_notifier(&j, "r1", &f, &RecordingNotifier::default()).unwrap();

        let run = f.store.last_run("private").unwrap().unwrap();
        for path in [run.stdout_path.unwrap(), run.stderr_path.unwrap()] {
            let mode = std::fs::metadata(&path).unwrap().permissions().mode();
            assert_eq!(
                mode & 0o077,
                0,
                "{} is readable by group or others: {:o}",
                path.display(),
                mode & 0o777
            );
        }
    }

    #[test]
    fn job_records_failure_with_the_code_when_exit_is_nonzero() {
        let f = fixture();
        let j = job("bad", "exit 3", None);

        let outcome = execute(
            &j,
            "run2",
            Trigger::Manual,
            &f.paths,
            &f.store,
            &SystemClock,
            DEFAULT_OUTPUT_CAP,
            &RecordingNotifier::default(),
            None,
        )
        .unwrap();
        assert_eq!(outcome.status, RunStatus::Failure);
        assert_eq!(outcome.exit_code, Some(3));

        let run = f.store.last_run("bad").unwrap().unwrap();
        assert_eq!(run.exit_code, Some(3));
    }

    #[test]
    fn stderr_is_captured_separately_from_stdout() {
        let f = fixture();
        let j = job("noisy", "echo out; echo err 1>&2", None);

        execute(
            &j,
            "run3",
            Trigger::Manual,
            &f.paths,
            &f.store,
            &SystemClock,
            DEFAULT_OUTPUT_CAP,
            &RecordingNotifier::default(),
            None,
        )
        .unwrap();

        let run = f.store.last_run("noisy").unwrap().unwrap();
        assert_eq!(read(&run.stdout_path.unwrap()).trim(), "out");
        assert_eq!(read(&run.stderr_path.unwrap()).trim(), "err");
    }

    #[test]
    fn job_is_killed_and_recorded_as_timeout_when_it_exceeds_its_timeout() {
        let f = fixture();
        let j = job(
            "hang",
            "echo started; sleep 30; true",
            Some(Duration::from_millis(300)),
        );

        let started = Instant::now();
        let outcome = execute(
            &j,
            "run4",
            Trigger::Manual,
            &f.paths,
            &f.store,
            &SystemClock,
            DEFAULT_OUTPUT_CAP,
            &RecordingNotifier::default(),
            None,
        )
        .unwrap();

        assert_eq!(outcome.status, RunStatus::Timeout);
        assert_eq!(
            outcome.caught_signal, None,
            "no signal for cmd_exec to re-raise"
        );
        assert!(
            started.elapsed() < Duration::from_secs(11),
            "must not wait out the sleep; took {:?}",
            started.elapsed()
        );

        let run = f.store.last_run("hang").unwrap().unwrap();
        assert_eq!(run.status, RunStatus::Timeout);

        assert_eq!(
            read(&run.stdout_path.unwrap()).trim(),
            "started",
            "partial output must survive the kill"
        );
    }

    #[test]
    fn timeout_kills_descendants_the_shell_forked() {
        let f = fixture();
        let j = job("hang-child", "sleep 60; true", Some(Duration::from_secs(1)));

        let started = Instant::now();
        let outcome = execute(
            &j,
            "run8",
            Trigger::Manual,
            &f.paths,
            &f.store,
            &SystemClock,
            DEFAULT_OUTPUT_CAP,
            &RecordingNotifier::default(),
            None,
        )
        .unwrap();

        assert_eq!(outcome.status, RunStatus::Timeout);
        assert!(
            started.elapsed() < Duration::from_secs(11),
            "the forked sleep must be killed with the shell; took {:?}",
            started.elapsed()
        );
        assert_eq!(
            f.store.last_run("hang-child").unwrap().unwrap().status,
            RunStatus::Timeout
        );
    }

    #[test]
    fn run_is_recorded_as_timeout_not_success_when_a_background_descendant_survives() {
        let f = fixture();
        let j = job(
            "hang-bg",
            "sleep 60 & echo started",
            Some(Duration::from_secs(1)),
        );

        let started = Instant::now();
        let outcome = execute(
            &j,
            "run9",
            Trigger::Manual,
            &f.paths,
            &f.store,
            &SystemClock,
            DEFAULT_OUTPUT_CAP,
            &RecordingNotifier::default(),
            None,
        )
        .unwrap();

        assert!(
            started.elapsed() < Duration::from_secs(20),
            "must not wait out the background sleep; took {:?}",
            started.elapsed()
        );
        assert_eq!(outcome.status, RunStatus::Timeout);

        let run = f.store.last_run("hang-bg").unwrap().unwrap();
        assert_eq!(run.status, RunStatus::Timeout);
        assert_ne!(run.status, RunStatus::Success);
        assert!(
            run.duration_ms.unwrap() < 11_000,
            "recorded duration was {:?}ms",
            run.duration_ms
        );
    }

    #[test]
    fn running_row_exists_before_the_process_finishes() {
        let f = fixture();
        let j = job("slow", "sleep 0.4; echo done", None);

        let db = f.paths.db_path.clone();
        let probe = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(150));
            let s = Store::open(&db).unwrap();
            s.last_run("slow").unwrap().map(|r| r.status)
        });

        execute(
            &j,
            "run5",
            Trigger::Manual,
            &f.paths,
            &f.store,
            &SystemClock,
            DEFAULT_OUTPUT_CAP,
            &RecordingNotifier::default(),
            None,
        )
        .unwrap();

        assert_eq!(probe.join().unwrap(), Some(RunStatus::Running));
    }

    #[test]
    fn run_is_still_recorded_when_it_cannot_spawn() {
        let f = fixture();
        let mut j = job("nospawn", "true", None);
        j.shell = Some("/nonexistent/nightjar-test-shell".into());

        let err = execute(
            &j,
            "run10",
            Trigger::Manual,
            &f.paths,
            &f.store,
            &SystemClock,
            DEFAULT_OUTPUT_CAP,
            &RecordingNotifier::default(),
            None,
        )
        .unwrap_err();
        assert!(
            format!("{err:#}").contains("nightjar-test-shell"),
            "error was: {err:#}"
        );

        let run = f.store.last_run("nospawn").unwrap().unwrap();
        assert_eq!(run.status, RunStatus::Failure);
        assert_eq!(run.pid, None, "no process ever existed to have a pid");
        let message = run.message.expect("the row must say why it failed");
        assert!(
            message.contains("nightjar-test-shell"),
            "the reason must name what could not start: {message}"
        );
    }

    #[test]
    fn run_is_recorded_as_unknown_not_left_running_when_a_wait_failure_happens_after_spawn() {
        let f = fixture();
        let j = job("overflow", "true", Some(Duration::MAX));

        let err = execute(
            &j,
            "run11",
            Trigger::Manual,
            &f.paths,
            &f.store,
            &SystemClock,
            DEFAULT_OUTPUT_CAP,
            &RecordingNotifier::default(),
            None,
        )
        .unwrap_err();
        assert!(
            format!("{err:#}").contains("too large"),
            "error was: {err:#}"
        );

        let run = f.store.last_run("overflow").unwrap().unwrap();
        assert_eq!(run.status, RunStatus::Unknown);
        assert_eq!(
            run.pid,
            Some(std::process::id()),
            "pid is the wrapper's, not the job's"
        );
        assert!(
            run.finished_at.is_some(),
            "unknown is still a finished state"
        );
        assert!(
            run.message
                .as_deref()
                .unwrap_or_default()
                .contains("too large"),
            "an unknown outcome must carry its reason: {:?}",
            run.message
        );
    }

    #[test]
    fn env_and_workdir_are_applied_to_the_child() {
        let f = fixture();
        let dir = tempfile::tempdir().unwrap();
        let mut j = job("envtest", "echo $NIGHTJAR_TEST_VAR; pwd", None);
        j.env.insert("NIGHTJAR_TEST_VAR".into(), "applied".into());
        j.workdir = Some(dir.path().to_path_buf());

        execute(
            &j,
            "run6",
            Trigger::Manual,
            &f.paths,
            &f.store,
            &SystemClock,
            DEFAULT_OUTPUT_CAP,
            &RecordingNotifier::default(),
            None,
        )
        .unwrap();

        let run = f.store.last_run("envtest").unwrap().unwrap();
        let out = read(&run.stdout_path.unwrap());
        assert!(out.contains("applied"), "stdout was: {out}");
    }

    #[test]
    fn output_is_truncated_but_true_size_is_reported_when_it_is_beyond_the_cap() {
        let f = fixture();
        let j = job("flood", "head -c 50000 /dev/zero | tr '\\0' 'x'", None);

        execute(
            &j,
            "run7",
            Trigger::Manual,
            &f.paths,
            &f.store,
            &SystemClock,
            1000,
            &RecordingNotifier::default(),
            None,
        )
        .unwrap();

        let run = f.store.last_run("flood").unwrap().unwrap();
        assert!(
            run.output_bytes >= 50000,
            "true size was {}",
            run.output_bytes
        );

        let written = std::fs::metadata(run.stdout_path.unwrap()).unwrap().len();
        assert!(written < 2000, "file should be capped, was {written}");
    }

    #[test]
    fn output_bytes_falls_back_to_the_file_size_when_a_pump_is_abandoned() {
        let f = fixture();
        let j = job("abandoned", "echo hello-before-the-hold; sleep 30 &", None);
        execute(
            &j,
            "r1",
            Trigger::Manual,
            &f.paths,
            &f.store,
            &SystemClock,
            DEFAULT_OUTPUT_CAP,
            &RecordingNotifier::default(),
            None,
        )
        .unwrap();

        let run = f.store.last_run("abandoned").unwrap().unwrap();
        let on_disk = std::fs::metadata(run.stdout_path.clone().unwrap())
            .unwrap()
            .len();
        assert!(on_disk > 0, "precondition: the file has content");
        assert!(
            run.output_bytes >= on_disk,
            "recorded {} but {} bytes are on disk",
            run.output_bytes,
            on_disk
        );
    }

    #[test]
    fn output_bytes_falls_back_on_both_streams_independently_when_both_are_abandoned() {
        let f = fixture();
        let j = job(
            "abandoned-both",
            "echo out-before-hold; echo err-before-hold 1>&2; sleep 30 &",
            None,
        );
        execute(
            &j,
            "r1",
            Trigger::Manual,
            &f.paths,
            &f.store,
            &SystemClock,
            DEFAULT_OUTPUT_CAP,
            &RecordingNotifier::default(),
            None,
        )
        .unwrap();

        let run = f.store.last_run("abandoned-both").unwrap().unwrap();
        let out_len = std::fs::metadata(run.stdout_path.clone().unwrap())
            .unwrap()
            .len();
        let err_len = std::fs::metadata(run.stderr_path.clone().unwrap())
            .unwrap()
            .len();
        assert!(
            out_len > 0 && err_len > 0,
            "precondition: both files have content"
        );
        assert!(
            run.output_bytes >= out_len + err_len,
            "recorded {} but stdout+stderr on disk total {}",
            run.output_bytes,
            out_len + err_len
        );
    }

    #[test]
    fn run_does_not_use_the_fallback_and_reports_the_true_size_when_it_completes_normally() {
        let f = fixture();
        let j = job("finishes-clean", "echo hello", None);
        execute(
            &j,
            "r1",
            Trigger::Manual,
            &f.paths,
            &f.store,
            &SystemClock,
            DEFAULT_OUTPUT_CAP,
            &RecordingNotifier::default(),
            None,
        )
        .unwrap();

        let run = f.store.last_run("finishes-clean").unwrap().unwrap();
        assert_eq!(run.output_bytes, 6, "\"hello\\n\" is 6 bytes");
    }

    #[test]
    fn failing_job_alerts_once_and_a_succeeding_one_does_not() {
        let f = fixture();
        let n = RecordingNotifier::default();

        let bad = job_with(
            "boom",
            "exit 3",
            OnFailure {
                notify: true,
                run: None,
                webhook: None,
            },
        );
        execute_with_notifier(&bad, "r1", &f, &n).unwrap();
        let calls = n.calls();
        assert_eq!(calls.len(), 1, "a failure must alert");
        assert_eq!(
            calls[0].alert,
            Alert::Failed {
                job: "boom".into(),
                exit_code: Some(3),
            },
            "must be the failing job's own alert, not merely any alert"
        );

        let good = job_with(
            "fine",
            "true",
            OnFailure {
                notify: true,
                run: None,
                webhook: None,
            },
        );
        execute_with_notifier(&good, "r2", &f, &n).unwrap();
        assert_eq!(
            n.send_count(),
            1,
            "a success must not even attempt to notify, not just fail to configure a channel"
        );
    }

    #[test]
    fn job_is_killed_and_recorded_as_limit_when_it_exceeds_the_cpu_limit() {
        let f = fixture();
        let mut j = job(
            "burner",
            "while :; do :; done",
            Some(Duration::from_secs(20)),
        );
        j.limits.cpu_time = Some(1);
        j.shell = Some("/bin/sh".to_string());

        let started = Instant::now();
        let status = execute_with_notifier(&j, "r1", &f, &RecordingNotifier::default()).unwrap();

        assert_eq!(
            status,
            RunStatus::Limit,
            "a limit breach must be distinguishable from an ordinary failure"
        );
        assert!(
            started.elapsed() < Duration::from_secs(30),
            "the kernel must have killed it at its CPU ceiling, not run it out; took {:?}",
            started.elapsed()
        );
        let row = f.store.get_run("r1").unwrap().unwrap();
        assert_eq!(row.status, RunStatus::Limit, "and the row must say so too");
    }

    #[test]
    fn job_runs_normally_when_it_is_within_its_limits() {
        let f = fixture();
        let mut j = job("easy", "true", None);
        j.limits.cpu_time = Some(60);
        j.limits.files = Some(256);

        let status = execute_with_notifier(&j, "r1", &f, &RecordingNotifier::default()).unwrap();

        assert_eq!(
            status,
            RunStatus::Success,
            "control for the breach test above"
        );
    }

    #[test]
    fn limits_bind_the_child_and_never_the_daemon() {
        let before = unsafe {
            let mut rl = libc::rlimit {
                rlim_cur: 0,
                rlim_max: 0,
            };
            assert_eq!(libc::getrlimit(libc::RLIMIT_NOFILE, &raw mut rl), 0);
            rl.rlim_cur
        };

        let f = fixture();
        let mut j = job("capped", "true", None);
        j.limits.files = Some(64);
        j.limits.cpu_time = Some(60);
        execute_with_notifier(&j, "r1", &f, &RecordingNotifier::default()).unwrap();

        let after = unsafe {
            let mut rl = libc::rlimit {
                rlim_cur: 0,
                rlim_max: 0,
            };
            assert_eq!(libc::getrlimit(libc::RLIMIT_NOFILE, &raw mut rl), 0);
            rl.rlim_cur
        };
        assert_eq!(
            before, after,
            "running a limited job must leave this process's own limits untouched"
        );
    }

    #[test]
    fn job_still_runs_when_the_requested_limit_exceeds_the_inherited_hard_ceiling() {
        let f = fixture();
        let mut j = job("greedy", "true", None);
        j.limits.processes = Some(u64::MAX);
        j.limits.files = Some(u64::MAX);

        let status = execute_with_notifier(&j, "r1", &f, &RecordingNotifier::default()).unwrap();

        assert_eq!(status, RunStatus::Success);
    }

    #[test]
    fn run_outcome_never_changes_when_a_notification_fails() {
        let f = fixture();
        let n = RecordingNotifier::failing_on("command");
        let j = job_with(
            "ok",
            "exit 5",
            OnFailure {
                notify: false,
                run: Some("unused-by-a-recording-notifier".into()),
                webhook: None,
            },
        );

        let status = execute_with_notifier(&j, "r1", &f, &n).unwrap();
        assert_eq!(
            status,
            RunStatus::Failure,
            "a broken notifier must not change what execute() itself reports"
        );

        let run = f.store.last_run("ok").unwrap().unwrap();
        assert_eq!(run.status, RunStatus::Failure);
        assert_eq!(run.exit_code, Some(5));

        let calls = n.calls();
        assert_eq!(
            calls.len(),
            1,
            "the failing channel must still have been attempted"
        );
        assert_eq!(
            calls[0].alert,
            Alert::Failed {
                job: "ok".into(),
                exit_code: Some(5),
            }
        );
    }

    #[test]
    fn repeated_failures_are_rate_limited_rather_than_storming() {
        let f = fixture();
        let n = RecordingNotifier::default();
        let j = job_with(
            "flappy",
            "exit 1",
            OnFailure {
                notify: true,
                run: None,
                webhook: None,
            },
        );

        for i in 0..5 {
            execute_with_notifier(&j, &format!("r{i}"), &f, &n).unwrap();
        }
        let calls = n.calls();
        assert!(
            calls.len() < 5,
            "five failures in a row must not send five alerts, sent {}",
            calls.len()
        );
        assert!(!calls.is_empty(), "but the first one must alert");
        assert_eq!(
            calls[0].alert,
            Alert::Failed {
                job: "flappy".into(),
                exit_code: Some(1),
            },
            "the one alert that does fire must be for this job's real failure"
        );
    }

    #[test]
    fn consecutive_failures_counts_up_and_resets_when_a_run_succeeds() {
        let f = fixture();
        let n = RecordingNotifier::default();
        let bad = job_with("j", "exit 1", OnFailure::default());
        execute_with_notifier(&bad, "a", &f, &n).unwrap();
        execute_with_notifier(&bad, "b", &f, &n).unwrap();
        let state = f.store.job_state("j").unwrap().unwrap();
        assert_eq!(state.consecutive_failures, 2);
        assert_eq!(
            state.last_notified_at, None,
            "cooldown not stamped without a notification"
        );

        let good = job_with("j", "true", OnFailure::default());
        execute_with_notifier(&good, "c", &f, &n).unwrap();
        assert_eq!(
            f.store
                .job_state("j")
                .unwrap()
                .unwrap()
                .consecutive_failures,
            0
        );
    }

    #[test]
    fn job_alerts_as_timed_out_not_failed_when_it_times_out() {
        let f = fixture();
        let n = RecordingNotifier::default();
        let mut j = job(
            "hang-notify",
            "sleep 30; true",
            Some(Duration::from_millis(300)),
        );
        j.on_failure = OnFailure {
            notify: true,
            run: None,
            webhook: None,
        };

        let status = execute_with_notifier(&j, "r1", &f, &n).unwrap();
        assert_eq!(status, RunStatus::Timeout);

        let calls = n.calls();
        assert_eq!(calls.len(), 1);
        assert_eq!(
            calls[0].alert,
            Alert::TimedOut {
                job: "hang-notify".into()
            },
            "a timeout must alert as TimedOut, not be folded into Failed"
        );
    }

    struct OrderProbe {
        probe_store: Mutex<Store>,
        run_id: String,
        observed_status: Mutex<Option<RunStatus>>,
    }

    impl Notifier for OrderProbe {
        fn send(
            &self,
            _alert: &Alert,
            _on_failure: &OnFailure,
            _redact: &[SecretValue],
        ) -> Vec<NotifyOutcome> {
            let store = self.probe_store.lock().unwrap();
            let status = store.get_run(&self.run_id).unwrap().unwrap().status;
            *self.observed_status.lock().unwrap() = Some(status);
            Vec::new()
        }
    }

    #[test]
    fn alert_dispatch_observes_the_run_row_already_terminal() {
        let f = fixture();
        let probe = OrderProbe {
            probe_store: Mutex::new(Store::open(&f.paths.db_path).unwrap()),
            run_id: "r1".to_string(),
            observed_status: Mutex::new(None),
        };
        let j = job_with(
            "boom",
            "exit 1",
            OnFailure {
                notify: true,
                run: None,
                webhook: None,
            },
        );

        execute(
            &j,
            "r1",
            Trigger::Manual,
            &f.paths,
            &f.store,
            &SystemClock,
            DEFAULT_OUTPUT_CAP,
            &probe,
            None,
        )
        .unwrap();

        assert_eq!(
            *probe.observed_status.lock().unwrap(),
            Some(RunStatus::Failure),
            "alerts must never race the row reaching its terminal state"
        );
    }

    #[test]
    fn job_still_alerts_when_it_cannot_spawn() {
        let f = fixture();
        let n = RecordingNotifier::default();
        let mut j = job_with(
            "nospawn",
            "true",
            OnFailure {
                notify: true,
                run: None,
                webhook: None,
            },
        );
        j.shell = Some("/nonexistent/nightjar-test-shell".into());

        execute_with_notifier(&j, "r1", &f, &n).unwrap_err();

        let calls = n.calls();
        assert_eq!(
            calls.len(),
            1,
            "a job that never even started must still alert, not fail in total silence"
        );
        assert_eq!(
            calls[0].alert,
            Alert::Failed {
                job: "nospawn".into(),
                exit_code: None,
            }
        );
    }

    #[test]
    fn next_alert_is_not_suppressed_when_last_notified_at_is_in_the_future() {
        let f = fixture();
        let n = RecordingNotifier::default();
        let future: Timestamp = "2099-01-01T00:00:00Z".parse().unwrap();
        f.store.set_last_notified_at("skewed", future).unwrap();

        let j = job_with(
            "skewed",
            "exit 1",
            OnFailure {
                notify: true,
                run: None,
                webhook: None,
            },
        );
        execute_with_notifier(&j, "r1", &f, &n).unwrap();

        assert_eq!(
            n.send_count(),
            1,
            "future cooldown must not suppress alerts"
        );
    }

    #[test]
    fn cooldown_is_not_stamped_when_every_configured_channel_fails() {
        let f = fixture();
        let n = RecordingNotifier::failing_on("desktop");
        let j = job_with(
            "flaky-channel",
            "exit 1",
            OnFailure {
                notify: true,
                run: None,
                webhook: None,
            },
        );

        execute_with_notifier(&j, "r1", &f, &n).unwrap();
        assert_eq!(
            f.store
                .job_state("flaky-channel")
                .unwrap()
                .unwrap()
                .last_notified_at,
            None,
            "a send where every channel errored must not burn the cooldown"
        );

        execute_with_notifier(&j, "r2", &f, &n).unwrap();
        assert_eq!(
            n.calls().len(),
            2,
            "since nothing got through the first time, the second failure must attempt again"
        );
    }

    fn contains_bytes(haystack: &[u8], needle: &[u8]) -> bool {
        haystack.windows(needle.len()).any(|w| w == needle)
    }

    fn files_under(root: &std::path::Path) -> Vec<PathBuf> {
        let mut out = Vec::new();
        let mut stack = vec![root.to_path_buf()];
        while let Some(dir) = stack.pop() {
            let Ok(entries) = std::fs::read_dir(&dir) else {
                continue;
            };
            for entry in entries.filter_map(std::result::Result::ok) {
                let path = entry.path();
                if path.is_dir() {
                    stack.push(path);
                } else {
                    out.push(path);
                }
            }
        }
        out
    }

    fn all_nightjar_files(paths: &Paths) -> Vec<PathBuf> {
        let mut out = Vec::new();
        for dir in [
            &paths.config_dir,
            &paths.jobs_dir,
            &paths.data_dir,
            &paths.runs_dir,
        ] {
            out.extend(files_under(dir));
        }
        for file in [&paths.db_path, &paths.lock_path] {
            if file.exists() {
                out.push(file.clone());
            }
        }
        out
    }

    #[test]
    fn resolved_secret_reaches_the_child_environment() {
        let f = fixture();
        let mut j = job_with_secrets(
            "envsecret",
            "[ \"$PGPASSWORD\" = \"$EXPECTED\" ] && echo matched",
            &[("PGPASSWORD", "s3cr3t-value-9f2")],
        );
        j.env.insert("EXPECTED".into(), "s3cr3t-value-9f2".into());

        let outcome = execute(
            &j,
            "run1",
            Trigger::Manual,
            &f.paths,
            &f.store,
            &SystemClock,
            DEFAULT_OUTPUT_CAP,
            &RecordingNotifier::default(),
            Some("echo {}"),
        )
        .unwrap();
        assert_eq!(outcome.status, RunStatus::Success);

        let run = f.store.last_run("envsecret").unwrap().unwrap();
        assert_eq!(read(&run.stdout_path.unwrap()).trim(), "matched");
    }

    #[test]
    fn secret_never_appears_in_the_child_argv() {
        let f = fixture();
        let secret = "argvcanary-8f3d1a70";
        let mut j = job_with_secrets(
            "argvtest",
            "sleep 1; [ \"$MYSECRET\" = \"$EXPECTED\" ] && echo matched",
            &[("MYSECRET", secret)],
        );
        j.env.insert("EXPECTED".into(), secret.into());

        let probe = std::thread::spawn({
            let secret = secret.to_string();
            move || {
                let deadline = Instant::now() + Duration::from_millis(900);
                let mut leaked = false;
                while Instant::now() < deadline {
                    if let Ok(out) = std::process::Command::new("ps")
                        .args(["-axww", "-o", "command="])
                        .output()
                    {
                        if String::from_utf8_lossy(&out.stdout).contains(&secret) {
                            leaked = true;
                            break;
                        }
                    }
                    std::thread::sleep(Duration::from_millis(50));
                }
                leaked
            }
        });

        let outcome = execute(
            &j,
            "run2",
            Trigger::Manual,
            &f.paths,
            &f.store,
            &SystemClock,
            DEFAULT_OUTPUT_CAP,
            &RecordingNotifier::default(),
            Some("echo {}"),
        )
        .unwrap();

        assert!(
            !probe.join().unwrap(),
            "the secret must never appear in any process's argv"
        );
        assert_eq!(outcome.status, RunStatus::Success);
        let run = f.store.last_run("argvtest").unwrap().unwrap();
        assert_eq!(
            read(&run.stdout_path.unwrap()).trim(),
            "matched",
            "the secret must still have genuinely reached the child, via its environment"
        );
    }

    #[test]
    fn secret_never_reaches_sqlite() {
        let f = fixture();
        let marker_dir = tempfile::tempdir().unwrap();
        let marker = marker_dir.path().join("resolver-ran");
        let secret = "sqlitecanary-4b19e0c2";
        let resolver = format!("echo ran >> {} && echo {secret} #{{}}", marker.display());

        let j = job_with_secrets(
            "sqlitetest",
            "true",
            &[("DBPASS", "op://vault/db/password")],
        );
        let outcome = execute(
            &j,
            "run3",
            Trigger::Manual,
            &f.paths,
            &f.store,
            &SystemClock,
            DEFAULT_OUTPUT_CAP,
            &RecordingNotifier::default(),
            Some(&resolver),
        )
        .unwrap();
        assert_eq!(outcome.status, RunStatus::Success);
        assert!(marker.exists(), "the resolver must have actually run");

        for extra in ["", "-wal", "-shm"] {
            let path = PathBuf::from(format!("{}{extra}", f.paths.db_path.display()));
            let Ok(bytes) = std::fs::read(&path) else {
                continue;
            };
            assert!(
                !contains_bytes(&bytes, secret.as_bytes()),
                "secret leaked into {}",
                path.display()
            );
        }
    }

    #[test]
    fn secret_never_reaches_a_job_file_or_any_nightjar_file() {
        let f = fixture();
        let marker_dir = tempfile::tempdir().unwrap();
        let marker = marker_dir.path().join("resolver-ran");
        let secret = "filecanary-77aa22";
        let resolver = format!("echo ran >> {} && echo {secret} #{{}}", marker.display());

        std::fs::write(
            f.paths.jobs_dir.join("filejob.toml"),
            "command = \"true\"\nschedule = \"hourly\"\n\n[secrets]\n\
             MYSECRET = \"op://vault/db/password\"\n",
        )
        .unwrap();
        let j = Job::load(&f.paths.jobs_dir.join("filejob.toml")).unwrap();

        let outcome = execute(
            &j,
            "run4",
            Trigger::Manual,
            &f.paths,
            &f.store,
            &SystemClock,
            DEFAULT_OUTPUT_CAP,
            &RecordingNotifier::default(),
            Some(&resolver),
        )
        .unwrap();
        assert_eq!(outcome.status, RunStatus::Success);
        assert!(marker.exists(), "the resolver must have actually run");

        for path in all_nightjar_files(&f.paths) {
            let bytes = std::fs::read(&path).unwrap();
            assert!(
                !contains_bytes(&bytes, secret.as_bytes()),
                "secret leaked into {}",
                path.display()
            );
        }
    }

    #[test]
    fn resolution_stops_the_job_and_names_the_variable_when_it_fails() {
        let f = fixture();
        let marker_dir = tempfile::tempdir().unwrap();
        let marker = marker_dir.path().join("job-ran");
        let j = job_with_secrets(
            "failtest",
            &format!("touch {}", marker.display()),
            &[("PGPASSWORD", "op://vault/db/password")],
        );

        let err = execute(
            &j,
            "run5",
            Trigger::Manual,
            &f.paths,
            &f.store,
            &SystemClock,
            DEFAULT_OUTPUT_CAP,
            &RecordingNotifier::default(),
            Some("exit 9 #{}"),
        )
        .unwrap_err();
        assert!(
            format!("{err:#}").contains("PGPASSWORD"),
            "message was: {err:#}"
        );
        assert!(!marker.exists(), "job command must never have run");

        let run = f.store.last_run("failtest").unwrap().unwrap();
        assert_eq!(run.status, RunStatus::Failure);
        assert_eq!(
            run.exit_code, None,
            "the job never spawned, so it has no exit code of its own"
        );
        assert!(
            run.message
                .as_deref()
                .unwrap_or_default()
                .contains("PGPASSWORD"),
            "message was: {:?}",
            run.message
        );
    }

    #[test]
    fn resolver_stderr_is_never_logged_verbatim() {
        let f = fixture();
        let j = job_with_secrets(
            "stderrtest",
            "true",
            &[("PGPASSWORD", "op://vault/db/password")],
        );

        let err = execute(
            &j,
            "run6",
            Trigger::Manual,
            &f.paths,
            &f.store,
            &SystemClock,
            DEFAULT_OUTPUT_CAP,
            &RecordingNotifier::default(),
            Some("echo the-secret-itself-was-here >&2; exit 3 #{}"),
        )
        .unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            !msg.contains("the-secret-itself-was-here"),
            "resolver stderr leaked into the returned error: {msg}"
        );

        let run = f.store.last_run("stderrtest").unwrap().unwrap();
        let stored = run.message.unwrap_or_default();
        assert!(
            !stored.contains("the-secret-itself-was-here"),
            "resolver stderr leaked into the stored message: {stored}"
        );
        assert!(stored.contains("PGPASSWORD"), "message was: {stored}");
    }

    #[test]
    fn secret_is_redacted_on_disk_when_echoed_to_stdout() {
        let f = fixture();
        let j = job_with_secrets(
            "echostdout",
            "echo \"token=$TOKEN\"",
            &[("TOKEN", "op://vault/x")],
        );

        execute(
            &j,
            "run1",
            Trigger::Manual,
            &f.paths,
            &f.store,
            &SystemClock,
            DEFAULT_OUTPUT_CAP,
            &RecordingNotifier::default(),
            Some("echo s3cr3t-stdout-9f2 #{}"),
        )
        .unwrap();

        let run = f.store.last_run("echostdout").unwrap().unwrap();
        let bytes = std::fs::read(run.stdout_path.unwrap()).unwrap();
        let text = String::from_utf8_lossy(&bytes);
        assert!(!text.contains("s3cr3t-stdout-9f2"), "got: {text}");
        assert!(text.contains("[nightjar:redacted]"), "got: {text}");
    }

    #[test]
    fn secret_is_redacted_on_disk_when_echoed_to_stderr() {
        let f = fixture();
        let j = job_with_secrets(
            "echostderr",
            "echo \"token=$TOKEN\" 1>&2",
            &[("TOKEN", "op://vault/x")],
        );

        execute(
            &j,
            "run1",
            Trigger::Manual,
            &f.paths,
            &f.store,
            &SystemClock,
            DEFAULT_OUTPUT_CAP,
            &RecordingNotifier::default(),
            Some("echo s3cr3t-stderr-4a1 #{}"),
        )
        .unwrap();

        let run = f.store.last_run("echostderr").unwrap().unwrap();
        let bytes = std::fs::read(run.stderr_path.unwrap()).unwrap();
        let text = String::from_utf8_lossy(&bytes);
        assert!(!text.contains("s3cr3t-stderr-4a1"), "got: {text}");
        assert!(text.contains("[nightjar:redacted]"), "got: {text}");
    }

    struct RedactCapture {
        seen: Mutex<Vec<Vec<String>>>,
    }

    impl Notifier for RedactCapture {
        fn send(
            &self,
            _alert: &Alert,
            _on_failure: &OnFailure,
            redact: &[SecretValue],
        ) -> Vec<NotifyOutcome> {
            self.seen
                .lock()
                .unwrap()
                .push(redact.iter().map(|v| v.as_str().to_string()).collect());
            Vec::new()
        }
    }

    #[test]
    fn secret_is_redacted_from_the_failure_notification_body() {
        let f = fixture();
        let mut j = job_with_secrets("notifsecret", "exit 1", &[("TOKEN", "op://vault/x")]);
        j.on_failure = OnFailure {
            notify: true,
            run: None,
            webhook: None,
        };

        let capture = RedactCapture {
            seen: Mutex::new(Vec::new()),
        };
        execute(
            &j,
            "run1",
            Trigger::Manual,
            &f.paths,
            &f.store,
            &SystemClock,
            DEFAULT_OUTPUT_CAP,
            &capture,
            Some("echo notif-secret-9f2 #{}"),
        )
        .unwrap();

        let seen = capture.seen.lock().unwrap();
        assert_eq!(
            seen.len(),
            1,
            "a failing job with notify=true must dispatch exactly one alert"
        );
        assert_eq!(
            seen[0],
            vec!["notif-secret-9f2".to_string()],
            "the resolved secret must reach notification dispatch"
        );
    }
}
