use crate::merged::{self, HostPayload, HostView};
use crate::read_captured;
use anyhow::{Context, Result, bail};
use nightjar_core::format::json_string;
use nightjar_core::paths::Paths;
use nightjar_remote::HostResult;
use nightjar_store::Store;
use nightjar_store::run::{Run, RunStatus};
use serde_json::Value;
use std::fmt::Write as _;
use std::io::Write;
use std::path::Path;
use std::time::Duration;

/// How often `-f` re-checks for new output. Short enough to feel live.
/// Long enough to not busy-loop against the store.
const FOLLOW_POLL: Duration = Duration::from_millis(150);

enum Lookup {
    Found(Run),
    NoRuns,
    NotFound(String),
    WrongJob(String, String),
}

fn find_run(store: &Store, job: &str, run_id: Option<&str>) -> Result<Lookup> {
    Ok(match run_id {
        // `--run` names one run by id. Falling back to "newest" here would
        // return a different run's output when two runs overlap.
        Some(id) => match store.get_run(id)? {
            Some(r) if r.job == job => Lookup::Found(r),
            Some(r) => Lookup::WrongJob(id.to_string(), r.job),
            None => Lookup::NotFound(id.to_string()),
        },
        None => match store.last_run(job)? {
            Some(r) => Lookup::Found(r),
            None => Lookup::NoRuns,
        },
    })
}

pub fn cmd_logs(
    job: &str,
    run_id: Option<&str>,
    lines: Option<usize>,
    follow: bool,
    json: bool,
) -> Result<i32> {
    let paths = Paths::resolve()?;
    let store = Store::open(&paths.db_path)?;

    match find_run(&store, job, run_id)? {
        Lookup::Found(run) => {
            if json {
                emit_json(&run, lines)
            } else {
                emit_text(&run, lines, follow, &store)
            }
        }
        Lookup::NoRuns => {
            if json {
                println!(
                    r#"{{"schema":{},"job":{},"run":null}}"#,
                    merged::SCHEMA_VERSION,
                    json_string(job)
                );
            } else {
                println!("no runs recorded for {job}");
            }
            Ok(0)
        }
        Lookup::NotFound(id) => {
            if json {
                println!(
                    r#"{{"schema":{},"error":{}}}"#,
                    merged::SCHEMA_VERSION,
                    json_string(&format!("no such run: {id}"))
                );
                Ok(1)
            } else {
                bail!("no such run: {id}")
            }
        }
        Lookup::WrongJob(id, actual) => {
            let msg = format!("run {id} belongs to job {actual:?}, not {job:?}");
            if json {
                println!(
                    r#"{{"schema":{},"error":{}}}"#,
                    merged::SCHEMA_VERSION,
                    json_string(&msg)
                );
                Ok(1)
            } else {
                bail!("{msg}")
            }
        }
    }
}

fn emit_text(run: &Run, lines: Option<usize>, follow: bool, store: &Store) -> Result<i32> {
    let out_len = write_tail(run.stdout_path.as_deref(), lines, &mut std::io::stdout())?;
    let err_len = write_tail(run.stderr_path.as_deref(), lines, &mut std::io::stderr())?;

    // The one diagnostic capture files can't carry: which secret failed
    // to resolve. This goes to stderr instead of being sqlite3-only.
    if let Some(message) = &run.message {
        eprintln!("{message}");
    }

    if follow && run.status == RunStatus::Running {
        follow_run(store, run, out_len, err_len)?;
    }
    Ok(0)
}

/// The length is the full byte count, not what `max_lines` wrote.
/// `-f` starts tailing from this offset, so it never repeats bytes.
fn write_tail(path: Option<&Path>, max_lines: Option<usize>, w: &mut dyn Write) -> Result<u64> {
    let Some(p) = path else { return Ok(0) };
    let Some(bytes) = read_captured(p)? else {
        return Ok(0);
    };
    let len = bytes.len() as u64;
    match max_lines {
        Some(max_lines) => w.write_all(&last_n_lines(&bytes, max_lines))?,
        None => w.write_all(&bytes)?,
    }
    Ok(len)
}

