use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_nightjar")
}

fn nj(home: &Path, args: &[&str]) -> std::process::Output {
    Command::new(bin())
        .args(args)
        .env("NIGHTJAR_HOME", home)
        .env("NIGHTJAR_SERVICE_INSTALL_ROOT", home.join("service_units"))
        .output()
        .unwrap()
}

fn nj_stdin(home: &Path, args: &[&str], stdin: &str) -> std::process::Output {
    let mut child = Command::new(bin())
        .args(args)
        .env("NIGHTJAR_HOME", home)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(stdin.as_bytes())
        .unwrap();
    child.wait_with_output().unwrap()
}

#[test]
fn add_creates_job_file_that_loads_back() {
    let tmp = tempfile::tempdir().unwrap();
    let out = nj(
        tmp.path(),
        &["add", "backup", "--cmd", "echo hi", "--at", "daily at 2am"],
    );
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    let listed = String::from_utf8_lossy(&nj(tmp.path(), &["list"]).stdout).to_string();
    assert!(listed.contains("backup"));
    assert!(listed.contains("daily at 2am"));
}

#[test]
fn add_refuses_to_overwrite_when_job_already_exists() {
    let tmp = tempfile::tempdir().unwrap();
    nj(
        tmp.path(),
        &["add", "backup", "--cmd", "echo one", "--at", "hourly"],
    );
    let out = nj(
        tmp.path(),
        &["add", "backup", "--cmd", "echo two", "--at", "hourly"],
    );

    assert!(!out.status.success(), "overwriting must fail");
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("backup"), "error must name the job: {err}");

    let body = std::fs::read_to_string(tmp.path().join("jobs/backup.toml")).unwrap();
    assert!(
        body.contains("echo one"),
        "the original must survive: {body}"
    );
}

#[test]
fn add_rejects_before_writing_anything_when_schedule_is_invalid() {
    let tmp = tempfile::tempdir().unwrap();
    let out = nj(
        tmp.path(),
        &["add", "bad", "--cmd", "true", "--at", "every other tuesday"],
    );
    assert!(!out.status.success());
    assert!(
        !tmp.path().join("jobs/bad.toml").exists(),
        "a rejected schedule must leave no file"
    );
}

#[test]
fn add_rejects_before_writing_anything_when_timeout_or_catchup_is_invalid() {
    let tmp = tempfile::tempdir().unwrap();
    for (flag, value, field) in [
        ("--timeout", "5 minutes", "timeout"),
        ("--catchup", "sometimes", "catchup"),
    ] {
        let out = nj(
            tmp.path(),
            &["add", "bad", "--cmd", "true", "--at", "hourly", flag, value],
        );
        assert!(!out.status.success(), "{flag} {value} must be refused");
        let err = String::from_utf8_lossy(&out.stderr);
        assert!(err.contains(field), "the error must name the field: {err}");
        assert!(
            !tmp.path().join("jobs/bad.toml").exists(),
            "a rejected {field} must leave no file"
        );
    }
}

#[test]
fn add_rejects_when_job_name_contains_path_separator() {
    let tmp = tempfile::tempdir().unwrap();
    let out = nj(
        tmp.path(),
        &["add", "../escape", "--cmd", "true", "--at", "hourly"],
    );
    assert!(!out.status.success());
}

#[test]
fn add_hints_at_daemon_rather_than_installing_service() {
    let tmp = tempfile::tempdir().unwrap();
    let out = nj(tmp.path(), &["add", "j", "--cmd", "true", "--at", "hourly"]);
    let s = String::from_utf8_lossy(&out.stdout);
    assert!(
        s.contains("daemon") || s.contains("service"),
        "a new user needs to be told what makes this run: {s}"
    );
}

#[test]
fn doctor_reports_problems_and_exits_non_zero_when_home_is_fresh() {
    let tmp = tempfile::tempdir().unwrap();
    let out = nj(tmp.path(), &["doctor"]);
    let s = String::from_utf8_lossy(&out.stdout);
    assert!(
        !out.status.success(),
        "a diagnostic that always exits 0 is useless in a script"
    );
    assert!(
        s.to_lowercase().contains("daemon"),
        "must check the daemon: {s}"
    );
}

