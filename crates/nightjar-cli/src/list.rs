use crate::merged::{self, HostPayload, HostView};
use anyhow::Result;
use nightjar_config::Job;
use nightjar_config::job::{JobsDirState, probe_jobs_dir};
use nightjar_core::format::error_summary;
use nightjar_core::paths::Paths;
use nightjar_remote::HostResult;
use owo_colors::{OwoColorize, Stream};
use serde_json::{Value, json};
use std::fmt::Write as _;

pub fn cmd_list(json: bool) -> Result<i32> {
    let paths = Paths::resolve()?;

    if let JobsDirState::Missing = probe_jobs_dir(&paths.jobs_dir)? {
        if json {
            println!(
                "{}",
                json!({ "schema": merged::SCHEMA_VERSION, "jobs": [] })
            );
        } else {
            println!(
                "no jobs directory yet; create one at {}",
                paths.jobs_dir.display()
            );
        }
        return Ok(0);
    }

    let jobs = Job::load_all(&paths.jobs_dir);
    let any_invalid = jobs.iter().any(|(_, r)| r.is_err());

    if json {
        print_json(&jobs);
        return Ok(i32::from(any_invalid));
    }

    if jobs.is_empty() {
        println!(
            "no jobs configured yet (add a .toml file to {})",
            paths.jobs_dir.display()
        );
        return Ok(0);
    }

    let header = format!("{:<16} {:<20} {}", "JOB", "SCHEDULE", "STATE");
    println!("{}", header.if_supports_color(Stream::Stdout, |t| t.bold()));
    for (name, result) in &jobs {
        match result {
            Ok(j) => {
                let state = if j.enabled { "enabled" } else { "disabled" };
                println!(
                    "{name:<16} {:<20} {state}",
                    j.schedule_source().unwrap_or("—")
                );
            }
            Err(e) => {
                let invalid = "invalid".if_supports_color(Stream::Stdout, |t| t.red());
                println!("{name:<16} {:<20} {invalid} {e}", "—");
            }
        }
    }
    Ok(i32::from(any_invalid))
}

fn print_json(jobs: &[(String, Result<Job>)]) {
    let rows: Vec<Value> = jobs
        .iter()
        .map(|(name, result)| match result {
            Ok(j) => json!({
                "job": name,
                "schedule": j.schedule_source(),
                "state": if j.enabled { "enabled" } else { "disabled" },
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
    println!(
        "{}",
        json!({ "schema": merged::SCHEMA_VERSION, "jobs": rows })
    );
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
        let schedule = job.get("schedule").and_then(Value::as_str).unwrap_or("—");
        // Invalid remote jobs have `state: null` but `status: "invalid"`.
        let state = job
            .get("state")
            .and_then(Value::as_str)
            .or_else(|| job.get("status").and_then(Value::as_str))
            .unwrap_or("—");
        let _ = writeln!(out, "{host:<12} {name:<16} {schedule:<20} {state}");
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