/// A trailing line with no `\n` still counts as one line.
fn last_n_lines(bytes: &[u8], n: usize) -> Vec<u8> {
    if n == 0 {
        return Vec::new();
    }
    let all: Vec<&[u8]> = bytes.split_inclusive(|&b| b == b'\n').collect();
    let start = all.len().saturating_sub(n);
    all[start..].concat()
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
            Some(r) if r.status == RunStatus::Running
        );
        if !still_running {
            // One last drain: the row can go terminal between this poll's
            // file read and its store read.
            tail_new_bytes(run.stdout_path.as_deref(), out_at, &mut std::io::stdout())?;
            tail_new_bytes(run.stderr_path.as_deref(), err_at, &mut std::io::stderr())?;
            return Ok(());
        }
    }
}

/// Reads from `from` to the end, never the whole file: `-f` polls several
/// times a second, and re-reading a capture near `output_cap` each time
/// would cost tens of megabytes a second for a job that prints a lot.
fn tail_new_bytes(path: Option<&Path>, from: u64, w: &mut dyn Write) -> Result<u64> {
    use std::io::{Read, Seek, SeekFrom};

    let Some(p) = path else { return Ok(from) };
    let mut file = match std::fs::File::open(p) {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(from),
        Err(e) => return Err(e).with_context(|| format!("reading {}", p.display())),
    };
    let len = file
        .metadata()
        .with_context(|| format!("reading {}", p.display()))?
        .len();
    if len <= from {
        return Ok(from);
    }
    file.seek(SeekFrom::Start(from))?;
    let mut fresh = Vec::new();
    file.read_to_end(&mut fresh)?;
    w.write_all(&fresh)?;
    Ok(from + fresh.len() as u64)
}

fn emit_json(run: &Run, lines: Option<usize>) -> Result<i32> {
    let stdout = read_tail_lossy(run.stdout_path.as_deref(), lines)?;
    let stderr = read_tail_lossy(run.stderr_path.as_deref(), lines)?;

    println!(
        r#"{{"schema":{},"job":{},"run":{{"id":{},"trigger":{},"status":{},"exit_code":{},"started_ms":{},"finished_ms":{},"duration_ms":{},"message":{}}},"stdout":{},"stderr":{}}}"#,
        merged::SCHEMA_VERSION,
        json_string(&run.job),
        json_string(&run.id),
        json_string(&run.trigger.to_db_string()),
        json_string(run.status.as_str()),
        run.exit_code
            .map_or_else(|| "null".into(), |c| c.to_string()),
        run.started_at.as_millisecond(),
        run.finished_at
            .map_or_else(|| "null".into(), |t| t.as_millisecond().to_string()),
        run.duration_ms
            .map_or_else(|| "null".into(), |d| d.to_string()),
        run.message
            .as_deref()
            .map_or_else(|| "null".into(), json_string),
        json_string(&stdout),
        json_string(&stderr),
    );
    Ok(0)
}

fn read_tail_lossy(path: Option<&Path>, max_lines: Option<usize>) -> Result<String> {
    let Some(p) = path else {
        return Ok(String::new());
    };
    let Some(bytes) = read_captured(p)? else {
        return Ok(String::new());
    };
    let tail = match max_lines {
        Some(max_lines) => last_n_lines(&bytes, max_lines),
        None => bytes,
    };
    Ok(String::from_utf8_lossy(&tail).into_owned())
}

/// `--follow` is refused with `--host` before this is reached, so every
/// host's response here is already complete.
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

/// A `== host ==` header per block avoids a per-line HOST column
/// repeating itself down every line of free-text log output.
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
    if value.get("run").is_none_or(Value::is_null) {
        let _ = writeln!(out, "no runs recorded");
        return;
    }
    if let Some(stdout) = value.get("stdout").and_then(Value::as_str) {
        out.push_str(stdout);
    }
    if let Some(stderr) = value.get("stderr").and_then(Value::as_str) {
        err.push_str(stderr);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
}

#[cfg(test)]
mod remote_render_tests {
    use super::*;

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
