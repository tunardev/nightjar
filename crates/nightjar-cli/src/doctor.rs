use std::path::Path;

use anyhow::Result;
use jiff::Timestamp;
use jiff::tz::TimeZone;
use nightjar_config::Job;
use nightjar_config::job::{JobsDirState, probe_jobs_dir};
use nightjar_core::clock::{Clock, SystemClock};
use nightjar_core::format::{error_summary, quantity, relative_time};
use nightjar_core::guidance::{
    daemon_never_ran, daemon_not_responding, daemon_running, no_jobs_yet, no_service_installed,
    relative_nightjar_home, service_installed_but_daemon_is_down, unnamed_timezone,
};
use nightjar_core::paths::Paths;
use nightjar_runner::service;
use nightjar_store::Store;
use owo_colors::{OwoColorize, Stream};
use serde_json::{Value, json};

use crate::status::{DaemonHealth, daemon_health};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Status {
    Pass,
    Warn,
    Fail,
}

impl Status {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Pass => "pass",
            Self::Warn => "warn",
            Self::Fail => "fail",
        }
    }
}

struct Check {
    name: &'static str,
    status: Status,
    message: String,
}

impl Check {
    const fn pass(name: &'static str, message: String) -> Self {
        Self {
            name,
            status: Status::Pass,
            message,
        }
    }

    const fn warn(name: &'static str, message: String) -> Self {
        Self {
            name,
            status: Status::Warn,
            message,
        }
    }

    const fn fail(name: &'static str, message: String) -> Self {
        Self {
            name,
            status: Status::Fail,
            message,
        }
    }
}

/// # Errors
/// fails if the nightjar directories cannot be resolved
pub fn cmd_doctor(json: bool) -> Result<i32> {
    let paths = Paths::resolve()?;
    let now = SystemClock.now();
    let checks = run_checks(&paths, now);
    let any_failed = checks.iter().any(|check| check.status == Status::Fail);

    if json {
        print_json(&checks);
    } else {
        print_human(&checks);
    }
    Ok(i32::from(any_failed))
}

fn run_checks(paths: &Paths, now: Timestamp) -> Vec<Check> {
    let (store_check, store) = check_store(paths);

    let health = store.as_ref().map_or_else(
        || {
            Err(anyhow::anyhow!(
                "the store did not open; see the store check above"
            ))
        },
        |store| daemon_health(store, now),
    );

    vec![
        check_config(paths),
        store_check,
        check_jobs(&paths.jobs_dir),
        check_daemon(&health, now),
        check_service(paths, health.as_ref().ok()),
        check_timezone(),
        check_home(),
    ]
}

fn check_config(paths: &Paths) -> Check {
    let path = paths.config_dir.join("config.toml");
    match nightjar_config::Config::load(paths) {
        Ok(_) if !path.exists() => Check::pass(
            "config",
            format!("no {}; every setting is at its default", path.display()),
        ),
        Ok(_) => Check::pass("config", format!("{} loads", path.display())),
        Err(e) => Check::fail(
            "config",
            format!(
                "{} does not load, so neither the daemon nor `nightjar run` can start: {} \
                ; fix it or remove the file",
                path.display(),
                config_error_summary(&e)
            ),
        ),
    }
}

fn config_error_summary(e: &anyhow::Error) -> String {
    let text = format!("{e:#}");
    let head = error_summary(e);
    let tail = text
        .lines()
        .map(str::trim_end)
        .rfind(|l| !l.trim().is_empty() && !l.trim_start().starts_with('|'))
        .unwrap_or_default()
        .trim();
    if tail.is_empty() || tail == head {
        head
    } else {
        format!("{head}: {tail}")
    }
}

fn check_store(paths: &Paths) -> (Check, Option<Store>) {
    let existed = paths.db_path.exists();
    match Store::open(&paths.db_path) {
        Ok(store) => match store.schema_version() {
            Ok(v) => (
                Check::pass(
                    "store",
                    if existed {
                        format!("opened {} at schema v{v}", paths.db_path.display())
                    } else {
                        format!(
                            "no store found at {}; created fresh at schema v{v} (first run?)",
                            paths.db_path.display()
                        )
                    },
                ),
                Some(store),
            ),
            Err(e) => (
                Check::fail(
                    "store",
                    format!(
                        "opened {} but could not read its schema: {e:#}",
                        paths.db_path.display()
                    ),
                ),
                None,
            ),
        },
        Err(e) => (
            Check::fail(
                "store",
                format!(
                    "could not open {}: {e:#}; check that {} is writable",
                    paths.db_path.display(),
                    paths.data_dir.display()
                ),
            ),
            None,
        ),
    }
}

