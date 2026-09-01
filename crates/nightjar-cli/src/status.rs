use crate::merged::{self, HostPayload, HostView};
use anyhow::Result;
use jiff::Timestamp;
use jiff::tz::TimeZone;
use nightjar_config::Job;
use nightjar_config::job::next_column;
use nightjar_config::job::{JobsDirState, probe_jobs_dir};
use nightjar_core::clock::{Clock, SystemClock};
use nightjar_core::format::{
    abbreviate_schedule, duration_human, error_summary, json_string, relative_future, relative_time,
};
use nightjar_core::paths::Paths;
use nightjar_remote::HostResult;
use nightjar_store::run::RunStatus;
use nightjar_store::{DaemonBeat, Store, overdue_since};
use owo_colors::{OwoColorize, Stream};
use serde_json::Value;
use std::fmt::Write as _;

/// 90s is three heartbeat intervals. One missed beat is scheduling
/// jitter. Three is not.
const HEARTBEAT_STALE_AFTER: i64 = 90;

/// Classified once against `now`, shared by the text and `--json`
/// renderers, so both agree on whether the daemon is alive.
pub(crate) fn daemon_state(store: &Store, now: Timestamp) -> Result<Option<(DaemonBeat, bool)>> {
    Ok(store.daemon_heartbeat()?.map(|b| {
        let age = now.as_second() - b.at.as_second();
        let stale = age > HEARTBEAT_STALE_AFTER;
        (b, stale)
    }))
}

fn daemon_line(store: &Store, now: Timestamp) -> Result<String> {
    Ok(match daemon_state(store, now)? {
        None => "no daemon has run yet — start one with `nightjar daemon`".to_string(),
        Some((b, false)) => format!("daemon running (pid {})", b.pid),
        Some((b, true)) => format!(
            "daemon not responding — last heartbeat {} (pid {})",
            relative_time(b.at, now),
            b.pid
        ),
    })
}

/// A monitoring script has no other way to learn the scheduler died.
/// Waiting for `next_ms` gives no signal until a job is overdue.
fn daemon_state_json(store: &Store, now: Timestamp) -> Result<String> {
    Ok(match daemon_state(store, now)? {
        None => r#"{"state":"never_run","heartbeat_ms":null,"pid":null}"#.to_string(),
        Some((b, stale)) => {
            let label = if stale { "not_responding" } else { "running" };
            format!(
                r#"{{"state":"{label}","heartbeat_ms":{},"pid":{}}}"#,
                b.at.as_millisecond(),
                b.pid
            )
        }
    })
}

pub fn cmd_status(job_filter: Option<&str>, json: bool) -> Result<i32> {
    let paths = Paths::resolve()?;
    let store = Store::open(&paths.db_path)?;
    let now = SystemClock.now();
    let tz = TimeZone::system();

    // Keeps each job's parse `Result` so an invalid TOML file doesn't
    // render identically to a job that has simply never run.
    let dir_state = probe_jobs_dir(&paths.jobs_dir)?;
    let jobs: Vec<(String, Result<Job>)> = match dir_state {
        JobsDirState::Missing => Vec::new(),
        JobsDirState::Present => Job::load_all(&paths.jobs_dir)
            .into_iter()
            .filter(|(n, _)| job_filter.is_none_or(|f| f == n))
            .collect(),
    };
    let any_invalid = jobs.iter().any(|(_, r)| r.is_err());

    if json {
        return render_status_json(&store, &jobs, now, &tz, any_invalid);
    }
    render_status_table(
        &store,
        &paths,
        &jobs,
        dir_state,
        job_filter,
        now,
        &tz,
        any_invalid,
    )
}

pub(crate) fn cmd_status_remote(results: Vec<HostResult>, local_json: bool) -> i32 {
    let views = merged::collect(results);
    let problem = merged::any_problem(&views);

    if local_json {
        println!("{}", merged::merged_json(&views));
    } else {
        print!("{}", render_status_text(&views, SystemClock.now()));
    }
    i32::from(problem)
}

