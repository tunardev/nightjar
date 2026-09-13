use std::fmt::Write as _;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

use anyhow::Result;
use jiff::Timestamp;
use jiff::tz::TimeZone;
use nightjar_config::job::{JobsDirState, next_column, probe_jobs_dir};
use nightjar_config::{Config, Job};
use nightjar_core::format::{abbreviate_schedule, duration_human, error_summary, relative_time};
use nightjar_core::guidance::no_jobs_yet;
use nightjar_core::paths::Paths;
use nightjar_store::run::{Run, RunStatus};
use nightjar_store::{Store, overdue_since};

use crate::url_encode_query_value;

const RUN_OUTPUT_PREVIEW_BYTES: u64 = 400;

pub fn escape_html(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            other => out.push(other),
        }
    }
    out
}

fn page(title: &str, body: &str) -> String {
    format!(
        "<!doctype html>\n\
         <html lang=\"en\">\n\
         <head>\n\
         <meta charset=\"utf-8\">\n\
         <meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">\n\
         <title>{}</title>\n\
         <link rel=\"stylesheet\" href=\"/style.css\">\n\
         </head>\n\
         <body>\n{body}\n\
         <script src=\"/refresh.js\" defer></script>\n\
         </body>\n\
         </html>\n",
        escape_html(title),
    )
}

fn empty_state(message: &str) -> String {
    format!("<p class=\"empty\">{}</p>\n", escape_html(message))
}

struct StatusBadge {
    label: &'static str,
    css_class: &'static str,
}

impl StatusBadge {
    fn html(&self) -> String {
        format!("<span class=\"{}\">{}</span>", self.css_class, self.label)
    }
}

const fn status_badge(status: RunStatus) -> StatusBadge {
    let (label, css_class) = match status {
        RunStatus::Success => ("ok", "status-ok"),
        RunStatus::Running => ("running", "status-running"),
        RunStatus::Timeout => ("TIMEOUT", "status-fail"),
        RunStatus::Unknown => ("UNKNOWN", "status-warn"),
        RunStatus::Missed => ("MISSED", "status-warn"),
        RunStatus::Failure => ("FAIL", "status-fail"),
        RunStatus::Limit => ("LIMIT", "status-fail"),
    };
    StatusBadge { label, css_class }
}

pub fn jobs_page(store: &Store, paths: &Paths, now: Timestamp) -> Result<String> {
    let dir_state = probe_jobs_dir(&paths.jobs_dir)?;
    if matches!(dir_state, JobsDirState::Missing) {
        return Ok(page(
            "nightjar",
            &empty_state(&no_jobs_yet(&paths.jobs_dir)),
        ));
    }

    let jobs = Job::load_all(&paths.jobs_dir);
    if jobs.is_empty() {
        return Ok(page(
            "nightjar",
            &empty_state(&no_jobs_yet(&paths.jobs_dir)),
        ));
    }

    let tz = TimeZone::system();
    let mut rows = String::new();
    for (name, loaded) in &jobs {
        rows.push_str(&job_row(store, name, loaded, now, &tz)?);
    }

    let body = format!(
        "<h1>nightjar</h1>\n\
         <table>\n\
         <thead><tr><th>Job</th><th>Status</th><th>Schedule</th><th>Last run</th>\
         <th>Duration</th><th>Next</th></tr></thead>\n\
         <tbody>\n{rows}</tbody>\n\
         </table>\n"
    );
    Ok(page("nightjar", &body))
}

