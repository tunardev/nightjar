use crate::status::daemon_state;
use anyhow::Result;
use jiff::Timestamp;
use jiff::tz::TimeZone;
use nightjar_config::Job;
use nightjar_config::job::{JobsDirState, probe_jobs_dir};
use nightjar_core::clock::{Clock, SystemClock};
use nightjar_core::format::{error_summary, json_string, relative_time};
use nightjar_core::paths::Paths;
use nightjar_runner::service;
use nightjar_store::{DaemonBeat, Store};
use owo_colors::{OwoColorize, Stream};
use std::path::Path;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Status {
    Pass,
    Warn,
    Fail,
}

impl Status {
    fn as_str(self) -> &'static str {
        match self {
            Status::Pass => "pass",
            Status::Warn => "warn",
            Status::Fail => "fail",
        }
    }
}

struct Check {
    name: &'static str,
    status: Status,
    message: String,
}

impl Check {
    fn pass(name: &'static str, message: String) -> Self {
        Self {
            name,
            status: Status::Pass,
            message,
        }
    }
    fn warn(name: &'static str, message: String) -> Self {
        Self {
            name,
            status: Status::Warn,
            message,
        }
    }
    fn fail(name: &'static str, message: String) -> Self {
        Self {
            name,
            status: Status::Fail,
            message,
        }
    }
}

pub fn cmd_doctor(json: bool) -> Result<i32> {
    let paths = Paths::resolve()?;
    let now = SystemClock.now();
    let checks = run_checks(&paths, now);
    let failed = checks.iter().any(|c| c.status == Status::Fail);

    if json {
        print_json(&checks);
    } else {
        print_human(&checks);
    }
    Ok(i32::from(failed))
}

fn run_checks(paths: &Paths, now: Timestamp) -> Vec<Check> {
    let (store_check, store) = check_store(paths);

    let daemon_result = match &store {
        Some(s) => daemon_state(s, now),
        None => Err(anyhow::anyhow!(
            "the store did not open; see the store check above"
        )),
    };
    let daemon = daemon_result.as_ref().ok().and_then(|d| d.as_ref());

    vec![
        check_config(paths),
        store_check,
        check_jobs(&paths.jobs_dir),
        check_daemon(&daemon_result, now),
        check_service(paths, daemon),
        check_timezone(),
        check_home(),
    ]
}

/// A config error blocks the daemon and every `nightjar exec`, so this
/// check must run first.
fn check_config(paths: &Paths) -> Check {
    let path = paths.config_dir.join("config.toml");
    match nightjar_config::Config::load(paths) {
        Ok(_) if !path.exists() => Check::pass(
            "config",
            format!("no {} — every setting is at its default", path.display()),
        ),
        Ok(_) => Check::pass("config", format!("{} loads", path.display())),
        Err(e) => Check::fail(
            "config",
            format!(
                "{} does not load, so neither the daemon nor `nightjar run` can start: {} \
                 — fix it or remove the file",
                path.display(),
                config_error_summary(&e)
            ),
        ),
    }
}