fn render_status_text(views: &[HostView], now: Timestamp) -> String {
    let mut out = String::new();
    let _ = writeln!(
        out,
        "{:<12} {:<16} {:<20} {:<18} {:<10} {:<10} NEXT",
        "HOST", "JOB", "SCHEDULE", "LAST RUN", "STATUS", "DURATION"
    );
    for view in views {
        match &view.payload {
            HostPayload::Ok(value) => {
                // Printed once per host, not per row — a dead daemon
                // affects every job's NEXT time, not just the overdue one.
                if let Some(note) = remote_daemon_note(&view.host, value, now) {
                    let _ = writeln!(
                        out,
                        "{}",
                        note.if_supports_color(Stream::Stdout, |t| t.red())
                    );
                }
                append_status_rows(&mut out, &view.host, value, now);
            }
            other => {
                let label = merged::problem_label(other).unwrap_or("error");
                let _ = writeln!(out, "{:<12} {label}", view.host);
            }
        }
    }
    out
}

/// Mirrors local `daemon_line` wording, so a fleet view and `ssh <host>
/// nightjar status` describe the same dead daemon the same way.
fn remote_daemon_note(host: &str, value: &Value, now: Timestamp) -> Option<String> {
    let daemon = value.get("daemon")?;
    if daemon.get("state").and_then(Value::as_str)? != "not_responding" {
        return None;
    }
    let when = daemon
        .get("heartbeat_ms")
        .and_then(Value::as_i64)
        .and_then(|ms| Timestamp::from_millisecond(ms).ok())
        .map_or_else(|| "unknown".to_string(), |t| relative_time(t, now));
    Some(match daemon.get("pid").and_then(Value::as_i64) {
        Some(pid) => format!("{host}: daemon not responding — last heartbeat {when} (pid {pid})"),
        None => format!("{host}: daemon not responding — last heartbeat {when}"),
    })
}

/// Anything that fails to parse — `"invalid"`, or a value from a
/// newer schema — passes through verbatim rather than being hidden.
fn remote_status_label(raw: &str) -> String {
    match RunStatus::parse(raw) {
        Ok(RunStatus::Success) => "ok".to_string(),
        Ok(RunStatus::Running) => "…".to_string(),
        Ok(RunStatus::Timeout) => "TIMEOUT".to_string(),
        Ok(RunStatus::Unknown) => "UNKNOWN".to_string(),
        Ok(RunStatus::Missed) => "MISSED".to_string(),
        Ok(RunStatus::Failure) => "FAIL".to_string(),
        Ok(RunStatus::Limit) => "LIMIT".to_string(),
        Err(_) => raw.to_string(),
    }
}

fn append_status_rows(out: &mut String, host: &str, value: &Value, now: Timestamp) {
    let jobs = value
        .get("jobs")
        .and_then(Value::as_array)
        .map_or(&[][..], Vec::as_slice);
    if jobs.is_empty() {
        let _ = writeln!(out, "{host:<12} (no jobs)");
        return;
    }
    for job in jobs {
        let name = job.get("job").and_then(Value::as_str).unwrap_or("?");
        let schedule = job
            .get("schedule")
            .and_then(Value::as_str)
            .map_or_else(|| "—".to_string(), abbreviate_schedule);
        let status = job
            .get("status")
            .and_then(Value::as_str)
            .map_or_else(|| "—".to_string(), remote_status_label);
        let last_run = job
            .get("last_run_ms")
            .and_then(Value::as_i64)
            .and_then(|ms| Timestamp::from_millisecond(ms).ok())
            .map_or_else(|| "never".to_string(), |t| relative_time(t, now));
        let duration = job
            .get("duration_ms")
            .and_then(Value::as_i64)
            .map_or_else(|| "—".to_string(), duration_human);
        // A live-computed next time looks plausible even when the daemon
        // is dead, which is exactly the overdue illusion. No timestamp
        // for *when* it went overdue is on the wire, so this shows the
        // flag alone.
        let next = if job.get("overdue").and_then(Value::as_bool).unwrap_or(false) {
            "OVERDUE"
                .if_supports_color(Stream::Stdout, |t| t.red())
                .to_string()
        } else {
            job.get("next_ms")
                .and_then(Value::as_i64)
                .and_then(|ms| Timestamp::from_millisecond(ms).ok())
                .map_or_else(|| "—".to_string(), |t| relative_future(t, now))
        };
        let _ = writeln!(
            out,
            "{host:<12} {name:<16} {schedule:<20} {last_run:<18} {status:<10} {duration:<10} {next}"
        );
    }
}