fn job_row(
    store: &Store,
    name: &str,
    loaded: &Result<Job>,
    now: Timestamp,
    tz: &TimeZone,
) -> Result<String> {
    let href = format!("/runs?job={}", url_encode_query_value(name));
    let escaped_name = escape_html(name);

    let job = match loaded {
        Err(e) => {
            let err = escape_html(&error_summary(e));
            return Ok(format!(
                "<tr>\
                 <td data-label=\"Job\"><a href=\"{href}\">{escaped_name}</a></td>\
                 <td data-label=\"Status\" class=\"status-fail\">invalid: {err}</td>\
                 <td data-label=\"Schedule\">-</td>\
                 <td data-label=\"Last run\">-</td>\
                 <td data-label=\"Duration\">-</td>\
                 <td data-label=\"Next\">-</td></tr>\n"
            ));
        }
        Ok(j) => j,
    };

    let schedule = job
        .schedule_source()
        .map_or_else(|| "-".to_string(), |s| escape_html(&abbreviate_schedule(s)));
    let state = store.job_state(name)?;
    let last = store.last_run(name)?;

    let (last_run, status_cell, duration) = last.as_ref().map_or_else(
        || ("never".to_string(), "-".to_string(), "-".to_string()),
        |r| {
            let dur = r
                .duration_ms
                .map_or_else(|| "-".to_string(), duration_human);
            (
                relative_time(r.started_at, now),
                status_badge(r.status).html(),
                dur,
            )
        },
    );

    let next = overdue_since(state.as_ref(), last.as_ref(), now).map_or_else(
        || escape_html(&next_column(job, tz, now)),
        |since| {
            format!(
                "<span class=\"status-fail\">OVERDUE {}</span>",
                relative_time(since, now)
            )
        },
    );

    Ok(format!(
        "<tr>\
         <td data-label=\"Job\"><a href=\"{href}\">{escaped_name}</a></td>\
         <td data-label=\"Status\">{status_cell}</td>\
         <td data-label=\"Schedule\">{schedule}</td>\
         <td data-label=\"Last run\">{last_run}</td>\
         <td data-label=\"Duration\">{duration}</td>\
         <td data-label=\"Next\">{next}</td></tr>\n"
    ))
}

pub fn runs_page(
    store: &Store,
    paths: &Paths,
    job: &str,
    now: Timestamp,
) -> Result<Option<String>> {
    let known = match probe_jobs_dir(&paths.jobs_dir)? {
        JobsDirState::Missing => false,
        JobsDirState::Present => Job::load_all(&paths.jobs_dir).iter().any(|(n, _)| n == job),
    };
    if !known {
        return Ok(None);
    }

    let config = Config::load(paths)?;
    let runs = store.recent_runs(Some(job), config.retention_runs)?;
    let escaped_job = escape_html(job);

    let body = if runs.is_empty() {
        format!(
            "<h1>{escaped_job}</h1>\n\
             <p><a href=\"/\">\u{2190} jobs</a></p>\n\
             {}",
            empty_state(&format!("no runs recorded for {job}"))
        )
    } else {
        let mut rows = String::new();
        for run in &runs {
            rows.push_str(&run_row(run, now));
        }
        format!(
            "<h1>{escaped_job}</h1>\n\
             <p><a href=\"/\">\u{2190} jobs</a></p>\n\
             <table>\n\
             <thead><tr><th>When</th><th>Exit</th><th>Duration</th><th>Trigger</th></tr></thead>\n\
             <tbody>\n{rows}</tbody>\n\
             </table>\n"
        )
    };

    Ok(Some(page(&format!("nightjar - {job}"), &body)))
}

fn run_row(run: &Run, now: Timestamp) -> String {
    let when = relative_time(run.started_at, now);
    let badge = status_badge(run.status).html();
    let duration = run
        .duration_ms
        .map_or_else(|| "-".to_string(), duration_human);
    let trigger = escape_html(&run.trigger.to_db_string());
    let href = format!("/output/{}", escape_html(&run.id));

    let status_cell = run.message.as_deref().map_or_else(
        || badge.clone(),
        |message| {
            format!(
                "{badge} <span class=\"note\">{}</span>",
                escape_html(message)
            )
        },
    );

    let mut out = format!(
        "<tr>\
         <td data-label=\"When\"><a href=\"{href}\">{when}</a></td>\
         <td data-label=\"Exit\">{status_cell}</td>\
         <td data-label=\"Duration\">{duration}</td>\
         <td data-label=\"Trigger\">{trigger}</td></tr>\n"
    );
    if let Some(preview) = output_preview(run) {
        let _ = writeln!(
            out,
            "<tr class=\"preview\"><td colspan=\"4\"><pre>{}</pre></td></tr>",
            escape_html(&preview)
        );
    }
    out
}