#[test]
fn doctor_names_job_when_its_toml_does_not_parse() {
    let tmp = tempfile::tempdir().unwrap();
    let jobs = tmp.path().join("jobs");
    std::fs::create_dir_all(&jobs).unwrap();
    std::fs::write(jobs.join("broken.toml"), "command = = =\n").unwrap();
    let out = nj(tmp.path(), &["doctor"]);
    let s = String::from_utf8_lossy(&out.stdout);
    assert!(s.contains("broken"), "must name the offending job: {s}");
    assert!(!out.status.success());
}

#[test]
fn doctor_reports_each_check_by_name_so_user_knows_what_was_examined() {
    let tmp = tempfile::tempdir().unwrap();
    let s = String::from_utf8_lossy(&nj(tmp.path(), &["doctor"]).stdout).to_lowercase();
    for expected in ["daemon", "jobs", "store"] {
        assert!(
            s.contains(expected),
            "doctor must report a `{expected}` check:\n{s}"
        );
    }
}

#[test]
fn doctor_json_is_machine_readable_and_carries_pass_fail_per_check() {
    let tmp = tempfile::tempdir().unwrap();
    let out = nj(tmp.path(), &["doctor", "--json"]);
    let s = String::from_utf8_lossy(&out.stdout);

    assert!(!s.contains('\u{1b}'), "json must never contain ANSI: {s}");
    assert!(
        s.trim_start().starts_with('{'),
        "expected a JSON object: {s}"
    );
    assert!(s.contains("\"checks\":["), "expected a checks array: {s}");
    assert!(
        s.contains("\"status\":\"pass\"")
            || s.contains("\"status\":\"warn\"")
            || s.contains("\"status\":\"fail\""),
        "expected a pass/warn/fail status per check: {s}"
    );
    assert_eq!(
        s.matches('{').count(),
        s.matches('}').count(),
        "braces must balance: {s}"
    );
}

#[test]
fn doctor_json_never_produces_human_rendering() {
    let tmp = tempfile::tempdir().unwrap();
    let s = String::from_utf8_lossy(&nj(tmp.path(), &["doctor", "--json"]).stdout).to_string();
    assert!(
        !s.contains("[fail]") && !s.contains("[ok") && !s.contains("[warn"),
        "--json leaked the human-formatted output: {s}"
    );
}

#[test]
fn doctor_exit_code_is_zero_only_when_no_check_failed() {
    let tmp = tempfile::tempdir().unwrap();
    let out = Command::new(bin())
        .args(["doctor"])
        .current_dir(tmp.path())
        .env("NIGHTJAR_HOME", "relative-home")
        .env(
            "NIGHTJAR_SERVICE_INSTALL_ROOT",
            tmp.path().join("service_units"),
        )
        .output()
        .unwrap();
    assert!(!out.status.success());
    let s = String::from_utf8_lossy(&out.stdout).to_lowercase();
    assert!(s.contains("relative"), "must explain why it failed: {s}");
}

#[test]
fn doctor_fails_when_config_toml_stops_everything_else_from_starting() {
    let tmp = tempfile::tempdir().unwrap();
    std::fs::write(tmp.path().join("config.toml"), "retention_runz = 5\n").unwrap();

    let out = nj(tmp.path(), &["doctor"]);
    let s = String::from_utf8_lossy(&out.stdout);
    assert!(
        s.contains("retention_runz"),
        "the check must name the key that will not load: {s}"
    );
    assert!(
        s.lines()
            .any(|l| l.contains("config") && l.contains("fail")),
        "the config check must fail, not warn: {s}"
    );
    assert!(!out.status.success());

    let jobs = tmp.path().join("jobs");
    std::fs::create_dir_all(&jobs).unwrap();
    std::fs::write(
        jobs.join("j.toml"),
        "command = \"true\"\nschedule = \"hourly\"\n",
    )
    .unwrap();
    assert!(
        !nj(tmp.path(), &["run", "j"]).status.success(),
        "`nightjar run` must be broken by this config, or the check proves nothing"
    );
}

