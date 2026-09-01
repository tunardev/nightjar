use nightjar_cli::clock::FixedClock;
use nightjar_cli::daemon::Daemon;
use nightjar_cli::paths::Paths;
use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::io::AsRawFd;
use std::os::unix::net::UnixStream;
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::sync::mpsc;
use std::time::{Duration, Instant};

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_nightjar")
}

fn nj(tmp: &Path, args: &[&str]) -> std::process::Output {
    assert!(
        !args.contains(&"install") && !args.contains(&"uninstall"),
        "registering subcommands must go through nj_dry_run; this helper reaches the real launchctl/systemctl"
    );
    Command::new(bin())
        .args(args)
        .env("NIGHTJAR_HOME", tmp)
        .env("NIGHTJAR_SERVICE_INSTALL_ROOT", tmp.join("service_units"))
        .output()
        .unwrap()
}

fn nj_dry_run(tmp: &Path, args: &[&str]) -> std::process::Output {
    Command::new(bin())
        .args(args)
        .env("NIGHTJAR_HOME", tmp)
        .env("NIGHTJAR_SERVICE_INSTALL_ROOT", tmp.join("service_units"))
        .env("NIGHTJAR_SERVICE_DRY_RUN", "1")
        .output()
        .unwrap()
}

#[test]
fn install_writes_unit_and_reports_what_it_would_run() {
    let tmp = tempfile::tempdir().unwrap();
    let out = nj_dry_run(tmp.path(), &["service", "install"]);
    let s = String::from_utf8_lossy(&out.stdout);

    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(s.contains("daemon"), "must say what it registers: {s}");

    let units: Vec<_> = std::fs::read_dir(tmp.path().join("service_units"))
        .unwrap()
        .collect();
    assert_eq!(units.len(), 1, "expected exactly one unit file written");
}

#[test]
fn status_reports_not_installed_when_home_is_fresh() {
    let tmp = tempfile::tempdir().unwrap();
    let out = nj(tmp.path(), &["service", "status"]);
    let s = String::from_utf8_lossy(&out.stdout).to_lowercase();

    assert!(out.status.success());
    assert!(s.contains("not installed"), "got: {s}");
}

#[test]
fn daemon_fires_job_without_anyone_running_it() {
    let tmp = tempfile::tempdir().unwrap();
    let jobs = tmp.path().join("jobs");
    std::fs::create_dir_all(&jobs).unwrap();
    std::fs::write(
        jobs.join("tick.toml"),
        "command = \"echo fired\"\nschedule = \"* * * * * *\"\n",
    )
    .unwrap();

    let mut child = Command::new(bin())
        .arg("daemon")
        .env("NIGHTJAR_HOME", tmp.path())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .unwrap();

    let deadline = Instant::now() + Duration::from_secs(20);
    let mut fired = false;
    while Instant::now() < deadline {
        let out = Command::new(bin())
            .args(["status", "--json"])
            .env("NIGHTJAR_HOME", tmp.path())
            .output()
            .unwrap();
        if String::from_utf8_lossy(&out.stdout).contains("\"status\":\"success\"") {
            fired = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(250));
    }

    let _ = child.kill();
    let _ = child.wait();

    assert!(fired, "daemon did not fire the job within 20s");

    let logs = Command::new(bin())
        .args(["logs", "tick"])
        .env("NIGHTJAR_HOME", tmp.path())
        .output()
        .unwrap();
    assert!(
        String::from_utf8_lossy(&logs.stdout).contains("fired"),
        "captured output missing"
    );
}

#[test]
fn daemon_refuses_to_start_when_another_instance_is_already_running() {
    let tmp = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(tmp.path().join("jobs")).unwrap();

    let mut first = Command::new(bin())
        .arg("daemon")
        .env("NIGHTJAR_HOME", tmp.path())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();

    let stderr = first.stderr.take().unwrap();
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        for line in BufReader::new(stderr).lines().map_while(Result::ok) {
            if tx.send(line).is_err() {
                break;
            }
        }
    });

    let deadline = Instant::now() + Duration::from_secs(10);
    let mut first_is_up = false;
    while Instant::now() < deadline {
        match rx.recv_timeout(Duration::from_millis(200)) {
            Ok(line) if line.contains("daemon started") => {
                first_is_up = true;
                break;
            }
            Ok(_) | Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }
    assert!(first_is_up, "first daemon never reported starting up");

    let second = Command::new(bin())
        .arg("daemon")
        .env("NIGHTJAR_HOME", tmp.path())
        .output()
        .unwrap();

    let _ = first.kill();
    let _ = first.wait();

    assert!(
        !second.status.success(),
        "a second daemon on the same NIGHTJAR_HOME must exit non-zero"
    );
    let msg = String::from_utf8_lossy(&second.stderr);
    assert!(
        msg.to_lowercase().contains("already running"),
        "message should name the condition; got: {msg}"
    );
}