fn render_status_json(
    store: &Store,
    jobs: &[(String, Result<Job>)],
    now: Timestamp,
    tz: &TimeZone,
    any_invalid: bool,
) -> Result<i32> {
    let mut rows = Vec::new();
    for (name, loaded) in jobs {
        let row = match loaded {
            // An invalid job has no schedule to be overdue against.
            Err(e) => format!(
                r#"{{"job":{},"schedule":null,"status":"invalid","error":{},"exit_code":null,"duration_ms":null,"last_run_ms":null,"next_ms":null,"overdue":false}}"#,
                json_string(name),
                json_string(&error_summary(e)),
            ),
            Ok(job) => {
                let last = store.last_run(name)?;
                let state = store.job_state(name)?;
                let overdue = overdue_since(state.as_ref(), last.as_ref(), now).is_some();
                // `Ok(None)` is reachable: jiff-cron's search is bounded
                // at year 2100. Both it and a computation error collapse
                // to `null` here.
                let next_ms = match &job.schedule {
                    Some(s) => match s.next_after(now, tz) {
                        Ok(Some(t)) => Some(t.as_millisecond()),
                        Ok(None) | Err(_) => None,
                    },
                    None => None,
                };
                let schedule_json = job
                    .schedule_source()
                    .map_or_else(|| "null".into(), json_string);
                format!(
                    r#"{{"job":{},"schedule":{},"status":{},"error":null,"exit_code":{},"duration_ms":{},"last_run_ms":{},"next_ms":{},"overdue":{overdue}}}"#,
                    json_string(name),
                    schedule_json,
                    last.as_ref()
                        .map_or_else(|| "null".into(), |r| json_string(r.status.as_str())),
                    last.as_ref()
                        .and_then(|r| r.exit_code)
                        .map_or_else(|| "null".into(), |c| c.to_string()),
                    last.as_ref()
                        .and_then(|r| r.duration_ms)
                        .map_or_else(|| "null".into(), |d| d.to_string()),
                    last.as_ref().map_or_else(
                        || "null".into(),
                        |r| r.started_at.as_millisecond().to_string()
                    ),
                    next_ms.map_or_else(|| "null".into(), |n| n.to_string()),
                )
            }
        };
        rows.push(row);
    }
    let daemon = daemon_state_json(store, now)?;
    println!(
        r#"{{"schema":{},"daemon":{daemon},"jobs":[{}]}}"#,
        merged::SCHEMA_VERSION,
        rows.join(",")
    );
    Ok(i32::from(any_invalid))
}

