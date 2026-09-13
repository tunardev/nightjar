use std::fmt::Write as _;

use anyhow::Result;
use jiff::Timestamp;
use jiff::tz::TimeZone;
use nightjar_config::Job;
use nightjar_config::job::{JobsDirState, next_column, probe_jobs_dir};
use nightjar_core::clock::{Clock, SystemClock};
use nightjar_core::format::{
    abbreviate_schedule, duration_human, error_summary, relative_future, relative_time,
};
use nightjar_core::guidance::{
    daemon_never_ran, daemon_not_responding, daemon_running, no_jobs_yet, no_such_job,
};
use nightjar_core::paths::Paths;
use nightjar_remote::HostResult;
use nightjar_store::run::{Run, RunStatus};
use nightjar_store::{DaemonBeat, Store, overdue_since};
use owo_colors::{OwoColorize, Stream};
use serde_json::{Value, json};

use crate::merged::{self, HostPayload, HostView};

const HEARTBEAT_STALE_AFTER_SECONDS: i64 = 90;

pub(crate) enum DaemonHealth {
    NeverRun,
    Responding(DaemonBeat),
    NotResponding(DaemonBeat),
}

pub(crate) fn daemon_health(store: &Store, now: Timestamp) -> Result<DaemonHealth> {
    let Some(beat) = store.daemon_heartbeat()? else {
        return Ok(DaemonHealth::NeverRun);
    };
    let age_seconds = now.as_second() - beat.at.as_second();
    Ok(if age_seconds > HEARTBEAT_STALE_AFTER_SECONDS {
        DaemonHealth::NotResponding(beat)
    } else {
        DaemonHealth::Responding(beat)
    })
}

fn daemon_line(health: &DaemonHealth, now: Timestamp) -> String {
    match health {
        DaemonHealth::NeverRun => daemon_never_ran().to_string(),
        DaemonHealth::Responding(beat) => daemon_running(beat.pid),
        DaemonHealth::NotResponding(beat) => {
            daemon_not_responding(Some(beat.pid), &relative_time(beat.at, now))
        }
    }
}

fn daemon_json(health: &DaemonHealth) -> Value {
    match health {
        DaemonHealth::NeverRun => {
            json!({ "state": "never_run", "heartbeat_ms": null, "pid": null })
        }
        DaemonHealth::Responding(beat) => json!({
            "state": "running",
            "heartbeat_ms": beat.at.as_millisecond(),
            "pid": beat.pid,
        }),
        DaemonHealth::NotResponding(beat) => json!({
            "state": "not_responding",
            "heartbeat_ms": beat.at.as_millisecond(),
            "pid": beat.pid,
        }),
    }
}

struct JobStatus<'a> {
    name: &'a str,
    facts: JobFacts<'a>,
}

enum JobFacts<'a> {
    Unloadable(&'a anyhow::Error),
    Loaded {
        job: &'a Job,
        last_run: Option<Box<Run>>,
        overdue_since: Option<Timestamp>,
    },
}

fn collect_job_status<'a>(
    store: &Store,
    jobs: &'a [(String, Result<Job>)],
    now: Timestamp,
) -> Result<Vec<JobStatus<'a>>> {
    jobs.iter()
        .map(|(name, loaded)| {
            let facts = match loaded {
                Err(e) => JobFacts::Unloadable(e),
                Ok(job) => {
                    let last_run = store.last_run(name)?.map(Box::new);
                    let state = store.job_state(name)?;
                    JobFacts::Loaded {
                        job,
                        overdue_since: overdue_since(state.as_ref(), last_run.as_deref(), now),
                        last_run,
                    }
                }
            };
            Ok(JobStatus {
                name: name.as_str(),
                facts,
            })
        })
        .collect()
}

fn any_job_unloadable(rows: &[JobStatus]) -> bool {
    rows.iter()
        .any(|row| matches!(row.facts, JobFacts::Unloadable(_)))
}

