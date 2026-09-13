use std::fmt::Write as _;
use std::io::Write;
use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result};
use jiff::Timestamp;
use nightjar_core::clock::{Clock, SystemClock};
use nightjar_core::format::{duration_human, relative_time};
use nightjar_core::guidance::no_such_job;
use nightjar_core::paths::Paths;
use nightjar_remote::HostResult;
use nightjar_store::Store;
use nightjar_store::run::{Run, RunStatus};
use owo_colors::{OwoColorize, Stream};
use serde_json::{Value, json};

use crate::merged::{self, HostPayload, HostView};
use crate::read_captured;

const FOLLOW_POLL: Duration = Duration::from_millis(150);

enum RunLookup {
    Found(Box<Run>),
    JobHasNoRunsYet,
    NoSuchJob,
    NoSuchRun { requested: String },
    RunBelongsToAnotherJob { requested: String, owner: String },
}

fn look_up_run(store: &Store, paths: &Paths, job: &str, run_id: Option<&str>) -> Result<RunLookup> {
    Ok(match run_id {
        Some(requested) => match store.get_run(requested)? {
            Some(run) if run.job == job => RunLookup::Found(Box::new(run)),
            Some(run) => RunLookup::RunBelongsToAnotherJob {
                requested: requested.to_string(),
                owner: run.job,
            },
            None => RunLookup::NoSuchRun {
                requested: requested.to_string(),
            },
        },
        None => match store.last_run(job)? {
            Some(run) => RunLookup::Found(Box::new(run)),
            None if job_file_exists(paths, job) => RunLookup::JobHasNoRunsYet,
            None => RunLookup::NoSuchJob,
        },
    })
}

fn job_file_exists(paths: &Paths, job: &str) -> bool {
    paths.job_file(job).is_ok_and(|path| path.exists())
}

/// # Errors
/// fails if the store cannot be opened, or a capture file cannot be read
pub fn cmd_logs(
    job: &str,
    run_id: Option<&str>,
    lines: Option<usize>,
    follow: bool,
    json: bool,
) -> Result<i32> {
    let paths = Paths::resolve()?;
    let store = Store::open(&paths.db_path)?;

    match look_up_run(&store, &paths, job, run_id)? {
        RunLookup::Found(run) => {
            if json {
                emit_json(&run, lines)
            } else {
                emit_text(&run, lines, follow, &store, SystemClock.now())
            }
        }
        RunLookup::JobHasNoRunsYet => {
            if json {
                println!(
                    "{}",
                    json!({ "schema": merged::SCHEMA_VERSION, "job": job, "run": null })
                );
            } else {
                println!("no runs recorded for {job} yet; run `nightjar run {job}` to try it now");
            }
            Ok(0)
        }
        RunLookup::NoSuchJob => {
            merged::refuse_as_not_found(&no_such_job(job, &paths.job_file(job)?), json)
        }
        RunLookup::NoSuchRun { requested } => {
            merged::refuse_as_not_found(&format!("no such run: {requested}"), json)
        }
        RunLookup::RunBelongsToAnotherJob { requested, owner } => merged::refuse_as_not_found(
            &format!("run {requested} belongs to job {owner:?}, not {job:?}"),
            json,
        ),
    }
}

fn emit_text(
    run: &Run,
    lines: Option<usize>,
    follow: bool,
    store: &Store,
    now: Timestamp,
) -> Result<i32> {
    aside(&headline(run, now));

    let out_len = write_tail(run.stdout_path.as_deref(), lines, &mut std::io::stdout())?;
    let err_len = write_tail(run.stderr_path.as_deref(), lines, &mut std::io::stderr())?;

    if let Some(message) = &run.message {
        eprintln!("{message}");
    }

    let streaming = follow && run.status == RunStatus::Running;
    if out_len == 0 && err_len == 0 && !streaming {
        aside(silence(run.status));
    }

    if streaming {
        follow_run(store, run, out_len, err_len)?;
    }
    Ok(0)
}

fn aside(line: &str) {
    eprintln!("{}", line.if_supports_color(Stream::Stderr, |t| t.dimmed()));
}

fn headline(run: &Run, now: Timestamp) -> String {
    format!(
        "{} {}, {} (run {})",
        run.job,
        outcome(run),
        when(run, now),
        run.id
    )
}

fn outcome(run: &Run) -> String {
    match run.status {
        RunStatus::Running => "is still running".to_string(),
        RunStatus::Success => format!("succeeded{}", took(run, "in")),
        RunStatus::Failure => format!("failed{}{}", exit_status(run), took(run, "after")),
        RunStatus::Timeout => format!("timed out{}", took(run, "after")),
        RunStatus::Limit => format!("was killed for outgrowing its limits{}", took(run, "after")),
        RunStatus::Missed => "never ran".to_string(),
        RunStatus::Unknown => format!("ended without a recorded outcome{}", took(run, "after")),
    }
}