#[test]
fn forked_child_does_not_keep_lock_held_when_forked_before_daemon_exits() {
    let tmp = tempfile::tempdir().unwrap();
    let paths = Paths::for_root(tmp.path());
    let clock = Arc::new(FixedClock::new("2026-06-01T00:00:00Z".parse().unwrap()));

    let first = Daemon::new(paths.clone(), clock.clone()).unwrap();
    assert!(
        Daemon::new(paths.clone(), clock.clone()).is_err(),
        "lock not held before fork window opens"
    );

    let (mut host, gate) = UnixStream::pair().unwrap();
    host.set_read_timeout(Some(Duration::from_secs(10)))
        .unwrap();
    let gate_fd = gate.as_raw_fd();
    let forked = std::thread::spawn(move || {
        let mut cmd = Command::new("/bin/sh");
        cmd.args(["-c", "exit 0"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        unsafe {
            cmd.pre_exec(move || {
                let ping = [0u8; 1];
                let mut go = [0u8; 1];
                libc::write(gate_fd, ping.as_ptr().cast(), 1);
                libc::read(gate_fd, go.as_mut_ptr().cast(), 1);
                Ok(())
            });
        }
        cmd.spawn().and_then(|mut c| c.wait())
    });

    let reached_gate = host.read_exact(&mut [0u8; 1]).is_ok();

    drop(first);
    let successor = Daemon::new(paths, clock);

    let _ = host.write_all(&[1]);
    let gated = forked.join().unwrap();

    assert!(reached_gate, "child never reached its gate");
    gated.expect("the gated child must have spawned and exited");
    assert!(
        successor.is_ok(),
        "lock not released: {:?}",
        successor.err()
    );
}

fn wait_until_up(child: &mut std::process::Child) -> bool {
    let stderr = child.stderr.take().unwrap();
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        for line in BufReader::new(stderr).lines().map_while(Result::ok) {
            let _ = tx.send(line);
        }
    });
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        match rx.recv_timeout(Duration::from_millis(200)) {
            Ok(line) if line.contains("daemon started") => return true,
            Ok(_) | Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }
    false
}

#[test]
fn daemon_exits_cleanly_and_releases_lock_when_sigtermed() {
    use std::os::unix::process::ExitStatusExt;

    let tmp = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(tmp.path().join("jobs")).unwrap();

    let mut first = Command::new(bin())
        .arg("daemon")
        .env("NIGHTJAR_HOME", tmp.path())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    assert!(wait_until_up(&mut first), "the first daemon never came up");

    unsafe { libc::kill(libc::pid_t::try_from(first.id()).unwrap(), libc::SIGTERM) };
    let status = first.wait().unwrap();

    assert_eq!(
        status.code(),
        Some(0),
        "a deliberate stop must be a clean exit, not death by signal; signal={:?}",
        status.signal()
    );
    assert_eq!(status.signal(), None, "must not die by the signal itself");

    let mut second = Command::new(bin())
        .arg("daemon")
        .env("NIGHTJAR_HOME", tmp.path())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let up = wait_until_up(&mut second);
    let _ = second.kill();
    let _ = second.wait();
    assert!(up, "the lock was not released by the stopped daemon");
}

#[test]
fn concurrent_first_time_opens_from_real_processes_all_succeed() {
    for _ in 0..8 {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join("jobs")).unwrap();

        let children: Vec<_> = (0..4)
            .map(|_| {
                Command::new(bin())
                    .args(["status", "--json"])
                    .env("NIGHTJAR_HOME", tmp.path())
                    .stdout(Stdio::null())
                    .stderr(Stdio::piped())
                    .spawn()
                    .expect("spawn nightjar status")
            })
            .collect();

        for child in children {
            let out = child.wait_with_output().unwrap();
            assert!(
                out.status.success(),
                "a concurrent `status` invocation failed: {}",
                String::from_utf8_lossy(&out.stderr)
            );
        }
    }
}

#[test]
fn concurrent_opens_of_out_of_date_database_all_succeed() {
    for _ in 0..8 {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join("jobs")).unwrap();
        write_v1_database(&tmp.path().join("nightjar.db"));

        let children: Vec<_> = (0..4)
            .map(|_| {
                Command::new(bin())
                    .args(["status", "--json"])
                    .env("NIGHTJAR_HOME", tmp.path())
                    .stdout(Stdio::null())
                    .stderr(Stdio::piped())
                    .spawn()
                    .expect("spawn nightjar status")
            })
            .collect();

        for child in children {
            let out = child.wait_with_output().unwrap();
            assert!(
                out.status.success(),
                "a concurrent `status` invocation failed: {}",
                String::from_utf8_lossy(&out.stderr)
            );
        }
    }
}

fn write_v1_database(path: &std::path::Path) {
    let conn = rusqlite::Connection::open(path).unwrap();
    conn.pragma_update(None, "journal_mode", "WAL").unwrap();
    conn.execute_batch(
        r"
        BEGIN;
        CREATE TABLE runs (
            id            TEXT PRIMARY KEY,
            job           TEXT NOT NULL,
            trigger       TEXT NOT NULL,
            started_at    INTEGER NOT NULL,
            finished_at   INTEGER,
            exit_code     INTEGER,
            duration_ms   INTEGER,
            status        TEXT NOT NULL,
            pid           INTEGER,
            stdout_path   TEXT,
            stderr_path   TEXT,
            output_bytes  INTEGER NOT NULL DEFAULT 0
        );
        CREATE INDEX runs_job_started ON runs(job, started_at DESC);
        CREATE INDEX runs_status ON runs(status);
        CREATE TABLE job_state (
            job                  TEXT PRIMARY KEY,
            last_run_at          INTEGER,
            next_run_at          INTEGER,
            consecutive_failures INTEGER NOT NULL DEFAULT 0,
            last_notified_at     INTEGER
        );
        CREATE TABLE daemon (
            id           INTEGER PRIMARY KEY CHECK (id = 1),
            heartbeat_at INTEGER NOT NULL,
            pid          INTEGER NOT NULL,
            version      TEXT NOT NULL
        );
        CREATE TABLE schema_version (
            id INTEGER PRIMARY KEY CHECK (id = 1),
            v  INTEGER NOT NULL
        );
        INSERT INTO schema_version (id, v) VALUES (1, 1);
        COMMIT;
        ",
    )
    .unwrap();
}