fn output_preview(run: &Run) -> Option<String> {
    if !matches!(
        run.status,
        RunStatus::Failure | RunStatus::Timeout | RunStatus::Unknown | RunStatus::Limit
    ) {
        return None;
    }
    let path = run.stderr_path.as_deref().or(run.stdout_path.as_deref())?;
    let bytes = read_tail(path, RUN_OUTPUT_PREVIEW_BYTES).ok()?;
    if bytes.is_empty() {
        return None;
    }
    Some(String::from_utf8_lossy(&bytes).into_owned())
}

fn read_tail(path: &Path, max: u64) -> std::io::Result<Vec<u8>> {
    let mut file = std::fs::File::open(path)?;
    let len = file.metadata()?.len();
    file.seek(SeekFrom::Start(len.saturating_sub(max)))?;
    let mut buf = Vec::new();
    file.read_to_end(&mut buf)?;
    Ok(buf)
}

#[cfg(test)]
mod tests {
    use nightjar_store::run::Trigger;

    use super::*;

    fn ts(s: &str) -> Timestamp {
        s.parse().unwrap()
    }

    fn write_job(dir: &Path, name: &str, body: &str) {
        std::fs::write(dir.join(format!("{name}.toml")), body).unwrap();
    }

    fn paths_with_jobs_dir(tmp: &Path) -> Paths {
        let paths = Paths::for_root(tmp);
        std::fs::create_dir_all(&paths.jobs_dir).unwrap();
        paths
    }

    #[test]
    fn jobs_page_lists_every_job_with_last_run_and_next_run() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = paths_with_jobs_dir(tmp.path());
        write_job(
            &paths.jobs_dir,
            "backup",
            "command = \"true\"\nschedule = \"hourly\"\n",
        );

        let store = Store::open_in_memory().unwrap();
        let t0 = ts("2026-06-01T00:00:00Z");
        store
            .start_run(
                "r1",
                "backup",
                Trigger::Schedule,
                t0,
                Path::new("/tmp/o"),
                Path::new("/tmp/e"),
            )
            .unwrap();
        store
            .finish_run(
                "r1",
                RunStatus::Success,
                Some(0),
                t0 + jiff::Span::new().seconds(5),
                10,
            )
            .unwrap();
        store
            .set_next_run("backup", Some(t0 + jiff::Span::new().hours(1)))
            .unwrap();

        let now = t0 + jiff::Span::new().minutes(10);
        let html = jobs_page(&store, &paths, now).unwrap();