/// # Errors
/// fails if the store cannot be opened, or the named job does not exist
pub fn cmd_status(job_filter: Option<&str>, json: bool) -> Result<i32> {
    let paths = Paths::resolve()?;
    let store = Store::open(&paths.db_path)?;
    let now = SystemClock.now();
    let tz = TimeZone::system();

    let dir_state = probe_jobs_dir(&paths.jobs_dir)?;
    let jobs: Vec<(String, Result<Job>)> = match dir_state {
        JobsDirState::Missing => Vec::new(),
        JobsDirState::Present => Job::load_all(&paths.jobs_dir)
            .into_iter()
            .filter(|(name, _)| job_filter.is_none_or(|wanted| wanted == name))
            .collect(),
    };

    if let Some(wanted) = job_filter
        && jobs.is_empty()
    {
        return merged::refuse_as_not_found(&no_such_job(wanted, &paths.job_file(wanted)?), json);
    }

    let rows = collect_job_status(&store, &jobs, now)?;
    let health = daemon_health(&store, now)?;
    let exit_code = i32::from(any_job_unloadable(&rows));

    if json {
        println!("{}", status_json(&rows, &health, &tz, now));
        return Ok(exit_code);
    }

    if matches!(dir_state, JobsDirState::Missing) {
        println!("{}", no_jobs_yet(&paths.jobs_dir));
        return Ok(0);
    }
    if rows.is_empty() {
        println!("{}", no_jobs_yet(&paths.jobs_dir));
        return Ok(0);
    }

    print!(
        "{}",
        status_table(&rows, &daemon_line(&health, now), &tz, now)
    );
    Ok(exit_code)
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
    let pid = daemon
        .get("pid")
        .and_then(Value::as_i64)
        .and_then(|pid| u32::try_from(pid).ok());
    Some(format!("{host}: {}", daemon_not_responding(pid, &when)))
}

fn remote_status_label(raw: &str) -> String {
    RunStatus::parse(raw).map_or_else(
        |_| raw.to_string(),
        |status| status_label(status).to_string(),
    )
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
            .map_or_else(|| "-".to_string(), abbreviate_schedule);
        let status = job
            .get("status")
            .and_then(Value::as_str)
            .map_or_else(|| "-".to_string(), remote_status_label);
        let last_run = job
            .get("last_run_ms")
            .and_then(Value::as_i64)
            .and_then(|ms| Timestamp::from_millisecond(ms).ok())
            .map_or_else(|| "never".to_string(), |t| relative_time(t, now));
        let duration = job
            .get("duration_ms")
            .and_then(Value::as_i64)
            .map_or_else(|| "-".to_string(), duration_human);
        let next = if job.get("overdue").and_then(Value::as_bool).unwrap_or(false) {
            "OVERDUE"
                .if_supports_color(Stream::Stdout, |t| t.red())
                .to_string()
        } else {
            job.get("next_ms")
                .and_then(Value::as_i64)
                .and_then(|ms| Timestamp::from_millisecond(ms).ok())
                .map_or_else(|| "-".to_string(), |t| relative_future(t, now))
        };
        let _ = writeln!(
            out,
            "{host:<12} {name:<16} {schedule:<20} {last_run:<18} {status:<10} {duration:<10} {next}"
        );
    }
}

fn status_json(rows: &[JobStatus], health: &DaemonHealth, tz: &TimeZone, now: Timestamp) -> Value {
    let jobs: Vec<Value> = rows.iter().map(|row| job_json(row, tz, now)).collect();
    json!({
        "schema": merged::SCHEMA_VERSION,
        "daemon": daemon_json(health),
        "jobs": jobs,
    })
}

fn job_json(row: &JobStatus, tz: &TimeZone, now: Timestamp) -> Value {
    match &row.facts {
        JobFacts::Unloadable(e) => json!({
            "job": row.name,
            "schedule": null,
            "status": "invalid",
            "error": error_summary(e),
            "exit_code": null,
            "duration_ms": null,
            "last_run_ms": null,
            "next_ms": null,
            "overdue": false,
        }),
        JobFacts::Loaded {
            job,
            last_run,
            overdue_since,
        } => {
            let next_ms =
                job.schedule
                    .as_ref()
                    .and_then(|schedule| match schedule.next_after(now, tz) {
                        Ok(Some(t)) => Some(t.as_millisecond()),
                        Ok(None) | Err(_) => None,
                    });
            json!({
                "job": row.name,
                "schedule": job.schedule_source(),
                "status": last_run.as_ref().map(|r| r.status.as_str()),
                "error": null,
                "exit_code": last_run.as_ref().and_then(|r| r.exit_code),
                "duration_ms": last_run.as_ref().and_then(|r| r.duration_ms),
                "last_run_ms": last_run.as_ref().map(|r| r.started_at.as_millisecond()),
                "next_ms": next_ms,
                "overdue": overdue_since.is_some(),
            })
        }
    }
}

