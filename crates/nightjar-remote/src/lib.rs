use nightjar_core::process::{own_process_group, signal_group};
use std::collections::{HashMap, HashSet};
use std::io::Read;
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::Mutex;
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

/// `Success` keeps `--json` text unparsed for the caller to parse.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostResult {
    pub host: String,
    pub outcome: HostOutcome,
}

/// Exit 127 means ssh connected but the remote has no `nightjar` on
/// `PATH`. `Unreachable` means ssh itself failed. `Success` keeps the
/// remote's real exit code so callers can match manual ssh use.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HostOutcome {
    Success(String, i32),
    Unreachable,
    MissingBinary,
}

/// `collect_results` classifies this into a `HostOutcome`, so fake
/// runners never need nightjar's own interpretation of it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RunOutcome {
    Exited { stdout: Vec<u8>, code: i32 },
    ConnectFailed,
}

/// Lets `fan_out`'s tests substitute a fake for real SSH. `Send + Sync`
/// is required up front to avoid breaking every implementor later.
pub trait HostRunner: Send + Sync {
    fn run(&self, host: &str, args: &[String]) -> RunOutcome;
}

/// Bounds only the TCP handshake, not a host that wedges after
/// accepting. `BatchMode=yes` turns a password or host-key prompt into
/// an immediate failure. `COMMAND_TIMEOUT` covers the rest.
const CONNECT_TIMEOUT_SECS: u32 = 10;

/// Bounds the run past the handshake, which `CONNECT_TIMEOUT_SECS`
/// doesn't cover. We kill the child ourselves, touching no ssh setting.
const COMMAND_TIMEOUT: Duration = Duration::from_secs(60);

/// Short. `ssh` has no application state to flush, unlike the job runner.
const KILL_GRACE: Duration = Duration::from_secs(2);

const POLL: Duration = Duration::from_millis(50);

/// Caps simultaneous connections, so a large `--host` fleet can't
/// self-inflict a denial of service.
const MAX_CONCURRENT_CONNECTIONS: usize = 8;

/// Argv for `ssh`, without the `ssh` binary itself. Host-key checking
/// must never be touched here. That is the whole security case for using
/// SSH instead of a new protocol.
///
/// Without `--`, ssh could read a host string starting with `-` as an
/// option, not a hostname. That includes an option that runs a local
/// command via `ProxyCommand`.
///
/// `ssh` joins its trailing argv with spaces before sending it over the
/// wire. An unquoted space or `;` would be re-split by the remote shell.
fn ssh_argv(host: &str, args: &[String]) -> Vec<String> {
    let mut invocation = vec![
        "-o".to_string(),
        "BatchMode=yes".to_string(),
        "-o".to_string(),
        format!("ConnectTimeout={CONNECT_TIMEOUT_SECS}"),
        "--".to_string(),
        host.to_string(),
        "nightjar".to_string(),
    ];
    invocation.extend(args.iter().map(|a| shell_quote(a)));
    invocation
}

pub use nightjar_core::shell::quote as shell_quote;

struct RealHostRunner;

impl HostRunner for RealHostRunner {
    fn run(&self, host: &str, args: &[String]) -> RunOutcome {
        run_command_with_timeout("ssh", &ssh_argv(host, args), COMMAND_TIMEOUT)
    }
}

struct ChildGuard<'a> {
    child: &'a mut Child,
    done: bool,
}

impl<'a> ChildGuard<'a> {
    fn new(child: &'a mut Child) -> Self {
        Self { child, done: false }
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
        // Immediate kill, unlike `kill_wedged`. No exit was classified here.
        signal_group(self.child.id(), libc::SIGKILL);
        let _ = self.child.wait();
    }
}