#[allow(clippy::too_many_arguments)] // one render call, not a public API worth a params struct
fn render_status_table(
    store: &Store,
    paths: &Paths,
    jobs: &[(String, Result<Job>)],
    dir_state: JobsDirState,
    job_filter: Option<&str>,
    now: Timestamp,
    tz: &TimeZone,
    any_invalid: bool,
) -> Result<i32> {
    if matches!(dir_state, JobsDirState::Missing) {
        println!(
            "no jobs directory yet; create one at {}",
            paths.jobs_dir.display()
        );
        return Ok(0);
    }

    if jobs.is_empty() {
        match job_filter {
            Some(f) => println!("no such job: {f}"),
            None => println!(
                "no jobs configured yet (add a .toml file to {})",
                paths.jobs_dir.display()
            ),
        }
        return Ok(0);
    }

    println!("{}", daemon_line(store, now)?);

    let header = format!(
        "{:<16} {:<20} {:<18} {:<8} {:<10} {}",
        "JOB", "SCHEDULE", "LAST RUN", "EXIT", "DURATION", "NEXT"
    );
    println!("{}", header.if_supports_color(Stream::Stdout, |t| t.bold()));

    for (name, loaded) in jobs {
        let job = match loaded {
            Err(e) => {
                let padded = format!("{:<8}", "invalid");
                let invalid = padded.if_supports_color(Stream::Stdout, |t| t.red());
                println!(
                    "{name:<16} {:<20} {:<18} {invalid} {:<10} {:<9} {}",
                    "—",
                    "—",
                    "—",
                    "—",
                    error_summary(e)
                );
                continue;
            }
            Ok(j) => j,
        };
        let schedule = job
            .schedule_source()
            .map_or_else(|| "—".to_string(), abbreviate_schedule);
        let state = store.job_state(name)?;
        let last = store.last_run(name)?;

        // An overdue job's NEXT cell replaces the live-computed
        // projection — that looks plausible even when nothing is
        // watching the clock.
        let next = if let Some(since) = overdue_since(state.as_ref(), last.as_ref(), now) {
            let plain = format!("OVERDUE {}", relative_time(since, now));
            plain
                .if_supports_color(Stream::Stdout, |t| t.red())
                .to_string()
        } else {
            next_column(job, tz, now)
        };

        match last {
            None => println!(
                "{name:<16} {schedule:<20} {:<18} {:<8} {:<10} {next}",
                "never", "—", "—"
            ),
            Some(r) => {
                let when = relative_time(r.started_at, now);

                // Pad first, then colorize — ANSI bytes count toward
                // field width and would misalign colored rows.
                let plain = match r.status {
                    RunStatus::Success => "ok",
                    RunStatus::Running => "…",
                    RunStatus::Timeout => "TIMEOUT",
                    RunStatus::Unknown => "UNKNOWN",
                    RunStatus::Missed => "MISSED",
                    RunStatus::Failure => "FAIL",
                    RunStatus::Limit => "LIMIT",
                };
                let padded = format!("{plain:<8}");
                let exit = match r.status {
                    RunStatus::Success => padded
                        .if_supports_color(Stream::Stdout, |t| t.green())
                        .to_string(),
                    RunStatus::Running => padded,
                    RunStatus::Timeout => padded
                        .if_supports_color(Stream::Stdout, |t| t.red())
                        .to_string(),
                    RunStatus::Unknown => padded
                        .if_supports_color(Stream::Stdout, |t| t.yellow())
                        .to_string(),
                    RunStatus::Missed => padded
                        .if_supports_color(Stream::Stdout, |t| t.yellow())
                        .to_string(),
                    RunStatus::Failure | RunStatus::Limit => padded
                        .if_supports_color(Stream::Stdout, |t| t.red())
                        .to_string(),
                };

                let dur = r.duration_ms.map_or_else(|| "—".into(), duration_human);
                let mut line =
                    format!("{name:<16} {schedule:<20} {when:<18} {exit} {dur:<10} {next}");
                // The one diagnostic `exit_code` can't carry: which secret
                // failed to resolve. Otherwise it's sqlite3-only.
                if let Some(message) = &r.message {
                    let _ = write!(line, "  ({message})");
                }
                println!("{line}");
            }
        }
    }
    Ok(i32::from(any_invalid))
}

#[cfg(test)]
mod next_column_tests {
    use super::*;
    use nightjar_config::{Catchup, OnFailure, Overlap};
    use std::collections::BTreeMap;

    fn job_with_schedule(schedule: &str) -> Job {
        Job {
            name: "t".into(),
            command: "true".into(),
            schedule: Some(nightjar_schedule::Schedule::parse(schedule).unwrap()),
            after: None,
            timeout: None,
            limits: nightjar_config::Limits::default(),
            catchup: Catchup::Once,
            overlap: Overlap::Skip,
            workdir: None,
            enabled: true,
            shell: None,
            login_shell: Some(false),
            env: BTreeMap::new(),
            secrets: BTreeMap::new(),
            on_failure: OnFailure::default(),
            warnings: Vec::new(),
        }
    }

    #[test]
    fn next_column_renders_none_not_never_or_dash_when_schedule_has_no_calendar_match() {
        let job = job_with_schedule("0 0 30 2 *");
        let tz = TimeZone::system();
        let now: Timestamp = "2026-06-01T00:00:00Z".parse().unwrap();
        assert_eq!(next_column(&job, &tz, now), "none");
    }

    #[test]
    fn next_column_renders_relative_time_when_schedule_is_normal() {
        let job = job_with_schedule("hourly");
        let tz = TimeZone::system();
        let now: Timestamp = "2026-06-01T00:00:00Z".parse().unwrap();
        assert_eq!(next_column(&job, &tz, now), "in 1h");
    }

    #[test]
    fn next_column_shows_what_it_waits_for_instead_of_schedule_when_job_is_triggered() {
        let mut job = job_with_schedule("hourly");
        job.schedule = None;
        job.after = Some("backup".to_string());

        let tz = TimeZone::system();
        let now: Timestamp = "2026-06-01T00:00:00Z".parse().unwrap();
        assert_eq!(next_column(&job, &tz, now), "after backup");
    }
}

#[cfg(test)]
mod daemon_line_tests {
    use super::*;

    fn ts(s: &str) -> Timestamp {
        s.parse().unwrap()
    }