        assert!(html.contains("backup"), "got: {html}");
        assert!(html.contains("hourly"), "got: {html}");
        assert!(html.contains("ok"), "got: {html}");
        assert!(html.contains("in 50m"), "got: {html}");
    }

    #[test]
    fn job_is_distinguishable_without_relying_on_colour_alone_when_failing() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = paths_with_jobs_dir(tmp.path());
        write_job(
            &paths.jobs_dir,
            "backup",
            "command = \"true\"\nschedule = \"hourly\"\n",
        );

        let store = Store::open_in_memory().unwrap();
        let t0 = ts("2026-06-01T00:00:00Z");
        store
            .start_run(
                "r1",
                "backup",
                Trigger::Schedule,
                t0,
                Path::new("/tmp/o"),
                Path::new("/tmp/e"),
            )
            .unwrap();
        store
            .finish_run("r1", RunStatus::Failure, Some(1), t0, 0)
            .unwrap();

        let html = jobs_page(&store, &paths, t0).unwrap();
        assert!(html.contains(">FAIL<"), "got: {html}");
    }

    #[test]
    fn job_names_are_html_escaped() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = paths_with_jobs_dir(tmp.path());
        write_job(
            &paths.jobs_dir,
            "<img src=x onerror=alert(1)>",
            "command = \"true\"\nschedule = \"hourly\"\n",
        );

        let store = Store::open_in_memory().unwrap();
        let html = jobs_page(&store, &paths, ts("2026-06-01T00:00:00Z")).unwrap();

        assert!(
            !html.contains("<img src=x onerror=alert(1)>"),
            "the raw tag must never reach the page unescaped: {html}"
        );
        assert!(
            html.contains("&lt;img src=x onerror=alert(1)&gt;"),
            "got: {html}"
        );
    }

    #[test]
    fn run_output_is_html_escaped() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = paths_with_jobs_dir(tmp.path());
        write_job(
            &paths.jobs_dir,
            "backup",
            "command = \"true\"\nschedule = \"hourly\"\n",
        );

        let stderr_path = tmp.path().join("r1.err");
        std::fs::write(&stderr_path, "boom: <script>alert(1)</script>\n").unwrap();

        let store = Store::open_in_memory().unwrap();
        let t0 = ts("2026-06-01T00:00:00Z");
        store
            .start_run(
                "r1",
                "backup",
                Trigger::Schedule,
                t0,
                Path::new("/tmp/o"),
                &stderr_path,
            )
            .unwrap();
        store
            .finish_run("r1", RunStatus::Failure, Some(1), t0, 10)
            .unwrap();

        let html = runs_page(&store, &paths, "backup", t0).unwrap().unwrap();

        assert!(
            !html.contains("<script>alert(1)</script>"),
            "raw run output must never reach the page unescaped: {html}"
        );
        assert!(
            html.contains("&lt;script&gt;alert(1)&lt;/script&gt;"),
            "got: {html}"
        );
    }

    #[test]
    fn run_id_cannot_break_out_of_the_link_it_is_written_into() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = paths_with_jobs_dir(tmp.path());
        write_job(
            &paths.jobs_dir,
            "backup",
            "command = \"true\"\nschedule = \"hourly\"\n",
        );

        let store = Store::open_in_memory().unwrap();
        let t0 = ts("2026-06-01T00:00:00Z");
        let hostile_id = "r1\" onmouseover=\"alert(1)";
        store
            .start_run(
                hostile_id,
                "backup",
                Trigger::Schedule,
                t0,
                Path::new("/tmp/o"),
                Path::new("/tmp/e"),
            )
            .unwrap();

        let html = runs_page(&store, &paths, "backup", t0).unwrap().unwrap();

        assert!(
            !html.contains("onmouseover=\"alert(1)\""),
            "a run id is written straight into an href attribute; nothing in it may \
             close that attribute: {html}"
        );
        assert!(html.contains("&quot;"), "got: {html}");
    }

    #[test]
    fn run_message_is_shown_next_to_status_escaped() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = paths_with_jobs_dir(tmp.path());
        write_job(
            &paths.jobs_dir,
            "backup",
            "command = \"true\"\nschedule = \"hourly\"\n",
        );

        let store = Store::open_in_memory().unwrap();
        let t0 = ts("2026-06-01T00:00:00Z");
        store
            .start_run(
                "r1",
                "backup",
                Trigger::Schedule,
                t0,
                Path::new("/tmp/o"),
                Path::new("/tmp/e"),
            )
            .unwrap();
        store
            .finish_run("r1", RunStatus::Failure, Some(1), t0, 0)
            .unwrap();
        store
            .set_run_message(
                "r1",
                "secret <script>DB_PASSWORD</script> failed to resolve",
            )
            .unwrap();

        let html = runs_page(&store, &paths, "backup", t0).unwrap().unwrap();

        assert!(
            html.contains("DB_PASSWORD"),
            "the secret's name must be shown, not swallowed: {html}"
        );
        assert!(
            !html.contains("<script>DB_PASSWORD</script>"),
            "a message is job-adjacent text, not markup; it must be escaped: {html}"
        );
        assert!(
            html.contains("&lt;script&gt;DB_PASSWORD&lt;/script&gt;"),
            "got: {html}"
        );
    }

    #[test]
    fn run_shows_only_status_when_message_is_missing() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = paths_with_jobs_dir(tmp.path());
        write_job(
            &paths.jobs_dir,
            "backup",
            "command = \"true\"\nschedule = \"hourly\"\n",
        );

        let store = Store::open_in_memory().unwrap();
        let t0 = ts("2026-06-01T00:00:00Z");
        store
            .start_run(
                "r1",
                "backup",
                Trigger::Schedule,
                t0,
                Path::new("/tmp/o"),
                Path::new("/tmp/e"),
            )
            .unwrap();
        store
            .finish_run("r1", RunStatus::Success, Some(0), t0, 5)
            .unwrap();

        let html = runs_page(&store, &paths, "backup", t0).unwrap().unwrap();
        assert!(
            !html.contains("None"),
            "an absent message must not leak as the literal text 'None': {html}"
        );
    }

    #[test]
    fn every_page_is_complete_and_readable_when_javascript_is_disabled() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = paths_with_jobs_dir(tmp.path());
        write_job(
            &paths.jobs_dir,
            "backup",
            "command = \"true\"\nschedule = \"hourly\"\n",
        );

        let store = Store::open_in_memory().unwrap();
        let t0 = ts("2026-06-01T00:00:00Z");
        store
            .start_run(
                "r1",
                "backup",
                Trigger::Schedule,
                t0,
                Path::new("/tmp/o"),
                Path::new("/tmp/e"),
            )
            .unwrap();
        store
            .finish_run("r1", RunStatus::Success, Some(0), t0, 10)
            .unwrap();

        let jobs_html = jobs_page(&store, &paths, t0).unwrap();
        let runs_html = runs_page(&store, &paths, "backup", t0).unwrap().unwrap();

        for html in [&jobs_html, &runs_html] {
            let stripped = strip_scripts(html);
            assert!(stripped.contains("backup"), "got: {stripped}");
        }
        assert!(jobs_html.contains("backup"));
        assert!(runs_html.contains("ok"));
    }

    fn strip_scripts(html: &str) -> String {
        let mut out = String::new();
        let mut rest = html;
        while let Some(start) = rest.find("<script") {
            out.push_str(&rest[..start]);
            rest = rest[start..]
                .split_once("</script>")
                .map_or("", |(_, after)| after);
        }
        out.push_str(rest);
        out
    }

    #[test]
    fn installation_renders_a_useful_empty_state_when_empty() {
        let tmp = tempfile::tempdir().unwrap();
        let paths_without_jobs_dir = Paths::for_root(tmp.path());

        let store = Store::open_in_memory().unwrap();
        let html = jobs_page(&store, &paths_without_jobs_dir, ts("2026-06-01T00:00:00Z")).unwrap();

        assert!(html.contains("nightjar add"), "got: {html}");
        assert!(
            html.contains(&paths_without_jobs_dir.jobs_dir.display().to_string()),
            "got: {html}"
        );
    }

    #[test]
    fn pages_render_when_the_daemon_is_stopped() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = paths_with_jobs_dir(tmp.path());
        write_job(
            &paths.jobs_dir,
            "backup",
            "command = \"true\"\nschedule = \"hourly\"\n",
        );
        assert!(!paths.lock_path.exists());

        let store = Store::open(&paths.db_path).unwrap();
        let now = ts("2026-06-01T00:00:00Z");

        let jobs_html = jobs_page(&store, &paths, now).unwrap();
        assert!(jobs_html.contains("backup"), "got: {jobs_html}");

        let runs_html = runs_page(&store, &paths, "backup", now).unwrap().unwrap();
        assert!(runs_html.contains("no runs recorded"), "got: {runs_html}");
    }

    #[test]
    fn runs_page_fails_loudly_when_config_is_malformed() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = paths_with_jobs_dir(tmp.path());
        write_job(
            &paths.jobs_dir,
            "backup",
            "command = \"true\"\nschedule = \"hourly\"\n",
        );
        std::fs::write(
            paths.config_dir.join("config.toml"),
            "retention_runs = = 5\n",
        )
        .unwrap();

        let store = Store::open_in_memory().unwrap();
        let err = runs_page(&store, &paths, "backup", ts("2026-06-01T00:00:00Z")).unwrap_err();
        assert!(err.to_string().contains("config.toml"), "got: {err:#}");
    }

    #[test]
    fn runs_page_is_none_when_job_is_unknown() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = paths_with_jobs_dir(tmp.path());
        let store = Store::open_in_memory().unwrap();
        assert!(
            runs_page(&store, &paths, "no-such-job", ts("2026-06-01T00:00:00Z"))
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn job_still_appears_with_parse_error_when_invalid() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = paths_with_jobs_dir(tmp.path());
        write_job(&paths.jobs_dir, "broken", "command = = =\n");

        let store = Store::open_in_memory().unwrap();
        let html = jobs_page(&store, &paths, ts("2026-06-01T00:00:00Z")).unwrap();
        assert!(html.contains("broken"), "got: {html}");
        assert!(html.contains("invalid"), "got: {html}");
    }

    #[test]
    fn job_says_so_instead_of_a_fabricated_next_time_when_overdue() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = paths_with_jobs_dir(tmp.path());
        write_job(
            &paths.jobs_dir,
            "backup",
            "command = \"true\"\nschedule = \"hourly\"\n",
        );

        let store = Store::open_in_memory().unwrap();
        let t0 = ts("2026-06-01T00:00:00Z");
        let past = t0 - jiff::Span::new().hours(2);
        store.set_next_run("backup", Some(past)).unwrap();

        let html = jobs_page(&store, &paths, t0).unwrap();
        assert!(html.contains("OVERDUE"), "got: {html}");
    }

    #[test]
    fn run_output_preview_is_only_shown_when_run_is_not_successful() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = paths_with_jobs_dir(tmp.path());
        write_job(
            &paths.jobs_dir,
            "backup",
            "command = \"true\"\nschedule = \"hourly\"\n",
        );

        let stderr_path = tmp.path().join("r1.err");
        std::fs::write(&stderr_path, "should not appear\n").unwrap();

        let store = Store::open_in_memory().unwrap();
        let t0 = ts("2026-06-01T00:00:00Z");
        store
            .start_run(
                "r1",
                "backup",
                Trigger::Schedule,
                t0,
                Path::new("/tmp/o"),
                &stderr_path,
            )
            .unwrap();
        store
            .finish_run("r1", RunStatus::Success, Some(0), t0, 5)
            .unwrap();

        let html = runs_page(&store, &paths, "backup", t0).unwrap().unwrap();
        assert!(
            !html.contains("should not appear"),
            "a successful run must not show a preview: {html}"
        );
    }

    #[test]
    fn run_output_preview_is_skipped_not_panicked_on_when_pruned() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = paths_with_jobs_dir(tmp.path());
        write_job(
            &paths.jobs_dir,
            "backup",
            "command = \"true\"\nschedule = \"hourly\"\n",
        );

        let store = Store::open_in_memory().unwrap();
        let t0 = ts("2026-06-01T00:00:00Z");
        let gone = tmp.path().join("gone.err");
        store
            .start_run(
                "r1",
                "backup",
                Trigger::Schedule,
                t0,
                Path::new("/tmp/o"),
                &gone,
            )
            .unwrap();
        store
            .finish_run("r1", RunStatus::Failure, Some(1), t0, 0)
            .unwrap();

        let html = runs_page(&store, &paths, "backup", t0).unwrap().unwrap();
        assert!(html.contains("FAIL"), "got: {html}");
    }

    #[test]
    fn escape_html_covers_every_special_character() {
        assert_eq!(
            escape_html("<a href=\"x\">'&'</a>"),
            "&lt;a href=&quot;x&quot;&gt;&#39;&amp;&#39;&lt;/a&gt;"
        );
    }
}