/// Enforces `timeout` itself, since ssh's `ConnectTimeout` only bounds
/// the handshake.
fn run_command_with_timeout(program: &str, argv: &[String], timeout: Duration) -> RunOutcome {
    let mut cmd = Command::new(program);
    cmd.args(argv)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        // Piped-but-unread would fill the pipe buffer and block the child's `write()`.
        .stderr(Stdio::null());
    own_process_group(&mut cmd);

    let Ok(mut child) = cmd.spawn() else {
        return RunOutcome::ConnectFailed;
    };
    let mut guard = ChildGuard::new(&mut child);

    let stdout = guard.child.stdout.take().expect("stdout was piped");
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        let mut buf = Vec::new();
        let mut stdout = stdout;
        let _ = stdout.read_to_end(&mut buf);
        let _ = tx.send(buf);
    });

    let deadline = Instant::now() + timeout;
    loop {
        match guard.child.try_wait() {
            Ok(Some(status)) => {
                guard.mark_done();
                // A backgrounded descendant can keep the stdout pipe open after the
                // child exits.
                let remaining = deadline.saturating_duration_since(Instant::now());
                let stdout = rx.recv_timeout(remaining).unwrap_or_default();
                return classify_exit(status, stdout);
            }
            Ok(None) => {}
            Err(_) => return RunOutcome::ConnectFailed,
        }
        if Instant::now() >= deadline {
            let outcome = kill_wedged(guard.child);
            guard.mark_done();
            return outcome;
        }
        thread::sleep(POLL);
    }
}

/// `SIGTERM` then `SIGKILL`, against the whole process group. This kills
/// local descendants too, like a `ControlMaster` or agent-forwarding
/// helper.
fn kill_wedged(child: &mut Child) -> RunOutcome {
    let pid = child.id();
    signal_group(pid, libc::SIGTERM);

    let grace_deadline = Instant::now() + KILL_GRACE;
    while Instant::now() < grace_deadline {
        if let Ok(Some(status)) = child.try_wait() {
            return classify_exit(status, Vec::new());
        }
        thread::sleep(POLL);
    }

    signal_group(pid, libc::SIGKILL);
    match child.wait() {
        Ok(status) => classify_exit(status, Vec::new()),
        Err(_) => RunOutcome::ConnectFailed,
    }
}

/// ssh(1) exits 255 when it fails before running the remote command. No
/// nightjar subcommand exits 255, so this is never a real outcome. A
/// `None` code (signal death) also carries no remote outcome.
fn classify_exit(status: ExitStatus, stdout: Vec<u8>) -> RunOutcome {
    match status.code() {
        Some(255) | None => RunOutcome::ConnectFailed,
        Some(code) => RunOutcome::Exited { stdout, code },
    }
}

pub fn fan_out(hosts: &[String], args: &[String]) -> Vec<HostResult> {
    collect_results(hosts, args, &RealHostRunner)
}

fn classify(outcome: RunOutcome) -> HostOutcome {
    match outcome {
        RunOutcome::ConnectFailed => HostOutcome::Unreachable,
        RunOutcome::Exited { code: 127, .. } => HostOutcome::MissingBinary,
        RunOutcome::Exited { stdout, code } => {
            HostOutcome::Success(String::from_utf8_lossy(&stdout).into_owned(), code)
        }
    }
}