    #[test]
    fn daemon_line_says_so_when_no_heartbeat_has_ever_been_written() {
        let store = Store::open_in_memory().unwrap();
        let now = ts("2026-06-01T00:00:00Z");
        assert!(daemon_line(&store, now).unwrap().contains("no daemon"));
    }

    #[test]
    fn daemon_line_reads_as_running_when_heartbeat_is_fresh() {
        let store = Store::open_in_memory().unwrap();
        let now = ts("2026-06-01T00:00:00Z");
        store.write_heartbeat(now, 123, "0.1.0").unwrap();
        let line = daemon_line(&store, now).unwrap();
        assert!(line.contains("running"), "got: {line}");
        assert!(!line.contains("not responding"), "got: {line}");
    }

    #[test]
    fn daemon_line_still_reads_as_running_when_heartbeat_is_exactly_at_stale_threshold() {
        let store = Store::open_in_memory().unwrap();
        let beat_at = ts("2026-06-01T00:00:00Z");
        let now = beat_at + jiff::Span::new().seconds(HEARTBEAT_STALE_AFTER);
        store.write_heartbeat(beat_at, 123, "0.1.0").unwrap();
        let line = daemon_line(&store, now).unwrap();
        assert!(
            line.contains("running") && !line.contains("not responding"),
            "a heartbeat exactly at the threshold is still within tolerance; got: {line}"
        );
    }

    #[test]
    fn daemon_line_reads_as_not_responding_when_heartbeat_is_one_second_past_stale_threshold() {
        let store = Store::open_in_memory().unwrap();
        let beat_at = ts("2026-06-01T00:00:00Z");
        let now = beat_at + jiff::Span::new().seconds(HEARTBEAT_STALE_AFTER + 1);
        store.write_heartbeat(beat_at, 123, "0.1.0").unwrap();
        let line = daemon_line(&store, now).unwrap();
        assert!(line.contains("not responding"), "got: {line}");
    }

    #[test]
    fn daemon_line_does_not_panic_and_reads_as_running_when_heartbeat_is_from_future() {
        let store = Store::open_in_memory().unwrap();
        let now = ts("2026-06-01T00:00:00Z");
        let beat_at = now + jiff::Span::new().minutes(10);
        store.write_heartbeat(beat_at, 123, "0.1.0").unwrap();
        let line = daemon_line(&store, now).unwrap();
        assert!(
            line.contains("running") && !line.contains("not responding"),
            "got: {line}"
        );
    }
}

#[cfg(test)]
mod daemon_state_json_tests {
    use super::*;

    fn ts(s: &str) -> Timestamp {
        s.parse().unwrap()
    }

    #[test]
    fn daemon_state_json_serialises_null_fields_when_daemon_has_never_run() {
        let store = Store::open_in_memory().unwrap();
        let now = ts("2026-06-01T00:00:00Z");
        assert_eq!(
            daemon_state_json(&store, now).unwrap(),
            r#"{"state":"never_run","heartbeat_ms":null,"pid":null}"#
        );
    }