#[test]
fn doctor_passes_config_check_when_there_is_no_config_file() {
    let tmp = tempfile::tempdir().unwrap();
    let out = nj(tmp.path(), &["doctor"]);
    let s = String::from_utf8_lossy(&out.stdout);
    assert!(
        s.lines().any(|l| l.contains("config") && l.contains("ok")),
        "an absent config.toml is the normal case: {s}"
    );
}

#[test]
fn import_writes_every_crontab_line_as_disabled_job() {
    let tmp = tempfile::tempdir().unwrap();
    let crontab = "# comment line\n0 2 * * * pg_dump mydb > /backups/db.sql\n*/15 * * * * rsync -a /src /dst\n";
    let out = nj_stdin(tmp.path(), &["import", "--from-stdin"], crontab);
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    let listed = String::from_utf8_lossy(&nj(tmp.path(), &["list"]).stdout).to_string();
    assert_eq!(
        listed.matches("disabled").count(),
        2,
        "imported jobs must arrive disabled so they do not double-run under cron:\n{listed}"
    );
}

#[test]
fn import_says_what_it_did_and_how_to_enable() {
    let tmp = tempfile::tempdir().unwrap();
    let out = nj_stdin(tmp.path(), &["import", "--from-stdin"], "0 2 * * * true\n");
    let s = String::from_utf8_lossy(&out.stdout);
    assert!(s.contains("disabled"), "must state they are disabled: {s}");
    assert!(s.contains("enable"), "must say how to turn them on: {s}");
}

#[test]
fn import_opts_in_explicitly_when_enable_flag_is_given() {
    let tmp = tempfile::tempdir().unwrap();
    nj_stdin(
        tmp.path(),
        &["import", "--from-stdin", "--enable"],
        "0 2 * * * true\n",
    );
    let listed = String::from_utf8_lossy(&nj(tmp.path(), &["list"]).stdout).to_string();
    assert!(listed.contains("enabled"), "got: {listed}");
}

#[test]
fn import_skips_comments_blanks_and_environment_assignments() {
    let tmp = tempfile::tempdir().unwrap();
    let crontab = "# a comment\n\nSHELL=/bin/bash\nMAILTO=me@example.com\n0 2 * * * true\n";
    let out = nj_stdin(tmp.path(), &["import", "--from-stdin"], crontab);
    let s = String::from_utf8_lossy(&out.stdout);
    assert!(
        s.contains('1') || s.to_lowercase().contains("one"),
        "exactly one job: {s}"
    );

    let n = std::fs::read_dir(tmp.path().join("jobs")).unwrap().count();
    assert_eq!(n, 1, "env assignments and comments must not become jobs");
}

#[test]
fn import_names_crontab_environment_lines_it_did_not_import() {
    let tmp = tempfile::tempdir().unwrap();
    let out = nj_stdin(
        tmp.path(),
        &["import", "--from-stdin"],
        "SHELL=/bin/bash\nPATH=/usr/local/bin:/usr/bin\nMAILTO=ops@example.com\n\
         0 4 * * * /usr/bin/backup\n",
    );
    let s = String::from_utf8_lossy(&out.stdout);
    for line in [
        "SHELL=/bin/bash",
        "PATH=/usr/local/bin:/usr/bin",
        "MAILTO=ops@example.com",
    ] {
        assert!(s.contains(line), "must name {line}: {s}");
    }
    assert!(
        out.status.success(),
        "environment lines are reported, not an error: {s}"
    );
    assert!(
        tmp.path().join("jobs/backup.toml").exists(),
        "the real crontab line must still import"
    );
}

#[test]
fn import_exits_non_zero_when_it_could_not_import_line() {
    let tmp = tempfile::tempdir().unwrap();
    let out = nj_stdin(
        tmp.path(),
        &["import", "--from-stdin"],
        "0 4 * * * /usr/bin/backup\nnot a crontab line at all\n*/5 * * * * /usr/bin/rotate\n",
    );
    assert!(!out.status.success());
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("not a crontab line at all"),
        "the skipped line must be named"
    );
    assert!(
        tmp.path().join("jobs/backup.toml").exists(),
        "a bad line must not stop lines before it from importing"
    );
    assert!(
        tmp.path().join("jobs/rotate.toml").exists(),
        "a bad line must not stop lines after it from importing"
    );
}