/// Results return in the caller's original order, not completion order.
fn collect_results(hosts: &[String], args: &[String], runner: &dyn HostRunner) -> Vec<HostResult> {
    let mut seen = HashSet::new();
    let mut order = Vec::new();
    for host in hosts {
        if seen.insert(host.clone()) {
            order.push(host.clone());
        }
    }

    let worker_count = order.len().min(MAX_CONCURRENT_CONNECTIONS);
    let (work_tx, work_rx) = mpsc::channel::<String>();
    let work_rx = Mutex::new(work_rx);
    let (result_tx, result_rx) = mpsc::channel::<(String, HostOutcome)>();

    thread::scope(|scope| {
        for _ in 0..worker_count {
            let work_rx = &work_rx;
            let result_tx = result_tx.clone();
            scope.spawn(move || {
                loop {
                    // The lock must drop before matching on `recv()`'s result. A `while
                    // let` here would serialize every worker on this lock.
                    let received = {
                        let queue = work_rx.lock().unwrap();
                        queue.recv()
                    };
                    let Ok(host) = received else {
                        break;
                    };
                    let outcome = classify(runner.run(&host, args));
                    let _ = result_tx.send((host, outcome));
                }
            });
        }
        // This clone must be dropped, or `result_rx` below waits forever.
        drop(result_tx);

        for host in &order {
            let _ = work_tx.send(host.clone());
        }
        // Closes the queue, so a drained `recv()` returns `Err` instead of blocking.
        drop(work_tx);
    });

    let mut outcomes: HashMap<String, HostOutcome> = result_rx.into_iter().collect();
    order
        .into_iter()
        .map(|host| {
            let outcome = outcomes
                .remove(&host)
                .expect("thread::scope joined every worker, and each reports before exiting");
            HostResult { host, outcome }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    struct RecordingRunner {
        calls: Mutex<Vec<String>>,
    }

    impl RecordingRunner {
        fn new() -> Self {
            Self {
                calls: Mutex::new(Vec::new()),
            }
        }
    }

    impl HostRunner for RecordingRunner {
        fn run(&self, host: &str, _args: &[String]) -> RunOutcome {
            self.calls.lock().unwrap().push(host.to_string());
            RunOutcome::Exited {
                stdout: b"{}".to_vec(),
                code: 0,
            }
        }
    }

    #[test]
    fn host_is_contacted_once_when_it_is_duplicated() {
        let runner = RecordingRunner::new();
        let hosts = vec!["a".to_string(), "a".to_string()];

        let results = collect_results(&hosts, &[], &runner);

        assert_eq!(
            runner.calls.lock().unwrap().len(),
            1,
            "expected the runner to be invoked exactly once for the duplicated host"
        );
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].host, "a");
    }

    #[test]
    fn every_unique_host_is_contacted() {
        let runner = RecordingRunner::new();
        let hosts = vec!["a".to_string(), "b".to_string()];

        let results = collect_results(&hosts, &[], &runner);

        assert_eq!(results.len(), 2);
        let mut calls = runner.calls.lock().unwrap().clone();
        calls.sort();
        assert_eq!(calls, vec!["a", "b"]);
    }

    struct PerHostRunner;

    impl HostRunner for PerHostRunner {
        fn run(&self, host: &str, _args: &[String]) -> RunOutcome {
            RunOutcome::Exited {
                stdout: format!(r#"{{"host":"{host}"}}"#).into_bytes(),
                code: 0,
            }
        }
    }

    #[test]
    fn every_host_is_contacted_and_results_are_merged() {
        let hosts = vec!["a".to_string(), "b".to_string(), "c".to_string()];

        let results = collect_results(&hosts, &[], &PerHostRunner);

        assert_eq!(results.len(), 3);
        assert_eq!(
            results.iter().map(|r| r.host.as_str()).collect::<Vec<_>>(),
            vec!["a", "b", "c"]
        );
        for r in &results {
            assert_eq!(
                r.outcome,
                HostOutcome::Success(format!(r#"{{"host":"{}"}}"#, r.host), 0),
                "each host's own output must land against that host's own row"
            );
        }
    }

    #[test]
    fn remote_exit_code_survives_classification_when_it_is_nonzero() {
        struct NonZeroRunner;
        impl HostRunner for NonZeroRunner {
            fn run(&self, _host: &str, _args: &[String]) -> RunOutcome {
                RunOutcome::Exited {
                    stdout: b"{}".to_vec(),
                    code: 1,
                }
            }
        }

        let results = collect_results(&["web1".to_string()], &[], &NonZeroRunner);

        assert_eq!(
            results[0].outcome,
            HostOutcome::Success("{}".to_string(), 1)
        );
    }

    #[test]
    fn connect_failure_is_reported_as_unreachable() {
        struct FailingRunner;
        impl HostRunner for FailingRunner {
            fn run(&self, _host: &str, _args: &[String]) -> RunOutcome {
                RunOutcome::ConnectFailed
            }
        }

        let results = collect_results(&["web1".to_string()], &[], &FailingRunner);

        assert_eq!(results[0].outcome, HostOutcome::Unreachable);
    }

    #[test]
    fn host_reports_missing_binary_when_the_nightjar_binary_is_missing() {
        struct NoBinaryRunner;
        impl HostRunner for NoBinaryRunner {
            fn run(&self, _host: &str, _args: &[String]) -> RunOutcome {
                RunOutcome::Exited {
                    stdout: Vec::new(),
                    code: 127,
                }
            }
        }

        let results = collect_results(&["web1".to_string()], &[], &NoBinaryRunner);

        assert_eq!(results[0].outcome, HostOutcome::MissingBinary);
    }

    #[test]
    fn one_unreachable_host_does_not_hide_the_others() {
        struct MixedRunner;
        impl HostRunner for MixedRunner {
            fn run(&self, host: &str, _args: &[String]) -> RunOutcome {
                if host == "dead" {
                    RunOutcome::ConnectFailed
                } else {
                    RunOutcome::Exited {
                        stdout: b"{}".to_vec(),
                        code: 0,
                    }
                }
            }
        }

        let mut hosts: Vec<String> = (0..9).map(|i| format!("host{i}")).collect();
        hosts.push("dead".to_string());

        let results = collect_results(&hosts, &[], &MixedRunner);

        assert_eq!(results.len(), 10, "the dead host must not swallow any row");
        let unreachable = results
            .iter()
            .filter(|r| r.outcome == HostOutcome::Unreachable)
            .count();
        let succeeded = results
            .iter()
            .filter(|r| matches!(r.outcome, HostOutcome::Success(_, _)))
            .count();
        assert_eq!(unreachable, 1);
        assert_eq!(succeeded, 9);
    }

    struct ConcurrencyRecordingRunner {
        active: AtomicUsize,
        peak: AtomicUsize,
    }

    impl ConcurrencyRecordingRunner {
        fn new() -> Self {
            Self {
                active: AtomicUsize::new(0),
                peak: AtomicUsize::new(0),
            }
        }
    }

    impl HostRunner for ConcurrencyRecordingRunner {
        fn run(&self, _host: &str, _args: &[String]) -> RunOutcome {
            let current = self.active.fetch_add(1, Ordering::SeqCst) + 1;
            self.peak.fetch_max(current, Ordering::SeqCst);
            thread::sleep(Duration::from_millis(50));
            self.active.fetch_sub(1, Ordering::SeqCst);
            RunOutcome::Exited {
                stdout: Vec::new(),
                code: 0,
            }
        }
    }

    #[test]
    fn concurrency_is_bounded_at_eight() {
        let hosts: Vec<String> = (0..20).map(|i| format!("host{i}")).collect();
        let runner = ConcurrencyRecordingRunner::new();

        let results = collect_results(&hosts, &[], &runner);

        assert_eq!(results.len(), 20);
        let peak = runner.peak.load(Ordering::SeqCst);
        assert!(
            peak >= 4,
            "expected genuine overlap among the 20 hosts, peak was only {peak}"
        );
        assert!(
            peak <= 8,
            "fan-out must never open more than 8 connections at once, peak was {peak}"
        );
    }

    #[test]
    fn ssh_invocation_never_weakens_host_key_checking() {
        let argv = ssh_argv("web1", &["status".to_string(), "--json".to_string()]);

        let expected: Vec<String> = [
            "-o",
            "BatchMode=yes",
            "-o",
            "ConnectTimeout=10",
            "--",
            "web1",
            "nightjar",
            "status",
            "--json",
        ]
        .into_iter()
        .map(str::to_string)
        .collect();
        assert_eq!(argv, expected);

        assert!(
            !argv.iter().any(|a| a.contains("StrictHostKeyChecking")),
            "host-key checking must never be touched: got {argv:?}"
        );
        assert!(
            !argv.iter().any(|a| a.contains("PasswordAuthentication")),
            "auth method must never be forced: got {argv:?}"
        );
    }

    #[test]
    fn host_string_can_never_precede_the_terminator_when_it_is_hostile() {
        for host in [
            "-oProxyCommand=touch /tmp/pwned",
            "-oStrictHostKeyChecking=no",
            "-oBatchMode=no",
            "-x",
        ] {
            let argv = ssh_argv(host, &["status".to_string()]);
            let dashdash = argv
                .iter()
                .position(|a| a == "--")
                .unwrap_or_else(|| panic!("no -- terminator for host {host:?}: {argv:?}"));
            assert_eq!(
                argv.get(dashdash + 1).map(String::as_str),
                Some(host),
                "the host must be the argument immediately after --, got {argv:?}"
            );
            assert!(
                argv[..dashdash].iter().all(|a| a != host),
                "host appeared before --: {argv:?}"
            );
        }
    }

    #[test]
    fn argument_is_quoted_for_the_remote_shell_when_it_contains_a_space() {
        let argv = ssh_argv(
            "web1",
            &[
                "logs".to_string(),
                "my backup".to_string(),
                "--json".to_string(),
            ],
        );
        assert!(argv.contains(&"'my backup'".to_string()), "got {argv:?}");
        assert!(argv.contains(&"logs".to_string()), "got {argv:?}");
        assert!(argv.contains(&"--json".to_string()), "got {argv:?}");
    }

    #[test]
    fn argument_cannot_terminate_the_remote_command_when_it_contains_a_semicolon() {
        let hostile = "x; touch /tmp/pwned";
        let argv = ssh_argv(
            "web1",
            &[
                "logs".to_string(),
                "backup".to_string(),
                "--run".to_string(),
                hostile.to_string(),
            ],
        );
        assert!(argv.contains(&format!("'{hostile}'")), "got {argv:?}");
    }

    #[test]
    fn embedded_single_quote_is_escaped_not_left_to_close_the_quote_early() {
        let argv = ssh_argv("web1", &["logs".to_string(), "o'brien".to_string()]);
        assert!(argv.contains(&"'o'\\''brien'".to_string()), "got {argv:?}");
    }

    #[test]
    fn host_is_bounded_by_a_connect_timeout_when_it_hangs() {
        let argv = ssh_argv("web1", &[]);

        assert!(
            argv.iter()
                .any(|a| a == &format!("ConnectTimeout={CONNECT_TIMEOUT_SECS}")),
            "got {argv:?}"
        );
    }

    #[test]
    fn ssh_level_connection_failure_is_reported_as_unreachable() {
        let status = Command::new("sh")
            .arg("-c")
            .arg("exit 255")
            .status()
            .unwrap();

        assert_eq!(classify_exit(status, Vec::new()), RunOutcome::ConnectFailed);
    }

    #[test]
    fn remote_exit_code_passes_through_when_ssh_itself_has_connected() {
        let status = Command::new("sh").arg("-c").arg("exit 0").status().unwrap();

        assert_eq!(
            classify_exit(status, b"{}".to_vec()),
            RunOutcome::Exited {
                stdout: b"{}".to_vec(),
                code: 0,
            }
        );
    }

    #[test]
    fn program_is_reported_as_unreachable_when_it_cannot_even_spawn() {
        let outcome = run_command_with_timeout(
            "/nonexistent/nightjar-test-not-a-real-binary",
            &[],
            Duration::from_secs(5),
        );

        assert_eq!(outcome, RunOutcome::ConnectFailed);
    }

    #[test]
    fn run_command_with_timeout_captures_stdout_when_it_succeeds_quickly() {
        let outcome = run_command_with_timeout(
            "sh",
            &["-c".to_string(), "printf hello".to_string()],
            Duration::from_secs(5),
        );

        assert_eq!(
            outcome,
            RunOutcome::Exited {
                stdout: b"hello".to_vec(),
                code: 0,
            }
        );
    }

    #[test]
    fn host_is_bounded_and_reported_unreachable_when_it_connects_and_then_wedges() {
        let started = Instant::now();
        let (tx, rx) = mpsc::channel();
        thread::spawn(move || {
            let outcome = run_command_with_timeout(
                "sh",
                &["-c".to_string(), "sleep 30".to_string()],
                Duration::from_millis(100),
            );
            let _ = tx.send(outcome);
        });

        let outcome = rx
            .recv_timeout(Duration::from_secs(10))
            .expect("run_command_with_timeout did not return within 10s of its own 100ms bound");

        assert_eq!(outcome, RunOutcome::ConnectFailed);
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "must not wait out the wedge; took {:?}",
            started.elapsed()
        );
    }

    #[test]
    fn successful_exit_is_not_hung_when_a_backgrounded_descendant_holds_stdout_open() {
        let started = Instant::now();
        let (tx, rx) = mpsc::channel();
        thread::spawn(move || {
            let outcome = run_command_with_timeout(
                "sh",
                &["-c".to_string(), "sleep 30 & exit 0".to_string()],
                Duration::from_millis(100),
            );
            let _ = tx.send(outcome);
        });

        let outcome = rx
            .recv_timeout(Duration::from_secs(10))
            .expect("run_command_with_timeout did not return within 10s of its own 100ms bound");

        assert!(
            started.elapsed() < Duration::from_secs(5),
            "must not wait out the backgrounded descendant; took {:?}",
            started.elapsed()
        );
        assert_eq!(
            outcome,
            RunOutcome::Exited {
                stdout: Vec::new(),
                code: 0
            }
        );
    }

    #[test]
    fn child_guard_still_kills_and_reaps_its_child_when_dropped_without_mark_done() {
        let mut cmd = Command::new("sh");
        cmd.arg("-c")
            .arg("sleep 30")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        own_process_group(&mut cmd);
        let mut child = cmd.spawn().unwrap();
        let pid = libc::pid_t::try_from(child.id()).unwrap();

        {
            let _guard = ChildGuard::new(&mut child);
        }

        let still_alive = unsafe { libc::kill(pid, 0) == 0 };
        assert!(
            !still_alive,
            "the child must not survive its guard being dropped"
        );
    }
}