fn exit_status(run: &Run) -> String {
    run.exit_code
        .map(|code| format!(" with exit {code}"))
        .unwrap_or_default()
}

fn took(run: &Run, preposition: &str) -> String {
    run.duration_ms
        .map(|ms| format!(" {preposition} {}", duration_human(ms)))
        .unwrap_or_default()
}

fn when(run: &Run, now: Timestamp) -> String {
    run.finished_at.map_or_else(
        || format!("started {}", relative_time(run.started_at, now)),
        |finished| relative_time(finished, now),
    )
}

const fn silence(status: RunStatus) -> &'static str {
    match status {
        RunStatus::Running => "nothing printed yet",
        RunStatus::Missed => "no output, because the run never started",
        _ => "the run printed nothing",
    }
}

fn write_tail(path: Option<&Path>, max_lines: Option<usize>, out: &mut dyn Write) -> Result<u64> {
    let Some(capture) = path else { return Ok(0) };
    let Some(bytes) = read_captured(capture)? else {
        return Ok(0);
    };
    let captured_len = bytes.len() as u64;
    match max_lines {
        Some(max_lines) => out.write_all(&last_n_lines(&bytes, max_lines))?,
        None => out.write_all(&bytes)?,
    }
    Ok(captured_len)
}

fn last_n_lines(bytes: &[u8], max_lines: usize) -> Vec<u8> {
    if max_lines == 0 {
        return Vec::new();
    }
    let lines: Vec<&[u8]> = bytes.split_inclusive(|&b| b == b'\n').collect();
    let start = lines.len().saturating_sub(max_lines);
    lines[start..].concat()
}

fn follow_run(store: &Store, run: &Run, out_from: u64, err_from: u64) -> Result<()> {
    let mut out_at = out_from;
    let mut err_at = err_from;
    loop {
        std::thread::sleep(FOLLOW_POLL);
        out_at = tail_new_bytes(run.stdout_path.as_deref(), out_at, &mut std::io::stdout())?;
        err_at = tail_new_bytes(run.stderr_path.as_deref(), err_at, &mut std::io::stderr())?;

        let still_running = matches!(
            store.get_run(&run.id)?,
            Some(latest) if latest.status == RunStatus::Running
        );
        if !still_running {
            tail_new_bytes(run.stdout_path.as_deref(), out_at, &mut std::io::stdout())?;
            tail_new_bytes(run.stderr_path.as_deref(), err_at, &mut std::io::stderr())?;
            return Ok(());
        }
    }
}

fn tail_new_bytes(path: Option<&Path>, from: u64, out: &mut dyn Write) -> Result<u64> {
    use std::io::{Read, Seek, SeekFrom};

    let Some(capture) = path else { return Ok(from) };
    let mut file = match std::fs::File::open(capture) {
        Ok(file) => file,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(from),
        Err(e) => return Err(e).with_context(|| format!("reading {}", capture.display())),
    };
    let capture_len = file
        .metadata()
        .with_context(|| format!("reading {}", capture.display()))?
        .len();
    if capture_len <= from {
        return Ok(from);
    }
    file.seek(SeekFrom::Start(from))?;
    let mut fresh = Vec::new();
    file.read_to_end(&mut fresh)?;
    out.write_all(&fresh)?;
    Ok(from + fresh.len() as u64)
}

fn emit_json(run: &Run, lines: Option<usize>) -> Result<i32> {
    let stdout = read_tail_lossy(run.stdout_path.as_deref(), lines)?;
    let stderr = read_tail_lossy(run.stderr_path.as_deref(), lines)?;

    println!(
        "{}",
        json!({
            "schema": merged::SCHEMA_VERSION,
            "job": run.job,
            "run": {
                "id": run.id,
                "trigger": run.trigger.to_db_string(),
                "status": run.status.as_str(),
                "exit_code": run.exit_code,
                "started_ms": run.started_at.as_millisecond(),
                "finished_ms": run.finished_at.map(jiff::Timestamp::as_millisecond),
                "duration_ms": run.duration_ms,
                "message": run.message,
            },
            "stdout": stdout,
            "stderr": stderr,
        })
    );
    Ok(0)
}

fn read_tail_lossy(path: Option<&Path>, max_lines: Option<usize>) -> Result<String> {
    let Some(capture) = path else {
        return Ok(String::new());
    };
    let Some(bytes) = read_captured(capture)? else {
        return Ok(String::new());
    };
    let tail = match max_lines {
        Some(max_lines) => last_n_lines(&bytes, max_lines),
        None => bytes,
    };
    Ok(String::from_utf8_lossy(&tail).into_owned())
}

