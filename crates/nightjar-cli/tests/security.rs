use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_nightjar")
}

fn setup_with_resolver(job_name: &str, job_body: &str) -> tempfile::TempDir {
    let tmp = tempfile::tempdir().unwrap();
    let jobs = tmp.path().join("jobs");
    std::fs::create_dir_all(&jobs).unwrap();
    std::fs::write(jobs.join(format!("{job_name}.toml")), job_body).unwrap();
    std::fs::write(
        tmp.path().join("config.toml"),
        "[secrets]\nresolver = \"echo {}\"\n",
    )
    .unwrap();
    tmp
}

fn run_cmd(home: &Path, args: &[&str]) -> std::process::Output {
    Command::new(bin())
        .args(args)
        .env("NIGHTJAR_HOME", home)
        .output()
        .unwrap()
}

const SECRET: &str = "e2e-redaction-secret-9f2c";
const MARKER: &str = "[nightjar:redacted]";

fn job_toml(command: &str) -> String {
    format!("command = \"{command}\"\nschedule = \"hourly\"\n\n[secrets]\nTOKEN = \"{SECRET}\"\n")
}

#[test]
fn secret_is_redacted_from_json_output_and_from_logs() {
    let job = "jsonsecret";
    let tmp = setup_with_resolver(job, &job_toml("echo token=$TOKEN"));

    let exec = run_cmd(
        tmp.path(),
        &[
            "exec",
            "--job",
            job,
            "--run",
            "e2e-run-1",
            "--trigger",
            "manual",
        ],
    );
    assert!(
        exec.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&exec.stderr)
    );

    let text = run_cmd(tmp.path(), &["logs", job]);
    assert!(text.status.success());
    let stdout_text = String::from_utf8_lossy(&text.stdout);
    assert!(!stdout_text.contains(SECRET), "got: {stdout_text}");
    assert!(stdout_text.contains(MARKER), "got: {stdout_text}");

    let json = run_cmd(tmp.path(), &["logs", job, "--json"]);
    assert!(json.status.success());
    let raw = String::from_utf8_lossy(&json.stdout);
    assert!(
        !raw.contains(SECRET),
        "the secret must not appear anywhere in --json output; got: {raw}"
    );
    let parsed: serde_json::Value = serde_json::from_str(&raw).unwrap();
    let stdout_field = parsed["stdout"].as_str().unwrap();
    assert!(!stdout_field.contains(SECRET), "got: {stdout_field}");
    assert!(stdout_field.contains(MARKER), "got: {stdout_field}");
}

#[test]
fn secret_is_redacted_from_stderr_in_logs_and_json() {
    let job = "jsonsecreterr";
    let tmp = setup_with_resolver(job, &job_toml("echo token=$TOKEN 1>&2"));

    let exec = run_cmd(
        tmp.path(),
        &[
            "exec",
            "--job",
            job,
            "--run",
            "e2e-run-2",
            "--trigger",
            "manual",
        ],
    );
    assert!(
        exec.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&exec.stderr)
    );

    let json = run_cmd(tmp.path(), &["logs", job, "--json"]);
    assert!(json.status.success());
    let raw = String::from_utf8_lossy(&json.stdout);
    let parsed: serde_json::Value = serde_json::from_str(&raw).unwrap();
    let stderr_field = parsed["stderr"].as_str().unwrap();
    assert!(!stderr_field.contains(SECRET), "got: {stderr_field}");
    assert!(stderr_field.contains(MARKER), "got: {stderr_field}");
}

#[test]
fn resolver_failure_message_is_visible_in_status_and_logs_not_only_sqlite() {
    let job = "resolverfails";
    let tmp = setup_with_resolver(
        job,
        "command = \"true\"\nschedule = \"hourly\"\n\n[secrets]\nTOKEN = \"whatever\"\n",
    );
    std::fs::write(
        tmp.path().join("config.toml"),
        "[secrets]\nresolver = \"exit 1 #{}\"\n",
    )
    .unwrap();

    let exec = run_cmd(
        tmp.path(),
        &[
            "exec",
            "--job",
            job,
            "--run",
            "e2e-run-message",
            "--trigger",
            "manual",
        ],
    );
    assert!(
        !exec.status.success(),
        "a broken resolver must fail the run"
    );

    let status = run_cmd(tmp.path(), &["status", job]);
    let status_text = String::from_utf8_lossy(&status.stdout);
    assert!(
        status_text.contains("resolver exited"),
        "the status table must show why the run failed; got: {status_text}"
    );

    let logs = run_cmd(tmp.path(), &["logs", job]);
    let logs_text = String::from_utf8_lossy(&logs.stdout) + String::from_utf8_lossy(&logs.stderr);
    assert!(logs_text.contains("resolver exited"), "got: {logs_text}");

    let logs_json = run_cmd(tmp.path(), &["logs", job, "--json"]);
    let raw = String::from_utf8_lossy(&logs_json.stdout);
    let parsed: serde_json::Value = serde_json::from_str(&raw).unwrap();
    assert!(
        parsed["run"]["message"]
            .as_str()
            .unwrap_or_default()
            .contains("resolver exited"),
        "got: {raw}"
    );
}