fn check_jobs(jobs_dir: &Path) -> Check {
    match probe_jobs_dir(jobs_dir) {
        Ok(JobsDirState::Missing) => Check::warn("jobs", no_jobs_yet(jobs_dir)),
        Ok(JobsDirState::Present) => {
            let loaded = Job::load_all(jobs_dir);
            let bad: Vec<String> = loaded
                .iter()
                .filter_map(|(name, r)| {
                    r.as_ref()
                        .err()
                        .map(|e| format!("{name}: {}", error_summary(e)))
                })
                .collect();
            let warnings: Vec<&str> = loaded
                .iter()
                .filter_map(|(_, r)| r.as_ref().ok())
                .flat_map(|j| j.warnings.iter().map(String::as_str))
                .collect();
            if loaded.is_empty() {
                Check::warn("jobs", no_jobs_yet(jobs_dir))
            } else if bad.is_empty() && warnings.is_empty() {
                Check::pass("jobs", parse_verdict(loaded.len()))
            } else if bad.is_empty() {
                Check::warn(
                    "jobs",
                    format!(
                        "{}, but: {}",
                        parse_verdict(loaded.len()),
                        warnings.join("; ")
                    ),
                )
            } else {
                Check::fail(
                    "jobs",
                    format!(
                        "{} of {} failed to parse; {}",
                        bad.len(),
                        quantity(loaded.len(), "job file", "job files"),
                        bad.join("; ")
                    ),
                )
            }
        }
        Err(e) => Check::fail(
            "jobs",
            format!("could not read {}: {e:#}", jobs_dir.display()),
        ),
    }
}

fn parse_verdict(count: usize) -> String {
    match count {
        1 => "1 job, and it parses".to_string(),
        n => format!("{n} jobs, all parse"),
    }
}

fn check_daemon(health: &Result<DaemonHealth>, now: Timestamp) -> Check {
    match health {
        Err(e) => Check::fail(
            "daemon",
            format!("could not read the daemon heartbeat: {e:#}"),
        ),
        Ok(DaemonHealth::NeverRun) => Check::fail("daemon", daemon_never_ran().to_string()),
        Ok(DaemonHealth::NotResponding(beat)) => Check::fail(
            "daemon",
            daemon_not_responding(Some(beat.pid), &relative_time(beat.at, now)),
        ),
        Ok(DaemonHealth::Responding(beat)) => Check::pass("daemon", daemon_running(beat.pid)),
    }
}

fn check_service(paths: &Paths, health: Option<&DaemonHealth>) -> Check {
    let exe = match std::env::current_exe() {
        Ok(e) => e,
        Err(e) => {
            return Check::warn(
                "service",
                format!("could not resolve the running executable's path: {e:#}"),
            );
        }
    };
    let root = match service::install_root() {
        Ok(r) => r,
        Err(e) => {
            return Check::warn(
                "service",
                format!("could not determine where a service unit would live: {e:#}"),
            );
        }
    };
    let plan = match service::plan(paths, &exe, &root) {
        Ok(p) => p,
        Err(e) => return Check::warn("service", format!("{e:#}")),
    };
    if !plan.unit_path.exists() {
        return Check::warn("service", no_service_installed(&plan.unit_path));
    }
    match health {
        Some(DaemonHealth::Responding(beat)) => Check::pass(
            "service",
            format!(
                "installed at {} and the daemon is running (pid {})",
                plan.unit_path.display(),
                beat.pid
            ),
        ),
        _ => Check::warn(
            "service",
            service_installed_but_daemon_is_down(&plan.unit_path),
        ),
    }
}

fn check_timezone() -> Check {
    TimeZone::system().iana_name().map_or_else(
        || Check::warn("timezone", unnamed_timezone().to_string()),
        |name| Check::pass("timezone", format!("resolved as {name}")),
    )
}

fn check_home() -> Check {
    std::env::var_os("NIGHTJAR_HOME")
        .filter(|v| !v.is_empty())
        .map_or_else(
            || {
                Check::pass(
                    "home",
                    "NIGHTJAR_HOME is not set; using the default XDG locations".to_string(),
                )
            },
            |v| {
                let p = Path::new(&v);
                if p.is_absolute() {
                    Check::pass("home", format!("NIGHTJAR_HOME={} is absolute", p.display()))
                } else {
                    Check::fail("home", relative_nightjar_home(p))
                }
            },
        )
}

fn status_cell(status: Status) -> String {
    let padded = match status {
        Status::Pass => format!("{:<4}", "ok"),
        Status::Warn => format!("{:<4}", "warn"),
        Status::Fail => format!("{:<4}", "fail"),
    };
    match status {
        Status::Pass => padded
            .if_supports_color(Stream::Stdout, |t| t.green())
            .to_string(),
        Status::Warn => padded
            .if_supports_color(Stream::Stdout, |t| t.yellow())
            .to_string(),
        Status::Fail => padded
            .if_supports_color(Stream::Stdout, |t| t.red())
            .to_string(),
    }
}