fn status_table(rows: &[JobStatus], daemon_line: &str, tz: &TimeZone, now: Timestamp) -> String {
    let mut out = String::new();
    let _ = writeln!(out, "{daemon_line}");

    let header = format!(
        "{:<16} {:<20} {:<18} {:<8} {:<10} {}",
        "JOB", "SCHEDULE", "LAST RUN", "EXIT", "DURATION", "NEXT"
    );
    let _ = writeln!(
        out,
        "{}",
        header.if_supports_color(Stream::Stdout, |t| t.bold())
    );

    for row in rows {
        let _ = writeln!(out, "{}", status_row(row, tz, now));
    }
    out
}

fn status_row(row: &JobStatus, tz: &TimeZone, now: Timestamp) -> String {
    let name = row.name;
    let (job, last_run, overdue_since) = match &row.facts {
        JobFacts::Unloadable(e) => {
            let padded = format!("{:<8}", "invalid");
            let invalid = padded.if_supports_color(Stream::Stdout, |t| t.red());
            return format!(
                "{name:<16} {:<20} {:<18} {invalid} {:<10} {:<9} {}",
                "-",
                "-",
                "-",
                "-",
                error_summary(e)
            );
        }
        JobFacts::Loaded {
            job,
            last_run,
            overdue_since,
        } => (job, last_run, overdue_since),
    };

    let schedule = job
        .schedule_source()
        .map_or_else(|| "-".to_string(), abbreviate_schedule);
    let next = overdue_since.map_or_else(
        || next_column(job, tz, now),
        |since| {
            format!("OVERDUE {}", relative_time(since, now))
                .if_supports_color(Stream::Stdout, |t| t.red())
                .to_string()
        },
    );

    let Some(run) = last_run else {
        return format!(
            "{name:<16} {schedule:<20} {:<18} {:<8} {:<10} {next}",
            "never", "-", "-"
        );
    };

    let when = relative_time(run.started_at, now);
    let duration = run.duration_ms.map_or_else(|| "-".into(), duration_human);
    let mut line = format!(
        "{name:<16} {schedule:<20} {when:<18} {} {duration:<10} {next}",
        status_cell(run.status)
    );
    if let Some(message) = &run.message {
        let _ = write!(line, "  ({message})");
    }
    line
}

const fn status_label(status: RunStatus) -> &'static str {
    match status {
        RunStatus::Success => "ok",
        RunStatus::Running => "…",
        RunStatus::Timeout => "TIMEOUT",
        RunStatus::Unknown => "UNKNOWN",
        RunStatus::Missed => "MISSED",
        RunStatus::Failure => "FAIL",
        RunStatus::Limit => "LIMIT",
    }
}

