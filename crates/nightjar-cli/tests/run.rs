use std::path::Path;
use std::process::Command;
use std::process::Stdio;
use std::time::{Duration, Instant};

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_nightjar")
}

fn pid_t(id: u32) -> libc::pid_t {
    libc::pid_t::try_from(id).unwrap()
}

fn setup(job_name: &str, body: &str) -> tempfile::TempDir {
    let tmp = tempfile::tempdir().unwrap();
    let jobs = tmp.path().join("jobs");
    std::fs::create_dir_all(&jobs).unwrap();
    std::fs::write(jobs.join(format!("{job_name}.toml")), body).unwrap();
    tmp
}

fn run_cmd(home: &Path, args: &[&str]) -> std::process::Output {
    Command::new(bin())
        .args(args)
        .env("NIGHTJAR_HOME", home)
        .output()
        .unwrap()
}

#[test]
fn run_executes_job_and_exits_zero() {
    let tmp = setup("hello", "command = \"echo hi\"\nschedule = \"hourly\"\n");
    let out = run_cmd(tmp.path(), &["run", "hello"]);

    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(String::from_utf8_lossy(&out.stdout).contains("hi"));
}

#[test]
fn run_propagates_failing_job_exit_code() {
    let tmp = setup("boom", "command = \"exit 7\"\nschedule = \"hourly\"\n");
    let out = run_cmd(tmp.path(), &["run", "boom"]);

    assert_eq!(out.status.code(), Some(7));
}

#[test]
fn run_fails_with_clear_message_when_job_is_unknown() {
    let tmp = tempfile::tempdir().unwrap();
    let out = run_cmd(tmp.path(), &["run", "nope"]);

    assert!(!out.status.success());
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("nope"), "stderr was: {err}");
}

#[test]
fn run_records_run_in_store() {
    let tmp = setup("recorded", "command = \"echo x\"\nschedule = \"hourly\"\n");
    run_cmd(tmp.path(), &["run", "recorded"]);

    assert!(tmp.path().join("nightjar.db").exists());
    let runs_dir = tmp.path().join("runs/recorded");
    assert!(
        runs_dir.is_dir(),
        "expected run output dir at {}",
        runs_dir.display()
    );
}

fn seed_in_flight_run(db_path: &Path, job: &str) {
    let store = nightjar_cli::store::Store::open(db_path).unwrap();
    let t: jiff::Timestamp = "2026-06-01T00:00:00Z".parse().unwrap();
    store
        .start_run(
            "already-running",
            job,
            nightjar_cli::store::run::Trigger::Manual,
            t,
            Path::new("/tmp/nightjar-test-out"),
            Path::new("/tmp/nightjar-test-err"),
        )
        .unwrap();
}

fn run_count(db_path: &Path, job: &str) -> usize {
    nightjar_cli::store::Store::open(db_path)
        .unwrap()
        .recent_runs(Some(job), 100)
        .unwrap()
        .len()
}

#[test]
fn run_refuses_when_overlap_is_skip_or_queue_and_run_is_already_in_flight() {
    for (job, policy, policy_debug) in [("skipjob", "skip", "Skip"), ("queuejob", "queue", "Queue")]
    {
        let tmp = setup(
            job,
            &format!("command = \"echo hi\"\nschedule = \"hourly\"\noverlap = \"{policy}\"\n"),
        );
        let db_path = tmp.path().join("nightjar.db");
        seed_in_flight_run(&db_path, job);

        let out = run_cmd(tmp.path(), &["run", job]);

        assert!(
            !out.status.success(),
            "a conflicting overlap must refuse under policy {policy}"
        );
        let err = String::from_utf8_lossy(&out.stderr);
        assert!(
            err.contains(job) && err.contains(policy_debug),
            "stderr must name the job and the policy that refused it; stderr was: {err}"
        );
        assert_eq!(
            run_count(&db_path, job),
            1,
            "a refused run must not create a second row under policy {policy}"
        );
    }
}

#[test]
fn run_is_allowed_alongside_in_flight_run_when_overlap_is_parallel() {
    let tmp = setup(
        "parjob",
        "command = \"echo hi\"\nschedule = \"hourly\"\noverlap = \"parallel\"\n",
    );
    let db_path = tmp.path().join("nightjar.db");
    seed_in_flight_run(&db_path, "parjob");

    let out = run_cmd(tmp.path(), &["run", "parjob"]);

    assert!(
        out.status.success(),
        "overlap = parallel must allow a second run; stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(
        run_count(&db_path, "parjob"),
        2,
        "the in-flight seed row and this run's own row"
    );
}

