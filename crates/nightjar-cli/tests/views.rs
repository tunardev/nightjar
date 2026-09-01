use std::path::Path;
use std::process::Command;

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_nightjar")
}

fn setup() -> tempfile::TempDir {
    let tmp = tempfile::tempdir().unwrap();
    let jobs = tmp.path().join("jobs");
    std::fs::create_dir_all(&jobs).unwrap();
    std::fs::write(
        jobs.join("alpha.toml"),
        "command = \"echo alpha-out\"\nschedule = \"hourly\"\n",
    )
    .unwrap();
    std::fs::write(
        jobs.join("beta.toml"),
        "command = \"exit 4\"\nschedule = \"daily at 2am\"\n",
    )
    .unwrap();
    std::fs::write(jobs.join("broken.toml"), "command = = =\n").unwrap();
    tmp
}

fn nj(home: &Path, args: &[&str]) -> std::process::Output {
    Command::new(bin())
        .args(args)
        .env("NIGHTJAR_HOME", home)
        .output()
        .unwrap()
}

fn row(out: &str, job: &str) -> String {
    out.lines()
        .find(|l| l.split_whitespace().next() == Some(job))
        .unwrap_or_else(|| panic!("no row for {job} in:\n{out}"))
        .to_string()
}

#[test]
fn list_shows_all_jobs_and_flags_broken_one() {
    let tmp = setup();
    let out = nj(tmp.path(), &["list"]);
    let s = String::from_utf8_lossy(&out.stdout);

    assert!(s.contains("alpha"));
    assert!(s.contains("beta"));
    assert!(s.contains("broken"));
    assert!(
        s.contains("invalid"),
        "broken job must be flagged; got:\n{s}"
    );
}

#[test]
fn status_reports_exit_state_after_runs() {
    let tmp = setup();
    nj(tmp.path(), &["run", "alpha"]);
    nj(tmp.path(), &["run", "beta"]);

    let out = nj(tmp.path(), &["status"]);
    let s = String::from_utf8_lossy(&out.stdout).to_string();

    let alpha = row(&s, "alpha");
    assert!(alpha.contains("ok"), "alpha exited 0; row was: {alpha}");
    assert!(!alpha.contains("FAIL"), "alpha exited 0; row was: {alpha}");
    assert!(
        alpha.contains("in "),
        "alpha has a valid schedule and must show a NEXT time; row was: {alpha}"
    );
    assert!(
        alpha.contains("hourly"),
        "alpha's SCHEDULE column must show its source text; row was: {alpha}"
    );

    let beta = row(&s, "beta");
    assert!(beta.contains("FAIL"), "beta exited 4; row was: {beta}");
    assert!(!beta.contains("ok"), "beta exited 4; row was: {beta}");
    assert!(
        beta.contains("in "),
        "beta has a valid schedule and must show a NEXT time; row was: {beta}"
    );
    assert!(
        beta.contains("daily at 2am"),
        "beta's SCHEDULE column must show its source text; row was: {beta}"
    );

    let broken = row(&s, "broken");
    assert!(broken.contains("invalid"), "row was: {broken}");
    assert!(!broken.contains("never"), "row was: {broken}");
    assert!(
        !broken.contains("in "),
        "an invalid job has no schedule and must not show a NEXT time; row was: {broken}"
    );
    assert!(
        broken.contains('—'),
        "an invalid job has no schedule and must render a dash, not a guess; row was: {broken}"
    );
}

#[test]
fn status_shows_only_that_job_when_scoped_to_single_job() {
    let tmp = setup();
    nj(tmp.path(), &["run", "alpha"]);

    let out = nj(tmp.path(), &["status", "alpha"]);
    let s = String::from_utf8_lossy(&out.stdout).to_string();

    assert!(row(&s, "alpha").contains("ok"), "got:\n{s}");
    assert!(!s.contains("beta"), "got:\n{s}");
}

#[test]
fn status_says_so_when_jobs_directory_is_empty() {
    let tmp = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(tmp.path().join("jobs")).unwrap();

    let out = nj(tmp.path(), &["status"]);
    let s = String::from_utf8_lossy(&out.stdout);

    assert!(
        s.to_lowercase().contains("no jobs"),
        "an empty jobs directory must say so, not print a bare header; got:\n{s}"
    );
}