fn print_human(checks: &[Check]) {
    for check in checks {
        println!(
            "[{}] {:<8} {}",
            status_cell(check.status),
            check.name,
            check.message
        );
    }
}

fn print_json(checks: &[Check]) {
    println!("{}", checks_json(checks));
}

fn checks_json(checks: &[Check]) -> Value {
    let items: Vec<Value> = checks
        .iter()
        .map(|check| {
            json!({
                "name": check.name,
                "status": check.status.as_str(),
                "message": check.message,
            })
        })
        .collect();
    json!({ "checks": items })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ts(s: &str) -> Timestamp {
        s.parse().unwrap()
    }

    #[test]
    fn check_store_says_so_when_it_had_to_create_store_and_stops_saying_so_after() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = Paths::for_root(tmp.path());

        let (fresh, _) = check_store(&paths);
        assert_eq!(fresh.status, Status::Pass);
        assert!(
            fresh.message.contains("no store found"),
            "a store that did not exist yet must say so: {}",
            fresh.message
        );

        let (existing, _) = check_store(&paths);
        assert_eq!(existing.status, Status::Pass);
        assert!(
            !existing.message.contains("no store found"),
            "must not claim first-run once the store already exists: {}",
            existing.message
        );
    }

    fn beat(pid: u32) -> nightjar_store::DaemonBeat {
        nightjar_store::DaemonBeat {
            at: ts("2026-06-01T00:00:00Z"),
            pid,
            version: "0.1.0".into(),
            caught_up_through: None,
        }
    }

    #[test]
    fn check_daemon_fails_and_says_how_to_start_one_when_no_daemon_has_ever_run() {
        let c = check_daemon(&Ok(DaemonHealth::NeverRun), ts("2026-06-01T00:00:00Z"));
        assert_eq!(c.status, Status::Fail);
        assert!(c.message.contains("nightjar daemon"));
    }

    #[test]
    fn check_daemon_passes_when_heartbeat_is_fresh() {
        let c = check_daemon(
            &Ok(DaemonHealth::Responding(beat(42))),
            ts("2026-06-01T00:00:00Z"),
        );
        assert_eq!(c.status, Status::Pass);
    }

    #[test]
    fn check_daemon_fails_when_heartbeat_is_stale() {
        let c = check_daemon(
            &Ok(DaemonHealth::NotResponding(beat(42))),
            ts("2026-06-01T00:00:00Z"),
        );
        assert_eq!(c.status, Status::Fail);
    }

    #[test]
    fn check_jobs_warns_and_names_the_disabled_parent_when_a_child_can_never_fire() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(
            tmp.path().join("a.toml"),
            "command = \"true\"\nschedule = \"hourly\"\nenabled = false\n",
        )
        .unwrap();
        std::fs::write(
            tmp.path().join("b.toml"),
            "command = \"true\"\nafter = [\"a\"]\n",
        )
        .unwrap();

        let c = check_jobs(tmp.path());
        assert_eq!(c.status, Status::Warn);
        assert!(c.message.contains("disabled"), "got: {}", c.message);
        assert!(c.message.contains("\"a\""), "got: {}", c.message);
    }

    #[test]
    fn check_jobs_passes_cleanly_when_every_job_parses_without_warnings() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(
            tmp.path().join("a.toml"),
            "command = \"true\"\nschedule = \"hourly\"\n",
        )
        .unwrap();
        let c = check_jobs(tmp.path());
        assert_eq!(c.status, Status::Pass);
    }

    #[test]
    fn checks_count_as_overall_failure_when_one_check_fails_among_passes() {
        let checks = [
            Check::pass("a", "fine".into()),
            Check::fail("b", "broken".into()),
            Check::pass("c", "fine".into()),
        ];
        assert!(checks.iter().any(|c| c.status == Status::Fail));
    }

    #[test]
    fn warn_alone_does_not_count_as_failure() {
        let checks = [
            Check::pass("a", "fine".into()),
            Check::warn("b", "meh".into()),
        ];
        assert!(!checks.iter().any(|c| c.status == Status::Fail));
    }

    #[test]
    fn json_output_has_no_ansi_and_names_every_check() {
        let checks = [
            Check::pass("store", "ok".into()),
            Check::fail("daemon", "dead \"quoted\"".into()),
        ];
        let v = checks_json(&checks);
        let s = v.to_string();
        assert!(!s.contains('\u{1b}'));
        assert_eq!(v["checks"][0]["name"], "store");
        assert_eq!(v["checks"][1]["name"], "daemon");
        assert_eq!(v["checks"][1]["status"], "fail");
        assert_eq!(v["checks"][1]["message"], "dead \"quoted\"");
    }
}