pub(crate) fn cmd_logs_remote(results: Vec<HostResult>, local_json: bool) -> i32 {
    let views = merged::collect(results);
    let problem = merged::any_problem(&views);

    if local_json {
        println!("{}", merged::merged_json(&views));
    } else {
        let (out, err) = render_logs_text(&views);
        print!("{out}");
        eprint!("{err}");
    }
    i32::from(problem)
}

fn render_logs_text(views: &[HostView]) -> (String, String) {
    let mut out = String::new();
    let mut err = String::new();
    for view in views {
        match &view.payload {
            HostPayload::Ok(value) => append_logs_block(&mut out, &mut err, &view.host, value),
            other => {
                let label = merged::problem_label(other).unwrap_or("error");
                let _ = writeln!(out, "== {} ({label}) ==", view.host);
            }
        }
    }
    (out, err)
}

fn append_logs_block(out: &mut String, err: &mut String, host: &str, value: &Value) {
    let _ = writeln!(out, "== {host} ==");
    if let Some(message) = value.get("error").and_then(Value::as_str) {
        let _ = writeln!(out, "{message}");
        return;
    }
    let Some(run) = value.get("run").filter(|run| !run.is_null()) else {
        let _ = writeln!(out, "no runs recorded");
        return;
    };
    let _ = writeln!(out, "{}", remote_outcome(run));

    let stdout = value.get("stdout").and_then(Value::as_str).unwrap_or("");
    let stderr = value.get("stderr").and_then(Value::as_str).unwrap_or("");
    if stdout.is_empty() && stderr.is_empty() {
        let _ = writeln!(out, "the run printed nothing");
    }
    out.push_str(stdout);
    err.push_str(stderr);
}

fn remote_outcome(run: &Value) -> String {
    let status = run
        .get("status")
        .and_then(Value::as_str)
        .unwrap_or("unrecorded");
    let exit = run
        .get("exit_code")
        .and_then(Value::as_i64)
        .map(|code| format!(", exit {code}"))
        .unwrap_or_default();
    let elapsed = run
        .get("duration_ms")
        .and_then(Value::as_i64)
        .map(|ms| format!(", {}", duration_human(ms)))
        .unwrap_or_default();
    format!("{status}{exit}{elapsed}")
}

#[cfg(test)]
mod tests {
    use nightjar_store::run::Trigger;

    use super::*;

    fn ts(s: &str) -> Timestamp {
        s.parse().unwrap()
    }

    fn finished(status: RunStatus, exit_code: Option<i32>) -> Run {
        Run {
            id: "019a4f2e-8c21-7b3a-9f01-0c2d4e6a8b10".to_string(),
            job: "sync-photos".to_string(),
            trigger: Trigger::Schedule,
            started_at: ts("2026-08-23T11:58:00Z"),
            finished_at: Some(ts("2026-08-23T11:58:01Z")),
            exit_code,
            duration_ms: Some(1200),
            status,
            pid: None,
            stdout_path: None,
            stderr_path: None,
            output_bytes: 0,
            message: None,
        }
    }

    #[test]
    fn headline_says_what_happened_when_and_which_run_it_was() {
        let run = finished(RunStatus::Failure, Some(3));
        assert_eq!(
            headline(&run, ts("2026-08-23T12:00:00Z")),
            "sync-photos failed with exit 3 after 1.2s, 1m ago (run 019a4f2e-8c21-7b3a-9f01-0c2d4e6a8b10)"
        );
    }

    #[test]
    fn every_outcome_reads_as_a_sentence_about_the_job() {
        let cases = [
            (RunStatus::Success, None, "succeeded in 1.2s"),
            (RunStatus::Failure, Some(3), "failed with exit 3 after 1.2s"),
            (RunStatus::Failure, None, "failed after 1.2s"),
            (RunStatus::Timeout, None, "timed out after 1.2s"),
            (
                RunStatus::Limit,
                None,
                "was killed for outgrowing its limits after 1.2s",
            ),
            (
                RunStatus::Unknown,
                None,
                "ended without a recorded outcome after 1.2s",
            ),
        ];
        for (status, exit_code, expected) in cases {
            assert_eq!(outcome(&finished(status, exit_code)), expected);
        }
    }

    #[test]
    fn a_run_that_never_finished_is_dated_by_when_it_started() {
        let mut run = finished(RunStatus::Running, None);
        run.finished_at = None;
        run.duration_ms = None;

        let line = headline(&run, ts("2026-08-23T12:00:00Z"));
        assert!(line.contains("is still running"), "got: {line}");
        assert!(line.contains("started 2m ago"), "got: {line}");
    }

    #[test]
    fn a_missed_run_is_never_described_as_having_taken_time() {
        let mut run = finished(RunStatus::Missed, None);
        run.duration_ms = Some(1200);
        assert_eq!(outcome(&run), "never ran");
        assert_eq!(
            silence(RunStatus::Missed),
            "no output, because the run never started"
        );
    }