#[test]
fn run_is_allowed_when_nothing_is_in_flight_regardless_of_overlap_policy() {
    for policy in ["skip", "queue", "parallel"] {
        let tmp = setup(
            "idlejob",
            &format!("command = \"echo hi\"\nschedule = \"hourly\"\noverlap = \"{policy}\"\n"),
        );
        let out = run_cmd(tmp.path(), &["run", "idlejob"]);
        assert!(
            out.status.success(),
            "overlap = {policy} must allow a run when nothing is in flight; stderr: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
}

#[test]
fn run_echoes_non_utf8_output_byte_for_byte() {
    let tmp = setup(
        "binout",
        r#"command = "printf 'before\\n\\377\\376\\375after\\n'"
schedule = "hourly"
"#,
    );
    let out = run_cmd(tmp.path(), &["run", "binout"]);
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    let expected: &[u8] = b"before\n\xff\xfe\xfdafter\n";
    assert_eq!(out.stdout, expected);

    let runs_dir = tmp.path().join("runs/binout");
    let on_disk_path = std::fs::read_dir(&runs_dir)
        .unwrap()
        .filter_map(std::result::Result::ok)
        .map(|e| e.path())
        .find(|p| p.extension().is_some_and(|x| x == "out"))
        .expect("expected a captured .out file");
    let on_disk = std::fs::read(&on_disk_path).unwrap();

    assert_eq!(
        out.stdout, on_disk,
        "stdout must match the captured file on disk exactly"
    );
}

#[test]
fn run_rejects_job_name_that_escapes_jobs_directory() {
    let tmp = setup("hello", "command = \"echo hi\"\nschedule = \"hourly\"\n");
    let out = run_cmd(tmp.path(), &["run", "../../../etc/hosts"]);

    assert!(!out.status.success());
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("job name"), "stderr was: {err}");
}

#[test]
fn workdir_resolves_to_home_when_it_is_tilde() {
    let tmp = setup(
        "athome",
        "command = \"pwd\"\nschedule = \"hourly\"\nworkdir = \"~\"\nlogin_shell = false\n",
    );
    let home = tempfile::tempdir().unwrap();

    let out = Command::new(bin())
        .args(["run", "athome"])
        .env("NIGHTJAR_HOME", tmp.path())
        .env("HOME", home.path())
        .output()
        .unwrap();

    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let printed = String::from_utf8_lossy(&out.stdout);
    let printed = printed.trim();
    assert_eq!(
        std::fs::canonicalize(printed).unwrap(),
        std::fs::canonicalize(home.path()).unwrap(),
        "workdir \"~\" must resolve to $HOME; job printed {printed:?}"
    );
}

#[test]
fn run_leaves_terminal_row_not_stale_running_one_when_ctrl_c_is_received() {
    assert_run_survives_signal("interrupted", libc::SIGINT);
}

#[test]
fn run_leaves_terminal_row_not_stale_running_one_when_terminal_is_closed() {
    assert_run_survives_signal("hungup", libc::SIGHUP);
}

#[test]
fn second_stop_signal_cuts_grace_period_short() {
    let job = "stubborn";
    let tmp = setup(
        job,
        "command = \"trap '' TERM; while true; do sleep 1; done\"\nschedule = \"hourly\"\n",
    );

    let mut child = {
        let mut cmd = Command::new(bin());
        cmd.args(["run", job])
            .env("NIGHTJAR_HOME", tmp.path())
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        unsafe {
            use std::os::unix::process::CommandExt;
            cmd.pre_exec(|| {
                if libc::setpgid(0, 0) == -1 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        cmd.spawn().unwrap()
    };

    let db_path = tmp.path().join("nightjar.db");
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let reached_running = nightjar_cli::store::Store::open(&db_path)
            .ok()
            .and_then(|s| s.last_run(job).ok().flatten())
            .is_some_and(|r| r.status == nightjar_cli::store::run::RunStatus::Running);
        if reached_running {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "the run never reached `running` before the deadline"
        );
        std::thread::sleep(Duration::from_millis(20));
    }

    let started = Instant::now();
    unsafe {
        libc::kill(-pid_t(child.id()), libc::SIGTERM);
    }
    let _ = child.wait();

    std::thread::sleep(Duration::from_millis(500));
    unsafe {
        libc::kill(-pid_t(child.id()), libc::SIGTERM);
    }

    let deadline = Instant::now() + Duration::from_secs(5);
    let terminal_status = loop {
        let status = nightjar_cli::store::Store::open(&db_path)
            .ok()
            .and_then(|s| s.last_run(job).ok().flatten())
            .map(|r| r.status);
        if let Some(status) = status {
            if status != nightjar_cli::store::run::RunStatus::Running {
                break status;
            }
        }
        assert!(
            Instant::now() < deadline,
            "still not terminal after {:?}",
            started.elapsed()
        );
        std::thread::sleep(Duration::from_millis(20));
    };

    assert_ne!(
        terminal_status,
        nightjar_cli::store::run::RunStatus::Running
    );
}

fn assert_run_survives_signal(job: &str, signal: libc::c_int) {
    let tmp = setup(job, "command = \"sleep 5\"\nschedule = \"hourly\"\n");

    let mut child = {
        let mut cmd = Command::new(bin());
        cmd.args(["run", job])
            .env("NIGHTJAR_HOME", tmp.path())
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        unsafe {
            use std::os::unix::process::CommandExt;
            cmd.pre_exec(|| {
                if libc::setpgid(0, 0) == -1 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        cmd.spawn().unwrap()
    };

    let db_path = tmp.path().join("nightjar.db");
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let reached_running = nightjar_cli::store::Store::open(&db_path)
            .ok()
            .and_then(|s| s.last_run(job).ok().flatten())
            .is_some_and(|r| r.status == nightjar_cli::store::run::RunStatus::Running);
        if reached_running {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "the run never reached `running` before the deadline"
        );
        std::thread::sleep(Duration::from_millis(20));
    }

    unsafe {
        libc::kill(-pid_t(child.id()), signal);
    }

    let _ = child.wait();

    let deadline = Instant::now() + Duration::from_secs(2);
    let terminal_status = loop {
        let status = nightjar_cli::store::Store::open(&db_path)
            .ok()
            .and_then(|s| s.last_run(job).ok().flatten())
            .map(|r| r.status);
        if let Some(status) = status {
            if status != nightjar_cli::store::run::RunStatus::Running {
                break status;
            }
        }
        assert!(
            Instant::now() < deadline,
            "the run row never reached a terminal status within 2s of signal {signal}"
        );
        std::thread::sleep(Duration::from_millis(20));
    };

    assert_ne!(
        terminal_status,
        nightjar_cli::store::run::RunStatus::Running,
        "signal {signal} must leave a terminal run row, not a stale running one"
    );
}

#[test]
fn run_captures_output_from_descendant_that_outlives_shell_but_not_pump_drain() {
    let tmp = setup(
        "straggler",
        "command = \"( sleep 1; echo LATE-OUTPUT ) & echo EARLY-OUTPUT; exit 1\"\nschedule = \"hourly\"\n",
    );

    let out = run_cmd(tmp.path(), &["run", "straggler"]);

    assert_eq!(out.status.code(), Some(1));
    assert_eq!(
        out.stdout,
        b"EARLY-OUTPUT\nLATE-OUTPUT\n".as_slice(),
        "got {:?}",
        String::from_utf8_lossy(&out.stdout)
    );
}

#[test]
fn notify_channel_is_logged_to_notify_log_when_it_fails() {
    let tmp = setup(
        "logfail",
        "command = \"exit 1\"\nschedule = \"hourly\"\n[on_failure]\nrun = \"exit 9\"\n",
    );

    run_cmd(
        tmp.path(),
        &[
            "exec",
            "--job",
            "logfail",
            "--run",
            "logfail-run",
            "--trigger",
            "manual",
        ],
    );

    let log_path = tmp.path().join("notify.log");
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let contents = std::fs::read_to_string(&log_path).unwrap_or_default();
        if contents.contains("alert failed for job") && contents.contains("logfail") {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "a failing notify channel must be logged to notify.log; contents: {contents:?}"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn nightjar_exec_dies_promptly_when_sigtermed_during_slow_notify_channel() {
    let job = "slowsig";
    let tmp = setup(
        job,
        "command = \"exit 1\"\nschedule = \"hourly\"\n[on_failure]\nrun = \"sleep 30\"\n",
    );
    let run_id = "sigprobe-run";

    let mut child = {
        let mut cmd = Command::new(bin());
        cmd.args([
            "exec",
            &format!("--job={job}"),
            &format!("--run={run_id}"),
            "--trigger=manual",
        ])
        .env("NIGHTJAR_HOME", tmp.path())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
        unsafe {
            use std::os::unix::process::CommandExt;
            cmd.pre_exec(|| {
                if libc::setpgid(0, 0) == -1 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        cmd.spawn().unwrap()
    };

    let db_path = tmp.path().join("nightjar.db");
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let terminal = nightjar_cli::store::Store::open(&db_path)
            .ok()
            .and_then(|s| s.get_run(run_id).ok().flatten())
            .is_some_and(|r| r.status != nightjar_cli::store::run::RunStatus::Running);
        if terminal {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "the run row never reached a terminal status before the deadline"
        );
        std::thread::sleep(Duration::from_millis(10));
    }

    let sent_at = Instant::now();
    unsafe {
        libc::kill(pid_t(child.id()), libc::SIGTERM);
    }

    let retry_at = sent_at + Duration::from_millis(150);
    let mut retried = false;
    let poll_deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if child.try_wait().unwrap().is_some() {
            break;
        }
        if !retried && Instant::now() >= retry_at {
            unsafe {
                libc::kill(pid_t(child.id()), libc::SIGTERM);
            }
            retried = true;
        }
        assert!(
            Instant::now() < poll_deadline,
            "the wrapper did not exit even after a retried SIGTERM"
        );
        std::thread::sleep(Duration::from_millis(5));
    }
    child.wait().unwrap();
    let elapsed = sent_at.elapsed();

    assert!(elapsed < Duration::from_secs(10), "took {elapsed:?}");
}

#[test]
fn wrapper_reports_signal_in_wait_status_when_it_is_terminated() {
    use std::os::unix::process::ExitStatusExt;

    let job = "sigwait";
    let tmp = setup(job, "command = \"sleep 5\"\nschedule = \"hourly\"\n");
    let run_id = "sigwait-run";

    let mut child = {
        let mut cmd = Command::new(bin());
        cmd.args([
            "exec",
            &format!("--job={job}"),
            &format!("--run={run_id}"),
            "--trigger=manual",
        ])
        .env("NIGHTJAR_HOME", tmp.path())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
        unsafe {
            use std::os::unix::process::CommandExt;
            cmd.pre_exec(|| {
                if libc::setpgid(0, 0) == -1 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        cmd.spawn().unwrap()
    };

    let db_path = tmp.path().join("nightjar.db");
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let reached_running = nightjar_cli::store::Store::open(&db_path)
            .ok()
            .and_then(|s| s.get_run(run_id).ok().flatten())
            .is_some_and(|r| r.status == nightjar_cli::store::run::RunStatus::Running);
        if reached_running {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "the run never reached `running` before the deadline"
        );
        std::thread::sleep(Duration::from_millis(20));
    }

    unsafe {
        libc::kill(pid_t(child.id()), libc::SIGTERM);
    }

    let status = child.wait().unwrap();

    assert_eq!(
        status.signal(),
        Some(libc::SIGTERM),
        "status was {status:?}"
    );
    assert_eq!(
        status.code(),
        None,
        "a signal-terminated process must not also report an exit code; status was {status:?}"
    );
}

#[test]
fn config_toml_supplies_shell_a_job_file_does_not_name() {
    let tmp = setup("which", "command = \"echo $0\"\nschedule = \"hourly\"\n");
    std::fs::write(
        tmp.path().join("config.toml"),
        "shell = \"/bin/sh\"\nlogin_shell = false\n",
    )
    .unwrap();
    let out = run_cmd(tmp.path(), &["run", "which"]);
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "/bin/sh");

    std::fs::write(
        tmp.path().join("config.toml"),
        "shell = \"/bin/echo\"\nlogin_shell = false\n",
    )
    .unwrap();
    let out = run_cmd(tmp.path(), &["run", "which"]);
    assert_eq!(
        String::from_utf8_lossy(&out.stdout).trim(),
        "-c echo $0",
        "the global default must reach the spawn, not just the parsed struct"
    );
}

#[test]
fn run_returns_promptly_even_when_notify_channel_would_hang() {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    std::thread::spawn(move || {
        let _conn = listener.accept();
        std::thread::sleep(Duration::from_secs(15));
    });

    let tmp = setup(
        "hangs-notify",
        &format!(
            "command = \"exit 1\"\nschedule = \"hourly\"\n[on_failure]\nwebhook = \"http://{addr}/hook\"\n"
        ),
    );

    let started = Instant::now();
    let out = run_cmd(tmp.path(), &["run", "hangs-notify"]);
    let elapsed = started.elapsed();

    assert_eq!(out.status.code(), Some(1));
    assert!(
        elapsed < Duration::from_secs(5),
        "nightjar run must not wait on the real notify send; took {elapsed:?}"
    );
}

#[test]
fn cooldown_eventually_stamps_once_detached_child_finishes() {
    let tmp = setup(
        "stamps-later",
        "command = \"exit 1\"\nschedule = \"hourly\"\n[on_failure]\nrun = \"true\"\n",
    );

    run_cmd(tmp.path(), &["run", "stamps-later"]);

    let store = nightjar_cli::store::Store::open(&tmp.path().join("nightjar.db")).unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if store.last_notified_at("stamps-later").unwrap().is_some() {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "the detached notify child never stamped the cooldown"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}