const FAKE_SSH: &str = r#"#!/bin/sh
: > "$SSH_ARGV_LOG"
for a in "$@"; do printf '%s\n' "$a" >> "$SSH_ARGV_LOG"; done

host=""
host_found=0
rest=""
while [ $# -gt 0 ]; do
  a="$1"
  if [ "$host_found" = 0 ]; then
    case "$a" in
      --)
        shift
        host="$1"
        shift
        host_found=1
        continue
        ;;
      -o)
        shift 2
        continue
        ;;
      -o*)
        shift
        continue
        ;;
      -*)
        shift
        continue
        ;;
      *)
        host="$a"
        shift
        host_found=1
        continue
        ;;
    esac
  fi
  if [ -z "$rest" ]; then rest="$a"; else rest="$rest $a"; fi
  shift
done

echo "RESOLVED_HOST=$host" >> "$SSH_ARGV_LOG"
exec sh -c "$rest"
"#;

fn fake_ssh_dir() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    let script = dir.path().join("ssh");
    std::fs::write(&script, FAKE_SSH).unwrap();
    let mut perms = std::fs::metadata(&script).unwrap().permissions();
    perms.set_mode(0o755);
    std::fs::set_permissions(&script, perms).unwrap();
    dir
}

fn nj_with_fake_ssh(
    fake_bin_dir: &Path,
    home: &Path,
    log: &Path,
    args: &[&str],
) -> std::process::Output {
    let real_bin_dir = PathBuf::from(bin())
        .parent()
        .expect("CARGO_BIN_EXE_nightjar has a parent directory")
        .to_path_buf();
    let path = format!(
        "{}:{}:{}",
        fake_bin_dir.display(),
        real_bin_dir.display(),
        std::env::var("PATH").unwrap_or_default(),
    );
    Command::new(bin())
        .args(args)
        .env("NIGHTJAR_HOME", home)
        .env("PATH", path)
        .env("SSH_ARGV_LOG", log)
        .output()
        .unwrap()
}

fn home_with_empty_jobs_dir() -> tempfile::TempDir {
    let home = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(home.path().join("jobs")).unwrap();
    home
}

#[test]
fn host_string_is_refused_before_ssh_ever_runs_when_it_looks_like_an_ssh_option() {
    let fake_bin_dir = fake_ssh_dir();
    let home = home_with_empty_jobs_dir();
    let log = tempfile::NamedTempFile::new().unwrap();
    let hostile_host = "-oProxyCommand=true";

    let out = nj_with_fake_ssh(
        fake_bin_dir.path(),
        home.path(),
        log.path(),
        &["--host", hostile_host, "status"],
    );

    assert!(!out.status.success(), "the hostile host must be refused");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("cannot start with"),
        "got stderr:\n{stderr}"
    );
    let logged = std::fs::read_to_string(log.path()).unwrap();
    assert!(logged.is_empty(), "log not empty: {logged}");
}

#[test]
fn job_name_reaches_remote_nightjar_as_one_argument_when_it_contains_a_space() {
    let fake_bin_dir = fake_ssh_dir();
    let home = home_with_empty_jobs_dir();
    let log = tempfile::NamedTempFile::new().unwrap();

    let out = nj_with_fake_ssh(
        fake_bin_dir.path(),
        home.path(),
        log.path(),
        &["--host", "anyhost", "logs", "my backup", "--json"],
    );

    let stdout = String::from_utf8_lossy(&out.stdout);
    let merged: serde_json::Value = serde_json::from_str(&stdout).unwrap_or_else(|e| {
        panic!(
            "merged output must be valid JSON once the job name survives \
             intact; got {e} on stdout:\n{stdout}\nstderr:\n{}",
            String::from_utf8_lossy(&out.stderr)
        )
    });
    assert_eq!(merged["hosts"][0]["ok"], true, "got: {merged}");
    assert_eq!(merged["hosts"][0]["job"], "my backup", "got: {merged}");
}

#[test]
fn run_value_cannot_execute_command_on_remote_shell() {
    let fake_bin_dir = fake_ssh_dir();
    let home = home_with_empty_jobs_dir();
    let log = tempfile::NamedTempFile::new().unwrap();
    let marker = home.path().join("pwned-marker");

    let run_value = format!("`touch {}`", marker.display());
    let out = nj_with_fake_ssh(
        fake_bin_dir.path(),
        home.path(),
        log.path(),
        &[
            "--host", "anyhost", "logs", "backup", "--run", &run_value, "--json",
        ],
    );

    assert!(
        !marker.exists(),
        "stdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
    );
}