    #[test]
    fn remote_outcome_survives_a_payload_that_omits_every_optional_field() {
        assert_eq!(
            remote_outcome(&json!({ "status": "failure", "exit_code": 3, "duration_ms": 1200 })),
            "failure, exit 3, 1.2s"
        );
        assert_eq!(remote_outcome(&json!({})), "unrecorded");
    }

    #[test]
    fn a_remote_run_with_no_captured_output_still_says_what_happened() {
        let views = vec![ok_view(
            "web1",
            r#"{"schema":1,"job":"backup","run":{"id":"r1","status":"failure","exit_code":3,"duration_ms":1200},"stdout":"","stderr":""}"#,
        )];

        let (out, _err) = render_logs_text(&views);
        assert!(out.contains("failure, exit 3, 1.2s"), "got: {out}");
        assert!(out.contains("the run printed nothing"), "got: {out}");
    }

    #[test]
    fn last_n_lines_keeps_only_final_n_including_partial_final_line() {
        let text = b"a\nb\nc\nd\ne\n";
        assert_eq!(last_n_lines(text, 2), b"d\ne\n");
        assert_eq!(last_n_lines(text, 0), b"");
        assert_eq!(last_n_lines(text, 100), text);
    }

    #[test]
    fn tail_new_bytes_writes_only_what_arrived_since_the_last_offset() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("r.out");
        std::fs::write(&path, b"abc").unwrap();

        let mut out = Vec::new();
        let at = tail_new_bytes(Some(&path), 0, &mut out).unwrap();
        assert_eq!((at, out.as_slice()), (3, &b"abc"[..]));

        std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap()
            .write_all(b"de")
            .unwrap();
        let mut out = Vec::new();
        let at = tail_new_bytes(Some(&path), at, &mut out).unwrap();
        assert_eq!((at, out.as_slice()), (5, &b"de"[..]));

        let mut out = Vec::new();
        let at = tail_new_bytes(Some(&path), at, &mut out).unwrap();
        assert_eq!(
            (at, out.as_slice()),
            (5, &b""[..]),
            "nothing new, nothing written"
        );
    }

    #[test]
    fn tail_new_bytes_keeps_the_offset_when_the_file_is_missing_or_shorter() {
        let tmp = tempfile::tempdir().unwrap();
        let missing = tmp.path().join("gone.out");
        let mut out = Vec::new();
        assert_eq!(tail_new_bytes(Some(&missing), 7, &mut out).unwrap(), 7);
        assert_eq!(tail_new_bytes(None, 7, &mut out).unwrap(), 7);

        let short = tmp.path().join("short.out");
        std::fs::write(&short, b"ab").unwrap();
        assert_eq!(tail_new_bytes(Some(&short), 7, &mut out).unwrap(), 7);
        assert!(out.is_empty());
    }

    #[test]
    fn last_n_lines_counts_trailing_line_with_no_newline() {
        let text = b"a\nb\nc";
        assert_eq!(last_n_lines(text, 1), b"c");
        assert_eq!(last_n_lines(text, 2), b"b\nc");
    }

    fn ok_view(host: &str, json: &str) -> HostView {
        HostView {
            host: host.to_string(),
            payload: HostPayload::Ok(serde_json::from_str(json).unwrap()),
            remote_exit_code: 0,
        }
    }

    #[test]
    fn each_hosts_output_is_labeled_so_it_cannot_be_mistaken_for_another_hosts() {
        let views = vec![
            ok_view(
                "web1",
                r#"{"schema":1,"job":"backup","run":{"id":"r1","status":"success"},"stdout":"from web1\n","stderr":""}"#,
            ),
            ok_view("web2", r#"{"schema":1,"job":"backup","run":null}"#),
        ];

        let (out, _err) = render_logs_text(&views);
        assert!(out.contains("== web1 =="), "got: {out}");
        assert!(out.contains("from web1"), "got: {out}");
        assert!(out.contains("== web2 =="), "got: {out}");
        assert!(out.contains("no runs recorded"), "got: {out}");
    }

    #[test]
    fn host_renders_labeled_block_and_flips_exit_code_when_it_is_unreachable() {
        let results = vec![
            HostResult {
                host: "web1".to_string(),
                outcome: nightjar_remote::HostOutcome::Unreachable,
            },
            HostResult {
                host: "web2".to_string(),
                outcome: nightjar_remote::HostOutcome::Success(
                    r#"{"schema":1,"job":"j","run":null}"#.to_string(),
                    0,
                ),
            },
        ];
        let views = merged::collect(results);
        assert!(merged::any_problem(&views));

        let (out, _err) = render_logs_text(&views);
        assert!(
            out.contains("web1") && out.contains("unreachable"),
            "got: {out}"
        );
    }
}