    #[test]
    fn daemon_state_json_reports_heartbeat_and_pid_when_daemon_is_running() {
        let store = Store::open_in_memory().unwrap();
        let now = ts("2026-06-01T00:00:00Z");
        store.write_heartbeat(now, 4242, "0.1.0").unwrap();
        let s = daemon_state_json(&store, now).unwrap();
        assert!(s.contains(r#""state":"running""#), "got: {s}");
        assert!(
            s.contains(&format!(r#""heartbeat_ms":{}"#, now.as_millisecond())),
            "got: {s}"
        );
        assert!(s.contains(r#""pid":4242"#), "got: {s}");
    }

    #[test]
    fn daemon_state_json_reports_not_responding_when_heartbeat_is_stale() {
        let store = Store::open_in_memory().unwrap();
        let beat_at = ts("2026-06-01T00:00:00Z");
        let now = beat_at + jiff::Span::new().seconds(HEARTBEAT_STALE_AFTER + 1);
        store.write_heartbeat(beat_at, 4242, "0.1.0").unwrap();
        let s = daemon_state_json(&store, now).unwrap();
        assert!(s.contains(r#""state":"not_responding""#), "got: {s}");
    }
}

#[cfg(test)]
mod remote_render_tests {
    use super::*;
    use nightjar_remote::HostOutcome;

    fn ts(s: &str) -> Timestamp {
        s.parse().unwrap()
    }

    fn ok_view(host: &str, json: &str) -> HostView {
        HostView {
            host: host.to_string(),
            payload: HostPayload::Ok(serde_json::from_str(json).unwrap()),
            remote_exit_code: 0,
        }
    }

    #[test]
    fn merged_output_gains_host_column() {
        let now = ts("2026-06-01T00:00:00Z");
        let views = vec![
            ok_view(
                "web1",
                r#"{"schema":1,"jobs":[{"job":"backup","schedule":"hourly","status":"success","exit_code":0,"duration_ms":1200,"last_run_ms":0,"next_ms":null}]}"#,
            ),
            ok_view(
                "web2",
                r#"{"schema":1,"jobs":[{"job":"backup","schedule":"hourly","status":"failure","exit_code":1,"duration_ms":300,"last_run_ms":0,"next_ms":null}]}"#,
            ),
        ];

        let text = render_status_text(&views, now);

        let mut lines = text.lines();
        assert!(
            lines.next().unwrap().starts_with("HOST"),
            "header must lead with a HOST column: {text}"
        );
        let rows: Vec<&str> = lines.collect();
        assert!(rows[0].starts_with("web1"), "got: {text}");
        assert!(rows[1].starts_with("web2"), "got: {text}");
    }

    #[test]
    fn host_renders_as_row_and_flips_exit_code_when_it_is_unreachable() {
        let results = vec![
            HostResult {
                host: "web1".to_string(),
                outcome: HostOutcome::Success(
                    r#"{"schema":1,"jobs":[{"job":"backup"}]}"#.to_string(),
                    0,
                ),
            },
            HostResult {
                host: "web2".to_string(),
                outcome: HostOutcome::Unreachable,
            },
        ];
        let views = merged::collect(results);
        assert!(merged::any_problem(&views));

        let text = render_status_text(&views, ts("2026-06-01T00:00:00Z"));
        assert!(
            text.lines()
                .any(|l| l.starts_with("web2") && l.contains("unreachable")),
            "got: {text}"
        );
    }

    #[test]
    fn host_still_flips_merged_exit_code_when_its_remote_status_exits_nonzero() {
        let results = vec![HostResult {
            host: "web1".to_string(),
            outcome: HostOutcome::Success(
                r#"{"schema":1,"jobs":[{"job":"backup","status":"invalid"}]}"#.to_string(),
                1,
            ),
        }];
        let views = merged::collect(results);

        assert!(
            merged::any_problem(&views),
            "a nonzero remote exit code must be treated as a problem"
        );
        let text = render_status_text(&views, ts("2026-06-01T00:00:00Z"));
        assert!(
            text.lines()
                .any(|l| l.starts_with("web1") && l.contains("backup")),
            "the row must still render normally, unlike an unreachable host; got: {text}"
        );
    }

    #[test]
    fn remote_job_shows_overdue_instead_of_fabricated_next_time_when_it_is_overdue() {
        let views = vec![ok_view(
            "web1",
            r#"{"schema":1,"jobs":[{"job":"backup","status":"failure","next_ms":9999999999999,"overdue":true}]}"#,
        )];

        let text = render_status_text(&views, ts("2026-06-01T00:00:00Z"));
        let row = text
            .lines()
            .find(|l| l.starts_with("web1"))
            .unwrap_or_else(|| panic!("got: {text}"));
        assert!(row.contains("OVERDUE"), "got: {row}");
        assert!(
            !row.contains("in "),
            "must not also show the fabricated future NEXT time; got: {row}"
        );
    }

    #[test]
    fn remote_daemon_is_surfaced_per_host_when_reported_not_responding() {
        let views = vec![ok_view(
            "web1",
            r#"{"schema":1,"daemon":{"state":"not_responding","heartbeat_ms":0,"pid":123},"jobs":[{"job":"backup"}]}"#,
        )];

        let text = render_status_text(&views, ts("2026-06-01T00:00:00Z"));
        assert!(
            text.lines().any(|l| l.starts_with("web1:")
                && l.contains("not responding")
                && l.contains("123")),
            "got: {text}"
        );
    }

    #[test]
    fn remote_daemon_prints_no_note_when_reported_running() {
        let views = vec![ok_view(
            "web1",
            r#"{"schema":1,"daemon":{"state":"running","heartbeat_ms":0,"pid":123},"jobs":[{"job":"backup"}]}"#,
        )];

        let text = render_status_text(&views, ts("2026-06-01T00:00:00Z"));
        assert!(!text.contains("not responding"), "got: {text}");
    }
}
