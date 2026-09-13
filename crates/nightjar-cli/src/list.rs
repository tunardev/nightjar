use std::fmt::Write as _;

use anyhow::Result;
use nightjar_config::Job;
use nightjar_config::job::{JobsDirState, probe_jobs_dir};
use nightjar_core::format::error_summary;
use nightjar_core::guidance::no_jobs_yet;
use nightjar_core::paths::Paths;
use nightjar_remote::HostResult;
use owo_colors::{OwoColorize, Stream};
use serde_json::{Value, json};

use crate::merged::{self, HostPayload, HostView};

/// # Errors
/// fails if the nightjar directories cannot be resolved
pub fn cmd_list(json: bool) -> Result<i32> {
    let paths = Paths::resolve()?;

    if matches!(probe_jobs_dir(&paths.jobs_dir)?, JobsDirState::Missing) {
        if json {
            println!(
                "{}",
                json!({ "schema": merged::SCHEMA_VERSION, "jobs": [] })
            );
        } else {
            println!("{}", no_jobs_yet(&paths.jobs_dir));
        }
        return Ok(0);
    }

    let jobs = Job::load_all(&paths.jobs_dir);
    let any_invalid = jobs.iter().any(|(_, r)| r.is_err());

    if json {
        println!("{}", list_json(&jobs));
        return Ok(i32::from(any_invalid));
    }

    if jobs.is_empty() {
        println!("{}", no_jobs_yet(&paths.jobs_dir));
        return Ok(0);
    }

    let header = format!("{:<16} {:<20} {}", "JOB", "SCHEDULE", "STATE");
    println!("{}", header.if_supports_color(Stream::Stdout, |t| t.bold()));
    for (name, loaded) in &jobs {
        match loaded {
            Ok(job) => {
                println!(
                    "{name:<16} {:<20} {}",
                    job.schedule_source().unwrap_or("-"),
                    enablement(job)
                );
            }
            Err(e) => {
                let invalid = "invalid".if_supports_color(Stream::Stdout, |t| t.red());
                println!("{name:<16} {:<20} {invalid} {e}", "-");
            }
        }
    }
    Ok(i32::from(any_invalid))
}

const fn enablement(job: &Job) -> &'static str {
    if job.enabled { "enabled" } else { "disabled" }
}

fn list_json(jobs: &[(String, Result<Job>)]) -> Value {
    let rows: Vec<Value> = jobs
        .iter()
        .map(|(name, loaded)| match loaded {
            Ok(job) => json!({
                "job": name,
                "schedule": job.schedule_source(),
                "state": enablement(job),
                "status": "ok",
                "error": null,
            }),
            Err(e) => json!({
                "job": name,
                "schedule": null,
                "state": null,
                "status": "invalid",
                "error": error_summary(e),
            }),
        })
        .collect();
    json!({ "schema": merged::SCHEMA_VERSION, "jobs": rows })
}

pub(crate) fn cmd_list_remote(results: Vec<HostResult>, local_json: bool) -> i32 {
    let views = merged::collect(results);
    let problem = merged::any_problem(&views);

    if local_json {
        println!("{}", merged::merged_json(&views));
    } else {
        print!("{}", render_list_text(&views));
    }
    i32::from(problem)
}

fn render_list_text(views: &[HostView]) -> String {
    let mut out = String::new();
    let _ = writeln!(out, "{:<12} {:<16} {:<20} STATE", "HOST", "JOB", "SCHEDULE");
    for view in views {
        match &view.payload {
            HostPayload::Ok(value) => append_list_rows(&mut out, &view.host, value),
            other => {
                let label = merged::problem_label(other).unwrap_or("error");
                let _ = writeln!(out, "{:<12} {label}", view.host);
            }
        }
    }
    out
}

fn append_list_rows(out: &mut String, host: &str, value: &Value) {
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
        let schedule = job.get("schedule").and_then(Value::as_str).unwrap_or("-");
        let state = job
            .get("state")
            .and_then(Value::as_str)
            .or_else(|| job.get("status").and_then(Value::as_str))
            .unwrap_or("-");
        let _ = writeln!(out, "{host:<12} {name:<16} {schedule:<20} {state}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn job(name: &str, enabled: bool) -> (String, Result<Job>) {
        let toml = format!("command = \"true\"\nschedule = \"hourly\"\nenabled = {enabled}\n");
        (name.to_string(), Job::from_toml_str(name, &toml))
    }

    #[test]
    fn list_json_keeps_its_documented_key_order() {
        let rendered = list_json(&[job("alpha", true)]).to_string();
        assert_eq!(
            rendered,
            r#"{"schema":1,"jobs":[{"job":"alpha","schedule":"hourly","state":"enabled","status":"ok","error":null}]}"#,
            "readers diff this document"
        );
    }

    #[test]
    fn list_json_reports_a_disabled_job_as_disabled_not_as_a_shared_default() {
        let rendered = list_json(&[job("alpha", true), job("beta", false)]);
        assert_eq!(rendered["jobs"][0]["state"], "enabled");
        assert_eq!(rendered["jobs"][1]["state"], "disabled");
    }

    #[test]
    fn list_json_nulls_every_field_it_cannot_know_when_job_does_not_parse() {
        let broken = (
            "broken".to_string(),
            Job::from_toml_str("broken", "command = = =\n"),
        );
        let rendered = list_json(&[broken]);
        let row = &rendered["jobs"][0];
        assert_eq!(row["status"], "invalid");
        assert!(row["schedule"].is_null(), "got: {row}");
        assert!(row["state"].is_null(), "got: {row}");
        assert!(
            row["error"].as_str().is_some_and(|e| !e.is_empty()),
            "an invalid job must say why; got: {row}"
        );
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
        let views = vec![
            ok_view(
                "web1",
                r#"{"schema":1,"jobs":[{"job":"backup","schedule":"hourly","state":"enabled","status":"ok"}]}"#,
            ),
            ok_view(
                "web2",
                r#"{"schema":1,"jobs":[{"job":"backup","schedule":"hourly","state":"disabled","status":"ok"}]}"#,
            ),
        ];

        let text = render_list_text(&views);
        let mut lines = text.lines();
        assert!(lines.next().unwrap().starts_with("HOST"), "got: {text}");
        let rows: Vec<&str> = lines.collect();
        assert!(
            rows[0].starts_with("web1") && rows[0].contains("enabled"),
            "got: {text}"
        );
        assert!(
            rows[1].starts_with("web2") && rows[1].contains("disabled"),
            "got: {text}"
        );
    }
}