fn status_cell(status: RunStatus) -> String {
    let padded = format!("{:<8}", status_label(status));
    match status {
        RunStatus::Running => padded,
        RunStatus::Success => padded
            .if_supports_color(Stream::Stdout, |t| t.green())
            .to_string(),
        RunStatus::Unknown | RunStatus::Missed => padded
            .if_supports_color(Stream::Stdout, |t| t.yellow())
            .to_string(),
        RunStatus::Timeout | RunStatus::Failure | RunStatus::Limit => padded
            .if_supports_color(Stream::Stdout, |t| t.red())
            .to_string(),
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use nightjar_config::{Catchup, OnFailure, Overlap};
    use nightjar_remote::HostOutcome;
    use nightjar_store::run::Trigger;

    use super::*;

    fn ts(s: &str) -> Timestamp {
        s.parse().unwrap()
    }

    fn now() -> Timestamp {
        ts("2026-06-01T00:00:00Z")
    }

    fn job(name: &str, schedule: Option<&str>) -> Job {
        Job {
            name: name.into(),
            command: "true".into(),
            schedule: schedule.map(|s| nightjar_schedule::Schedule::parse(s).unwrap()),
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

    fn run(status: RunStatus, message: Option<&str>) -> Run {
        Run {
            id: "r1".into(),
            job: "alpha".into(),
            trigger: Trigger::Manual,
            started_at: now(),
            finished_at: Some(now()),
            exit_code: Some(0),
            duration_ms: Some(1500),
            status,
            pid: None,
            stdout_path: None,
            stderr_path: None,
            output_bytes: 0,
            message: message.map(str::to_string),
        }
    }

    fn loaded<'a>(
        name: &'a str,
        job: &'a Job,
        last_run: Option<Box<Run>>,
        overdue_since: Option<Timestamp>,
    ) -> JobStatus<'a> {
        JobStatus {
            name,
            facts: JobFacts::Loaded {
                job,
                last_run,
                overdue_since,
            },
        }
    }

    #[test]
    fn table_leads_with_daemon_line_then_column_header() {
        let alpha = job("alpha", Some("hourly"));
        let rows = [loaded("alpha", &alpha, None, None)];

        let table = status_table(&rows, "daemon running (pid 7)", &TimeZone::UTC, now());

        let mut lines = table.lines();
        assert_eq!(lines.next().unwrap(), "daemon running (pid 7)");
        assert!(lines.next().unwrap().starts_with("JOB"), "got: {table}");
    }

    #[test]
    fn table_shows_overdue_in_place_of_next_time_when_job_owes_an_occurrence() {
        let alpha = job("alpha", Some("hourly"));
        let overdue_at = now() - jiff::Span::new().hours(3);
        let rows = [loaded("alpha", &alpha, None, Some(overdue_at))];

        let table = status_table(&rows, "daemon running (pid 7)", &TimeZone::UTC, now());

        assert!(table.contains("OVERDUE 3h ago"), "got: {table}");
        assert!(
            !table.contains("in 1h"),
            "an overdue job must not also advertise its next time; got: {table}"
        );
    }

    #[test]
    fn table_marks_unloadable_job_invalid_and_carries_its_reason() {
        let broken = anyhow::anyhow!("expected a value at line 1");
        let rows = [JobStatus {
            name: "broken",
            facts: JobFacts::Unloadable(&broken),
        }];

        let table = status_table(&rows, "daemon running (pid 7)", &TimeZone::UTC, now());
        let row = table.lines().nth(2).unwrap();

        assert!(row.starts_with("broken"), "got: {row}");
        assert!(row.contains("invalid"), "got: {row}");
        assert!(row.contains("expected a value"), "got: {row}");
        assert!(
            !row.contains("never"),
            "an unloadable job has no run history to describe; got: {row}"
        );
    }

    #[test]
    fn table_appends_recorded_reason_after_the_row_when_run_has_one() {
        let alpha = job("alpha", Some("hourly"));
        let rows = [loaded(
            "alpha",
            &alpha,
            Some(Box::new(run(
                RunStatus::Unknown,
                Some("wrapper was killed"),
            ))),
            None,
        )];

        let table = status_table(&rows, "d", &TimeZone::UTC, now());
        assert!(table.contains("  (wrapper was killed)"), "got: {table}");
    }

    #[test]
    fn table_and_json_report_the_same_overdue_verdict_for_one_row() {
        let alpha = job("alpha", Some("hourly"));
        let overdue_at = now() - jiff::Span::new().hours(3);

        for overdue_since in [None, Some(overdue_at)] {
            let rows = [loaded("alpha", &alpha, None, overdue_since)];
            let table = status_table(&rows, "d", &TimeZone::UTC, now());
            let rendered = status_json(&rows, &DaemonHealth::NeverRun, &TimeZone::UTC, now());

            assert_eq!(
                table.contains("OVERDUE"),
                rendered["jobs"][0]["overdue"] == true,
                "the table and --json read the same row; table was: {table}"
            );
        }
    }

    #[test]
    fn json_keeps_its_documented_key_order_and_top_level_shape() {
        let alpha = job("alpha", Some("hourly"));
        let rows = [loaded(
            "alpha",
            &alpha,
            Some(Box::new(run(RunStatus::Success, None))),
            None,
        )];

        let rendered =
            status_json(&rows, &DaemonHealth::NeverRun, &TimeZone::UTC, now()).to_string();

        assert!(
            rendered.starts_with(r#"{"schema":1,"daemon":{"#),
            "readers diff this document; got: {rendered}"
        );
        assert!(
            rendered
                .contains(r#"{"job":"alpha","schedule":"hourly","status":"success","error":null,"#),
            "got: {rendered}"
        );
    }

    #[test]
    fn a_local_row_and_a_remote_row_label_the_same_outcome_identically() {
        for status in [
            RunStatus::Success,
            RunStatus::Running,
            RunStatus::Timeout,
            RunStatus::Unknown,
            RunStatus::Missed,
            RunStatus::Failure,
            RunStatus::Limit,
        ] {
            assert_eq!(
                remote_status_label(status.as_str()),
                status_label(status),
                "a fleet view and a local one must not word {status:?} differently"
            );
        }
    }

    #[test]
    fn a_remote_status_this_version_does_not_know_passes_through_verbatim() {
        assert_eq!(remote_status_label("quarantined"), "quarantined");
    }

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

    fn health_of(beat_at: Option<Timestamp>, now: Timestamp) -> DaemonHealth {
        let store = Store::open_in_memory().unwrap();
        if let Some(at) = beat_at {
            store.write_heartbeat(at, 123, "0.1.0").unwrap();
        }
        daemon_health(&store, now).unwrap()
    }

    #[test]
    fn daemon_line_says_so_when_no_heartbeat_has_ever_been_written() {
        let now = ts("2026-06-01T00:00:00Z");
        let line = daemon_line(&health_of(None, now), now);
        assert!(line.contains("no daemon"), "got: {line}");
    }

    #[test]
    fn daemon_line_reads_as_running_when_heartbeat_is_fresh() {
        let now = ts("2026-06-01T00:00:00Z");
        let line = daemon_line(&health_of(Some(now), now), now);
        assert!(line.contains("running"), "got: {line}");
        assert!(!line.contains("not responding"), "got: {line}");
    }

    #[test]
    fn daemon_line_still_reads_as_running_when_heartbeat_is_exactly_at_stale_threshold() {
        let beat_at = ts("2026-06-01T00:00:00Z");
        let now = beat_at + jiff::Span::new().seconds(HEARTBEAT_STALE_AFTER_SECONDS);
        let line = daemon_line(&health_of(Some(beat_at), now), now);
        assert!(
            line.contains("running") && !line.contains("not responding"),
            "a heartbeat exactly at the threshold is still within tolerance; got: {line}"
        );
    }

    #[test]
    fn daemon_line_reads_as_not_responding_when_heartbeat_is_one_second_past_stale_threshold() {
        let beat_at = ts("2026-06-01T00:00:00Z");
        let now = beat_at + jiff::Span::new().seconds(HEARTBEAT_STALE_AFTER_SECONDS + 1);
        let line = daemon_line(&health_of(Some(beat_at), now), now);
        assert!(line.contains("not responding"), "got: {line}");
    }

    #[test]
    fn daemon_line_does_not_panic_and_reads_as_running_when_heartbeat_is_from_future() {
        let now = ts("2026-06-01T00:00:00Z");
        let beat_at = now + jiff::Span::new().minutes(10);
        let line = daemon_line(&health_of(Some(beat_at), now), now);
        assert!(
            line.contains("running") && !line.contains("not responding"),
            "got: {line}"
        );
    }

    #[test]
    fn daemon_json_serialises_null_fields_when_daemon_has_never_run() {
        assert_eq!(
            daemon_json(&DaemonHealth::NeverRun).to_string(),
            r#"{"state":"never_run","heartbeat_ms":null,"pid":null}"#,
            "key order is part of the documented output"
        );
    }

    #[test]
    fn daemon_json_reports_heartbeat_and_pid_when_daemon_is_running() {
        let now = ts("2026-06-01T00:00:00Z");
        let rendered = daemon_json(&health_of(Some(now), now));
        assert_eq!(rendered["state"], "running", "got: {rendered}");
        assert_eq!(
            rendered["heartbeat_ms"],
            now.as_millisecond(),
            "got: {rendered}"
        );
        assert_eq!(rendered["pid"], 123, "got: {rendered}");
    }

    #[test]
    fn daemon_json_reports_not_responding_when_heartbeat_is_stale() {
        let beat_at = ts("2026-06-01T00:00:00Z");
        let now = beat_at + jiff::Span::new().seconds(HEARTBEAT_STALE_AFTER_SECONDS + 1);
        let rendered = daemon_json(&health_of(Some(beat_at), now));
        assert_eq!(rendered["state"], "not_responding", "got: {rendered}");
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