#[test]
fn import_refuses_line_whose_command_no_job_file_can_hold() {
    let tmp = tempfile::tempdir().unwrap();
    let out = nj_stdin(
        tmp.path(),
        &["import", "--from-stdin"],
        "0 2 * * * true\renabled = true\n",
    );
    assert!(!out.status.success());
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("control character"),
        "stderr was: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let jobs = tmp.path().join("jobs");
    let written: Vec<_> = std::fs::read_dir(&jobs)
        .map(|d| d.map(|e| e.unwrap().file_name()).collect())
        .unwrap_or_default();
    assert!(written.is_empty(), "nothing must be written: {written:?}");
}

#[test]
fn disable_then_enable_preserves_comments_and_formatting() {
    let tmp = tempfile::tempdir().unwrap();
    let jobs = tmp.path().join("jobs");
    std::fs::create_dir_all(&jobs).unwrap();
    let original = "# nightly database dump\ncommand = \"pg_dump db\"   # keep this comment\nschedule = \"daily at 2am\"\n";
    std::fs::write(jobs.join("backup.toml"), original).unwrap();

    nj(tmp.path(), &["disable", "backup"]);
    let after = std::fs::read_to_string(jobs.join("backup.toml")).unwrap();
    assert!(
        after.contains("# nightly database dump"),
        "leading comment lost: {after}"
    );
    assert!(
        after.contains("# keep this comment"),
        "inline comment lost: {after}"
    );
    assert!(after.contains("enabled = false"));

    nj(tmp.path(), &["enable", "backup"]);
    let back = std::fs::read_to_string(jobs.join("backup.toml")).unwrap();
    assert!(back.contains("# nightly database dump"));
    assert!(back.contains("enabled = true"));
}

#[test]
fn disable_is_visible_in_list_and_stops_daemon_firing_it() {
    let tmp = tempfile::tempdir().unwrap();
    nj(
        tmp.path(),
        &["add", "j", "--cmd", "true", "--at", "every 15 minutes"],
    );
    nj(tmp.path(), &["disable", "j"]);
    let s = String::from_utf8_lossy(&nj(tmp.path(), &["list"]).stdout).to_string();
    assert!(s.contains("disabled"), "got: {s}");
}

#[test]
fn rm_deletes_job_and_is_no_op_second_time() {
    let tmp = tempfile::tempdir().unwrap();
    nj(
        tmp.path(),
        &["add", "gone", "--cmd", "true", "--at", "hourly"],
    );
    assert!(nj(tmp.path(), &["rm", "gone"]).status.success());
    assert!(!tmp.path().join("jobs/gone.toml").exists());

    let again = nj(tmp.path(), &["rm", "gone"]);
    let msg = String::from_utf8_lossy(&again.stderr);
    assert!(
        msg.contains("gone"),
        "removing a missing job must say which: {msg}"
    );
}

#[test]
fn enable_reports_parse_error_and_changes_nothing_when_job_toml_does_not_parse() {
    let tmp = tempfile::tempdir().unwrap();
    let jobs = tmp.path().join("jobs");
    std::fs::create_dir_all(&jobs).unwrap();
    std::fs::write(jobs.join("broken.toml"), "command = = =\n").unwrap();
    let before = std::fs::read_to_string(jobs.join("broken.toml")).unwrap();

    let out = nj(tmp.path(), &["enable", "broken"]);
    assert!(!out.status.success());
    assert_eq!(
        std::fs::read_to_string(jobs.join("broken.toml")).unwrap(),
        before,
        "a file we failed to read must not be rewritten"
    );
}

#[test]
fn rm_names_it_and_leaves_directory_untouched_when_job_is_unknown() {
    let tmp = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(tmp.path().join("jobs")).unwrap();
    let out = nj(tmp.path(), &["rm", "nope"]);
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("nope"));
}

fn write_script(dir: &Path, name: &str, body: &str) -> std::path::PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let path = dir.join(name);
    std::fs::write(&path, body).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    path
}