#[test]
fn status_json_marks_unparseable_job_invalid() {
    let tmp = setup();
    nj(tmp.path(), &["run", "alpha"]);

    let out = nj(tmp.path(), &["status", "--json"]);
    let s = String::from_utf8_lossy(&out.stdout);

    assert!(
        s.contains(r#""job":"broken","schedule":null,"status":"invalid""#),
        "a job that will not parse must be reported as invalid, with no schedule; got:\n{s}"
    );
    assert!(
        s.contains(r#""job":"alpha","schedule":"hourly","status":"success""#),
        "a valid job must report its schedule source; got:\n{s}"
    );
}

fn json_row<'a>(s: &'a str, job: &str) -> &'a str {
    let marker = format!(r#"{{"job":"{job}""#);
    let start = s
        .find(&marker)
        .unwrap_or_else(|| panic!("no json row for {job} in:\n{s}"));
    let rest = &s[start..];
    let end = rest
        .find(",{\"job\":")
        .unwrap_or_else(|| rest.trim_end_matches("]}").len());
    &rest[..end]
}

#[test]
fn status_json_next_ms_is_number_for_valid_job_and_null_for_invalid_one() {
    let tmp = setup();

    let out = nj(tmp.path(), &["status", "--json"]);
    let s = String::from_utf8_lossy(&out.stdout);

    let alpha = json_row(&s, "alpha");
    assert!(
        alpha.contains(r#""next_ms":"#) && !alpha.contains(r#""next_ms":null"#),
        "alpha has a valid schedule and must report a numeric next_ms; row was: {alpha}"
    );

    let broken = json_row(&s, "broken");
    assert!(
        broken.contains(r#""next_ms":null"#),
        "broken has no schedule and must report next_ms as null; row was: {broken}"
    );
}

#[test]
fn status_json_is_machine_readable_and_has_no_ansi() {
    let tmp = setup();
    nj(tmp.path(), &["run", "alpha"]);

    let out = nj(tmp.path(), &["status", "--json"]);
    let s = String::from_utf8_lossy(&out.stdout);

    assert!(
        s.trim_start().starts_with('{'),
        "expected a JSON object with top-level \"daemon\" and \"jobs\" keys; got:\n{s}"
    );
    assert!(s.contains("\"daemon\""), "got:\n{s}");
    assert!(s.contains("\"jobs\":["), "got:\n{s}");
    assert!(s.contains("\"job\""));
    assert!(
        !s.contains('\u{1b}'),
        "json output must never contain ANSI escapes"
    );
}

#[test]
fn no_color_is_what_disables_status_color() {
    let tmp = setup();
    nj(tmp.path(), &["run", "alpha"]);

    let status_with = |no_color: Option<&str>| {
        let mut c = Command::new(bin());
        c.args(["status"])
            .env("NIGHTJAR_HOME", tmp.path())
            .env("IGNORE_IS_TERMINAL", "1")
            .env("TERM", "xterm-256color")
            .env_remove("FORCE_COLOR")
            .env_remove("CLICOLOR_FORCE")
            .env_remove("NO_COLOR");
        if let Some(v) = no_color {
            c.env("NO_COLOR", v);
        }
        String::from_utf8_lossy(&c.output().unwrap().stdout).to_string()
    };

    let colored = status_with(None);
    assert!(
        colored.contains('\u{1b}'),
        "control: status must emit colour when colour is available, or this test proves nothing; got:\n{colored}"
    );

    let plain = status_with(Some("1"));
    assert!(plain.contains("alpha"), "got:\n{plain}");
    assert!(
        !plain.contains('\u{1b}'),
        "NO_COLOR must suppress every escape; got:\n{plain}"
    );
}

#[test]
fn status_exits_non_zero_when_job_is_invalid() {
    let tmp = setup();
    let out = nj(tmp.path(), &["status"]);
    assert!(!out.status.success(), "exit code must be non-zero");
}

#[test]
fn status_exits_zero_when_every_job_is_valid() {
    let tmp = tempfile::tempdir().unwrap();
    let jobs = tmp.path().join("jobs");
    std::fs::create_dir_all(&jobs).unwrap();
    std::fs::write(
        jobs.join("alpha.toml"),
        "command = \"true\"\nschedule = \"hourly\"\n",
    )
    .unwrap();

    let out = nj(tmp.path(), &["status"]);
    assert!(out.status.success());
}

#[test]
fn status_abbreviates_every_n_schedule_in_table_but_list_keeps_it_verbatim() {
    let tmp = tempfile::tempdir().unwrap();
    let jobs = tmp.path().join("jobs");
    std::fs::create_dir_all(&jobs).unwrap();
    std::fs::write(
        jobs.join("sync.toml"),
        "command = \"true\"\nschedule = \"every 15 minutes\"\n",
    )
    .unwrap();

    let status_out = String::from_utf8_lossy(&nj(tmp.path(), &["status"]).stdout).to_string();
    let sync_row = row(&status_out, "sync");
    assert!(
        sync_row.contains("every 15m"),
        "status's SCHEDULE column must abbreviate; row was: {sync_row}"
    );
    assert!(
        !sync_row.contains("every 15 minutes"),
        "row was: {sync_row}"
    );

    let list_out = String::from_utf8_lossy(&nj(tmp.path(), &["list"]).stdout).to_string();
    let list_row = row(&list_out, "sync");
    assert!(
        list_row.contains("every 15 minutes"),
        "list must keep the verbatim source; row was: {list_row}"
    );
}

#[test]
fn status_marks_job_overdue_when_its_next_run_has_passed_with_no_run() {
    let tmp = setup();
    let db = tmp.path().join("nightjar.db");
    let store = nightjar_cli::store::Store::open(&db).unwrap();
    let past: jiff::Timestamp = "2020-01-01T00:00:00Z".parse().unwrap();
    store.set_next_run("alpha", Some(past)).unwrap();
    drop(store);

    let out = nj(tmp.path(), &["status"]);
    let s = String::from_utf8_lossy(&out.stdout);
    assert!(s.contains("OVERDUE"), "expected OVERDUE; got:\n{s}");
}

#[test]
fn status_json_reports_overdue_true_and_false_correctly_per_job() {
    let tmp = setup();
    std::fs::write(
        tmp.path().join("jobs/gamma.toml"),
        "command = \"true\"\nschedule = \"hourly\"\n",
    )
    .unwrap();

    let db = tmp.path().join("nightjar.db");
    let past: jiff::Timestamp = "2020-01-01T00:00:00Z".parse().unwrap();

    let store = nightjar_cli::store::Store::open(&db).unwrap();
    store.set_next_run("alpha", Some(past)).unwrap();

    let future = jiff::Timestamp::now() + jiff::Span::new().hours(1);
    store.set_next_run("beta", Some(future)).unwrap();
    drop(store);

    nj(tmp.path(), &["run", "gamma"]);
    let store = nightjar_cli::store::Store::open(&db).unwrap();
    store.set_next_run("gamma", Some(past)).unwrap();
    drop(store);

    let out = nj(tmp.path(), &["status", "--json"]);
    let s = String::from_utf8_lossy(&out.stdout);

    let alpha = json_row(&s, "alpha");
    assert!(
        alpha.contains(r#""overdue":true"#),
        "alpha's schedule passed with no run since; row was: {alpha}"
    );

    let beta = json_row(&s, "beta");
    assert!(
        beta.contains(r#""overdue":false"#),
        "beta's next run is still in the future; row was: {beta}"
    );

    let gamma = json_row(&s, "gamma");
    assert!(
        gamma.contains(r#""overdue":false"#),
        "gamma ran after its scheduled time; row was: {gamma}"
    );

    assert!(!s.contains('\u{1b}'), "json must never contain ANSI");
}

#[test]
fn status_json_daemon_state_is_not_responding_when_heartbeat_goes_stale() {
    let tmp = setup();
    let db = tmp.path().join("nightjar.db");
    let store = nightjar_cli::store::Store::open(&db).unwrap();
    let stale_at = jiff::Timestamp::now() - jiff::Span::new().seconds(200);
    store.write_heartbeat(stale_at, 999, "0.1.0").unwrap();
    drop(store);

    let out = nj(tmp.path(), &["status", "--json"]);
    let s = String::from_utf8_lossy(&out.stdout);

    assert!(
        s.contains(r#""daemon":{"state":"not_responding""#),
        "got:\n{s}"
    );
    assert!(s.contains(r#""pid":999"#), "got:\n{s}");
}

#[test]
fn json_output_of_every_mergeable_command_carries_schema_field() {
    let tmp = setup();
    nj(tmp.path(), &["run", "alpha"]);

    let status = nj(tmp.path(), &["status", "--json"]);
    let status_json = String::from_utf8_lossy(&status.stdout);
    assert!(status_json.contains(r#""schema":1"#), "got:\n{status_json}");

    let list = nj(tmp.path(), &["list", "--json"]);
    let list_json = String::from_utf8_lossy(&list.stdout);
    assert!(list_json.contains(r#""schema":1"#), "got:\n{list_json}");

    let logs = nj(tmp.path(), &["logs", "alpha", "--json"]);
    let logs_json = String::from_utf8_lossy(&logs.stdout);
    assert!(logs_json.contains(r#""schema":1"#), "got:\n{logs_json}");

    let tmp2 = setup();
    let no_runs = nj(tmp2.path(), &["logs", "alpha", "--json"]);
    let no_runs_json = String::from_utf8_lossy(&no_runs.stdout);
    assert!(
        no_runs_json.contains(r#""schema":1"#),
        "got:\n{no_runs_json}"
    );

    let existing_empty_home = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(existing_empty_home.path().join("jobs")).unwrap();
    let existing_empty_list = nj(existing_empty_home.path(), &["list", "--json"]);
    let existing_empty_json = String::from_utf8_lossy(&existing_empty_list.stdout);
    assert!(
        existing_empty_json.contains(r#""schema":1"#),
        "an existing-but-empty jobs directory must still report a schema; got:\n{existing_empty_json}"
    );

    let missing_jobs_dir_home = tempfile::tempdir().unwrap();
    let missing_list = nj(missing_jobs_dir_home.path(), &["list", "--json"]);
    let missing_json = String::from_utf8_lossy(&missing_list.stdout);
    assert!(
        missing_json.contains(r#""schema":1"#),
        "a missing jobs directory must still report a schema; got:\n{missing_json}"
    );
}

#[test]
fn logs_shows_captured_output_of_last_run() {
    let tmp = setup();
    nj(tmp.path(), &["run", "alpha"]);

    let out = nj(tmp.path(), &["logs", "alpha"]);
    let s = String::from_utf8_lossy(&out.stdout);
    assert!(
        s.contains("alpha-out"),
        "expected captured stdout; got:\n{s}"
    );
}

#[test]
fn logs_says_so_without_erroring_when_job_never_ran() {
    let tmp = setup();
    let out = nj(tmp.path(), &["logs", "alpha"]);

    assert!(out.status.success());
    let s = String::from_utf8_lossy(&out.stdout) + String::from_utf8_lossy(&out.stderr);
    assert!(s.to_lowercase().contains("no runs"), "got:\n{s}");
}

#[test]
fn logs_selects_named_run_not_merely_newest() {
    let tmp = setup();
    let job = tmp.path().join("jobs/alpha.toml");

    std::fs::write(
        &job,
        "command = \"echo first-run\"\nschedule = \"hourly\"\n",
    )
    .unwrap();
    nj(tmp.path(), &["run", "alpha"]);
    std::fs::write(
        &job,
        "command = \"echo second-run\"\nschedule = \"hourly\"\n",
    )
    .unwrap();
    nj(tmp.path(), &["run", "alpha"]);

    let mut ids: Vec<String> = std::fs::read_dir(tmp.path().join("runs/alpha"))
        .unwrap()
        .filter_map(std::result::Result::ok)
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x == "out"))
        .map(|p| p.file_stem().unwrap().to_string_lossy().to_string())
        .collect();
    ids.sort();
    assert_eq!(ids.len(), 2, "expected two recorded runs, got {ids:?}");

    let out = nj(tmp.path(), &["logs", "alpha", "--run", &ids[0]]);
    let s = String::from_utf8_lossy(&out.stdout);
    assert!(
        s.contains("first-run"),
        "--run must return that run's output; got:\n{s}"
    );
    assert!(!s.contains("second-run"), "got:\n{s}");

    let out = nj(tmp.path(), &["logs", "alpha"]);
    let s = String::from_utf8_lossy(&out.stdout);
    assert!(
        s.contains("second-run"),
        "without --run the newest run is right; got:\n{s}"
    );
}

#[test]
fn logs_fails_instead_of_showing_another_run_when_run_id_is_unknown() {
    let tmp = setup();
    nj(tmp.path(), &["run", "alpha"]);

    let out = nj(tmp.path(), &["logs", "alpha", "--run", "not-a-real-run-id"]);
    assert!(!out.status.success());
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("not-a-real-run-id"), "stderr was: {err}");
    assert!(!String::from_utf8_lossy(&out.stdout).contains("alpha-out"));
}

#[test]
fn status_does_not_panic_when_reader_closes_pipe_early() {
    use std::io::{BufRead, Read};
    use std::os::unix::process::ExitStatusExt;
    use std::process::Stdio;

    let tmp = tempfile::tempdir().unwrap();
    let jobs = tmp.path().join("jobs");
    std::fs::create_dir_all(&jobs).unwrap();
    for i in 0..1000 {
        std::fs::write(
            jobs.join(format!("job{i}.toml")),
            "command = \"true\"\nschedule = \"hourly\"\n",
        )
        .unwrap();
    }

    let mut child = Command::new(bin())
        .args(["status"])
        .env("NIGHTJAR_HOME", tmp.path())
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();

    let mut reader = std::io::BufReader::new(child.stdout.take().unwrap());
    let mut daemon_line = String::new();
    reader.read_line(&mut daemon_line).unwrap();
    let mut header_line = String::new();
    reader.read_line(&mut header_line).unwrap();
    assert!(
        header_line.contains("JOB"),
        "expected the table header; got: {header_line}"
    );
    drop(reader);

    let mut stderr = String::new();
    child
        .stderr
        .take()
        .unwrap()
        .read_to_string(&mut stderr)
        .unwrap();
    let status = child.wait().unwrap();

    assert!(
        !stderr.contains("panicked"),
        "must exit quietly on a closed pipe, not panic; stderr was: {stderr}"
    );
    assert_ne!(
        status.signal(),
        Some(libc::SIGABRT),
        "an unhandled panic must not abort the process; stderr was: {stderr}"
    );
    assert_eq!(
        status.signal(),
        Some(libc::SIGPIPE),
        "expected termination by SIGPIPE; status was {status:?}, stderr was: {stderr}"
    );
}

#[test]
fn list_json_reports_each_jobs_own_schedule_and_state_not_shared_default() {
    let tmp = setup();
    std::fs::write(
        tmp.path().join("jobs/gamma.toml"),
        "command = \"true\"\nschedule = \"daily\"\nenabled = false\n",
    )
    .unwrap();

    let out = nj(tmp.path(), &["list", "--json"]);
    let s = String::from_utf8_lossy(&out.stdout);

    assert!(!s.contains('\u{1b}'), "json must never contain ANSI");
    assert!(
        s.contains("\"invalid\"") || s.contains("\"error\""),
        "the broken job from setup() must be marked: {s}"
    );
    assert!(
        s.contains(r#""job":"alpha","schedule":"hourly","state":"enabled""#),
        "got:\n{s}"
    );
    assert!(
        s.contains(r#""job":"gamma","schedule":"daily","state":"disabled""#),
        "a disabled job must not be reported as enabled; got:\n{s}"
    );
}

#[test]
fn list_exits_non_zero_when_job_is_invalid() {
    let tmp = setup();
    let out = nj(tmp.path(), &["list"]);
    assert!(
        !out.status.success(),
        "a broken job file must be detectable from the exit code alone"
    );
}

#[test]
fn logs_n_limits_output_to_last_n_lines() {
    let tmp = tempfile::tempdir().unwrap();
    nj(
        tmp.path(),
        &["add", "chatty", "--cmd", "seq 1 100", "--at", "hourly"],
    );
    nj(tmp.path(), &["run", "chatty"]);
    let out = nj(tmp.path(), &["logs", "chatty", "-n", "5"]);
    let s = String::from_utf8_lossy(&out.stdout);
    let lines = s.lines().count();
    assert!(lines <= 5, "expected at most 5 lines, got {lines}");
    assert!(
        s.contains("100") && s.contains("96"),
        "the last 5 lines are 96..100; got:\n{s}"
    );
    assert!(
        !s.contains("\n1\n") && !s.contains("95"),
        "-n 5 must not include earlier lines; got:\n{s}"
    );
}

#[test]
fn logs_json_carries_run_metadata_and_output_separately() {
    let tmp = tempfile::tempdir().unwrap();
    nj(
        tmp.path(),
        &["add", "j", "--cmd", "echo hello", "--at", "hourly"],
    );
    nj(tmp.path(), &["run", "j"]);
    let out = nj(tmp.path(), &["logs", "j", "--json"]);
    let s = String::from_utf8_lossy(&out.stdout);
    assert!(s.contains("hello"));
    assert!(s.contains("\"status\""), "metadata must be present: {s}");
    assert!(
        s.contains(r#""status":"success""#),
        "the run's actual outcome must be reported: {s}"
    );
    assert!(!s.contains('\u{1b}'));
}

#[test]
fn logs_json_is_still_valid_json_when_run_id_is_unknown() {
    let tmp = tempfile::tempdir().unwrap();
    nj(
        tmp.path(),
        &["add", "j", "--cmd", "echo hello", "--at", "hourly"],
    );
    nj(tmp.path(), &["run", "j"]);

    let out = nj(
        tmp.path(),
        &["logs", "j", "--run", "not-a-real-run", "--json"],
    );
    let s = String::from_utf8_lossy(&out.stdout);
    assert!(
        !out.status.success(),
        "an unresolvable --run must still be reported as a failure"
    );
    assert!(s.trim_start().starts_with('{'), "got:\n{s}");
    assert!(s.contains("not-a-real-run"), "got:\n{s}");
    assert!(!s.contains('\u{1b}'), "json must never contain ANSI");
}

#[test]
fn logs_follow_streams_output_written_after_it_starts_watching() {
    use std::io::Read;
    use std::process::Stdio;

    let tmp = tempfile::tempdir().unwrap();
    nj(
        tmp.path(),
        &[
            "add",
            "slowjob",
            "--cmd",
            "sleep 2 && echo finished-marker",
            "--at",
            "hourly",
        ],
    );

    let mut runner = Command::new(bin())
        .args(["run", "slowjob"])
        .env("NIGHTJAR_HOME", tmp.path())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();

    let db = tmp.path().join("nightjar.db");
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
        let running = nightjar_cli::store::Store::open(&db)
            .ok()
            .and_then(|s| s.last_run("slowjob").ok().flatten())
            .is_some_and(|r| r.status == nightjar_cli::store::run::RunStatus::Running);
        if running {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "the run never reached `running` in time"
        );
        std::thread::sleep(std::time::Duration::from_millis(50));
    }

    let mut follower = Command::new(bin())
        .args(["logs", "slowjob", "-f"])
        .env("NIGHTJAR_HOME", tmp.path())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
    let status = loop {
        if let Some(status) = follower.try_wait().unwrap() {
            break status;
        }
        if std::time::Instant::now() >= deadline {
            let _ = follower.kill();
            let _ = follower.wait();
            runner.kill().ok();
            runner.wait().ok();
            panic!("`logs -f` did not exit after the run finished");
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    };

    let mut out = String::new();
    follower
        .stdout
        .take()
        .unwrap()
        .read_to_string(&mut out)
        .unwrap();

    assert!(status.success(), "logs -f exited with {status:?}");
    assert!(
        out.contains("finished-marker"),
        "-f must show output written after it started watching; got:\n{out}"
    );

    runner.wait().unwrap();
}