/// `error_summary` keeps only a toml error's location line, not the
/// diagnosis. This stitches both together.
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
    // `Store::open` creates the file if absent, so this checks existence
    // first.
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
                            "no store found at {} — created fresh at schema v{v} (first run?)",
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
                    "could not open {}: {e:#} — check that {} is writable",
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
        Ok(JobsDirState::Missing) => Check::warn(
            "jobs",
            format!(
                "no jobs directory yet at {} — run `nightjar add` to create your first job",
                jobs_dir.display()
            ),
        ),
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
            if bad.is_empty() && warnings.is_empty() {
                Check::pass("jobs", format!("{} job(s), all parse", loaded.len()))
            } else if bad.is_empty() {
                Check::warn(
                    "jobs",
                    format!(
                        "{} job(s), all parse, but: {}",
                        loaded.len(),
                        warnings.join("; ")
                    ),
                )
            } else {
                Check::fail(
                    "jobs",
                    format!(
                        "{} of {} job file(s) do not parse — {}",
                        bad.len(),
                        loaded.len(),
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

fn check_daemon(daemon: &Result<Option<(DaemonBeat, bool)>>, now: Timestamp) -> Check {
    match daemon {
        Err(e) => Check::fail(
            "daemon",
            format!("could not read the daemon heartbeat: {e:#}"),
        ),
        Ok(None) => Check::fail(
            "daemon",
            "no daemon has ever run — start one with `nightjar daemon`, or \
             `nightjar service install` to keep it running across reboots"
                .to_string(),
        ),
        Ok(Some((beat, true))) => Check::fail(
            "daemon",
            format!(
                "heartbeat is stale (pid {}, last seen {}) — it may have crashed; \
                 restart it with `nightjar daemon` or check `nightjar service status`",
                beat.pid,
                relative_time(beat.at, now)
            ),
        ),
        Ok(Some((beat, false))) => Check::pass("daemon", format!("running (pid {})", beat.pid)),
    }
}

fn check_service(paths: &Paths, daemon: Option<&(DaemonBeat, bool)>) -> Check {
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
        return Check::warn(
            "service",
            format!(
                "no service installed at {} — jobs will not survive a reboot or logout; \
                 run `nightjar service install` to fix this",
                plan.unit_path.display()
            ),
        );
    }
    match daemon {
        Some((beat, false)) => Check::pass(
            "service",
            format!(
                "installed at {} and the daemon is running (pid {})",
                plan.unit_path.display(),
                beat.pid
            ),
        ),
        _ => Check::warn(
            "service",
            format!(
                "a unit is installed at {} but the daemon is not currently running — \
                 check `nightjar service status`",
                plan.unit_path.display()
            ),
        ),
    }
}

fn check_timezone() -> Check {
    match TimeZone::system().iana_name() {
        Some(name) => Check::pass("timezone", format!("resolved as {name}")),
        None => Check::warn(
            "timezone",
            "could not resolve a named IANA timezone; schedules will run against a fixed \
             UTC offset instead of DST-aware local time — set $TZ to an IANA name \
             (e.g. America/New_York) to fix this"
                .to_string(),
        ),
    }
}

/// `NIGHTJAR_HOME` is the one path `Paths` does not coerce to absolute.
/// A relative value differs between an interactive shell and a service
/// started by launchd/systemd.
fn check_home() -> Check {
    match std::env::var_os("NIGHTJAR_HOME").filter(|v| !v.is_empty()) {
        None => Check::pass(
            "home",
            "NIGHTJAR_HOME is not set; using the default XDG locations".to_string(),
        ),
        Some(v) => {
            let p = Path::new(&v);
            if p.is_absolute() {
                Check::pass("home", format!("NIGHTJAR_HOME={} is absolute", p.display()))
            } else {
                Check::fail(
                    "home",
                    format!(
                        "NIGHTJAR_HOME={} is a relative path — a service started by \
                         launchd/systemd has a different working directory than your \
                         shell, so it would resolve to a different location than this \
                         command just used; set it to an absolute path",
                        p.display()
                    ),
                )
            }
        }
    }
}

fn print_human(checks: &[Check]) {
    for c in checks {
        let plain = match c.status {
            Status::Pass => "ok",
            Status::Warn => "warn",
            Status::Fail => "fail",
        };
        let padded = format!("{plain:<4}");
        let label = match c.status {
            Status::Pass => padded
                .if_supports_color(Stream::Stdout, |t| t.green())
                .to_string(),
            Status::Warn => padded
                .if_supports_color(Stream::Stdout, |t| t.yellow())
                .to_string(),
            Status::Fail => padded
                .if_supports_color(Stream::Stdout, |t| t.red())
                .to_string(),
        };
        println!("[{label}] {:<8} {}", c.name, c.message);
    }
}

fn print_json(checks: &[Check]) {
    let items: Vec<String> = checks
        .iter()
        .map(|c| {
            format!(
                r#"{{"name":{},"status":"{}","message":{}}}"#,
                json_string(c.name),
                c.status.as_str(),
                json_string(&c.message)
            )
        })
        .collect();
    println!(r#"{{"checks":[{}]}}"#, items.join(","));
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

    fn beat(pid: u32) -> DaemonBeat {
        DaemonBeat {
            at: ts("2026-06-01T00:00:00Z"),
            pid,
            version: "0.1.0".into(),
            caught_up_through: None,
        }
    }

    #[test]
    fn check_daemon_fails_and_says_how_to_start_one_when_no_daemon_has_ever_run() {
        let c = check_daemon(&Ok(None), ts("2026-06-01T00:00:00Z"));
        assert_eq!(c.status, Status::Fail);
        assert!(c.message.contains("nightjar daemon"));
    }

    #[test]
    fn check_daemon_passes_when_heartbeat_is_fresh() {
        let c = check_daemon(&Ok(Some((beat(42), false))), ts("2026-06-01T00:00:00Z"));
        assert_eq!(c.status, Status::Pass);
    }

    #[test]
    fn check_daemon_fails_when_heartbeat_is_stale() {
        let c = check_daemon(&Ok(Some((beat(42), true))), ts("2026-06-01T00:00:00Z"));
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
            Check::fail("daemon", "dead".into()),
        ];
        let items: Vec<String> = checks
            .iter()
            .map(|c| {
                format!(
                    r#"{{"name":{},"status":"{}","message":{}}}"#,
                    json_string(c.name),
                    c.status.as_str(),
                    json_string(&c.message)
                )
            })
            .collect();
        let s = format!(r#"{{"checks":[{}]}}"#, items.join(","));
        assert!(!s.contains('\u{1b}'));
        assert!(s.contains("\"name\":\"store\""));
        assert!(s.contains("\"name\":\"daemon\""));
        assert!(s.contains("\"status\":\"fail\""));
    }
}