fn nj_with_editor(home: &Path, editor: &Path, args: &[&str]) -> std::process::Output {
    Command::new(bin())
        .args(args)
        .env("NIGHTJAR_HOME", home)
        .env("EDITOR", editor)
        .output()
        .unwrap()
}

#[test]
fn edit_saves_valid_change_made_by_editor() {
    let tmp = tempfile::tempdir().unwrap();
    let jobs = tmp.path().join("jobs");
    std::fs::create_dir_all(&jobs).unwrap();
    std::fs::write(
        jobs.join("j.toml"),
        "command = \"echo old\"\nschedule = \"hourly\"\n",
    )
    .unwrap();

    let editor = write_script(
        tmp.path(),
        "fake-editor.sh",
        "#!/bin/sh\nprintf 'command = \"echo new\"\\nschedule = \"hourly\"\\n' > \"$1\"\n",
    );

    let out = nj_with_editor(tmp.path(), &editor, &["edit", "j"]);
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let body = std::fs::read_to_string(jobs.join("j.toml")).unwrap();
    assert!(body.contains("echo new"), "got: {body}");
}

#[test]
fn edit_reports_parse_error_but_keeps_users_broken_edit_on_disk() {
    let tmp = tempfile::tempdir().unwrap();
    let jobs = tmp.path().join("jobs");
    std::fs::create_dir_all(&jobs).unwrap();
    std::fs::write(
        jobs.join("j.toml"),
        "command = \"echo old\"\nschedule = \"hourly\"\n",
    )
    .unwrap();

    let editor = write_script(
        tmp.path(),
        "fake-editor.sh",
        "#!/bin/sh\nprintf 'command = = =\\n' > \"$1\"\n",
    );

    let out = nj_with_editor(tmp.path(), &editor, &["edit", "j"]);
    assert!(!out.status.success());
    let body = std::fs::read_to_string(jobs.join("j.toml")).unwrap();
    assert_eq!(body, "command = = =\n", "the edit must survive: {body}");
}

#[test]
fn edit_fails_and_names_editor_when_editor_is_not_set() {
    let tmp = tempfile::tempdir().unwrap();
    let jobs = tmp.path().join("jobs");
    std::fs::create_dir_all(&jobs).unwrap();
    std::fs::write(
        jobs.join("j.toml"),
        "command = \"true\"\nschedule = \"hourly\"\n",
    )
    .unwrap();

    let out = Command::new(bin())
        .args(["edit", "j"])
        .env("NIGHTJAR_HOME", tmp.path())
        .env_remove("EDITOR")
        .output()
        .unwrap();
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("EDITOR"));
}

#[test]
fn edit_never_lets_editor_touch_live_job_file() {
    let tmp = tempfile::tempdir().unwrap();
    let jobs = tmp.path().join("jobs");
    std::fs::create_dir_all(&jobs).unwrap();
    let live = jobs.join("j.toml");
    std::fs::write(&live, "command = \"echo old\"\nschedule = \"hourly\"\n").unwrap();

    let seen = tmp.path().join("seen-path");
    let editor = write_script(
        tmp.path(),
        "fake-editor.sh",
        &format!(
            "#!/bin/sh\nprintf '%s' \"$1\" > {}\n: > \"$1\"\n\
             printf 'command = \"echo new\"\\nschedule = \"hourly\"\\n' > \"$1\"\n",
            seen.display()
        ),
    );

    let out = nj_with_editor(tmp.path(), &editor, &["edit", "j"]);
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let handed = std::fs::read_to_string(&seen).unwrap();
    assert_ne!(
        Path::new(&handed),
        live.as_path(),
        "the editor was handed the live job file"
    );
    assert_eq!(
        std::fs::read_to_string(&live).unwrap(),
        "command = \"echo new\"\nschedule = \"hourly\"\n",
        "the edit must still land on the live file"
    );

    let strays: Vec<String> = std::fs::read_dir(tmp.path().join("."))
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .filter(|n| n.starts_with(".edit-"))
        .collect();
    assert!(strays.is_empty(), "scratch copy left behind: {strays:?}");
}
