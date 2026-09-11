use std::path::Path;

#[must_use]
pub fn no_jobs_yet(jobs_dir: &Path) -> String {
    format!(
        "no jobs yet; run `nightjar add` to create your first one (jobs live in {})",
        jobs_dir.display()
    )
}

#[must_use]
pub fn no_such_job(job: &str, job_file: &Path) -> String {
    format!("no such job: {job} (expected {})", job_file.display())
}

#[must_use]
pub fn job_already_exists(job: &str, job_file: &Path) -> String {
    format!(
        "job {job:?} already exists at {}; use `nightjar edit {job}` to change it",
        job_file.display()
    )
}

#[must_use]
pub fn nothing_fires_until_the_daemon_runs(job: &str) -> String {
    format!(
        "{job} will not fire until a daemon is running; start one with `nightjar daemon`, \
         or `nightjar service install` to keep it running across reboots"
    )
}

#[must_use]
pub const fn daemon_never_ran() -> &'static str {
    "no daemon has ever run; start one with `nightjar daemon`, or `nightjar service install` \
     to keep it running across reboots"
}

#[must_use]
pub fn daemon_running(pid: u32) -> String {
    format!("daemon running (pid {pid})")
}

#[must_use]
pub fn daemon_not_responding(pid: Option<u32>, last_heartbeat: &str) -> String {
    let who = pid.map_or_else(String::new, |pid| format!("pid {pid}, "));
    format!(
        "daemon not responding ({who}last heartbeat {last_heartbeat}); it may have crashed, so \
         restart it with `nightjar daemon`, or check `nightjar service status`"
    )
}

#[must_use]
pub fn no_service_installed(unit_path: &Path) -> String {
    format!(
        "no service installed at {}; jobs will not survive a reboot or logout, \
         so run `nightjar service install` to fix this",
        unit_path.display()
    )
}

#[must_use]
pub fn service_installed_but_daemon_is_down(unit_path: &Path) -> String {
    format!(
        "a unit is installed at {} but the daemon is not currently running; \
         check `nightjar service status`",
        unit_path.display()
    )
}

#[must_use]
pub const fn unnamed_timezone() -> &'static str {
    "could not resolve a named IANA timezone; schedules will run against a fixed UTC offset \
     instead of DST-aware local time; set $TZ to an IANA name (e.g. America/New_York) \
     to fix this"
}

#[must_use]
pub fn relative_nightjar_home(home: &Path) -> String {
    format!(
        "NIGHTJAR_HOME={} is a relative path; a service started by launchd/systemd has a \
         different working directory than your shell, so it would resolve to a different \
         location than this command just used; set it to an absolute path",
        home.display()
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn every_message() -> Vec<String> {
        let jobs = Path::new("/nj/jobs");
        let file = Path::new("/nj/jobs/backup.toml");
        let unit = Path::new("/nj/units/nightjar.plist");
        vec![
            no_jobs_yet(jobs),
            no_such_job("backup", file),
            job_already_exists("backup", file),
            nothing_fires_until_the_daemon_runs("backup"),
            daemon_never_ran().to_string(),
            daemon_running(4211),
            daemon_not_responding(Some(4211), "5m ago"),
            no_service_installed(unit),
            service_installed_but_daemon_is_down(unit),
            unnamed_timezone().to_string(),
            relative_nightjar_home(Path::new("./nj")),
        ]
    }

    #[test]
    fn every_message_is_a_single_line_so_it_fits_a_table_row_or_a_check() {
        for message in every_message() {
            assert_eq!(message.lines().count(), 1, "got: {message}");
        }
    }

    #[test]
    fn every_message_reads_as_lowercase_prose_not_a_capitalised_sentence() {
        for message in every_message() {
            let mut chars = message.chars();
            let sentence_cased = chars.next().is_some_and(char::is_uppercase)
                && chars.next().is_some_and(char::is_lowercase);
            assert!(
                !sentence_cased,
                "nightjar speaks in lowercase, apart from names like NIGHTJAR_HOME; got: {message}"
            );
            assert!(!message.ends_with('.'), "got: {message}");
        }
    }

    #[test]
    fn a_problem_that_the_user_can_act_on_names_the_command_that_acts_on_it() {
        let jobs = Path::new("/nj/jobs");
        let unit = Path::new("/nj/units/nightjar.plist");
        for message in [
            no_jobs_yet(jobs),
            nothing_fires_until_the_daemon_runs("backup"),
            daemon_never_ran().to_string(),
            daemon_not_responding(Some(4211), "5m ago"),
            no_service_installed(unit),
            service_installed_but_daemon_is_down(unit),
        ] {
            assert!(message.contains("nightjar "), "got: {message}");
        }
    }

    #[test]
    fn no_jobs_yet_names_both_the_command_and_the_directory() {
        let message = no_jobs_yet(Path::new("/var/lib/nightjar/jobs"));
        assert!(message.contains("nightjar add"), "got: {message}");
        assert!(message.contains("/var/lib/nightjar/jobs"), "got: {message}");
    }

    #[test]
    fn no_such_job_names_the_file_it_looked_for() {
        let message = no_such_job("ghost", Path::new("/jobs/ghost.toml"));
        assert_eq!(message, "no such job: ghost (expected /jobs/ghost.toml)");
    }

    #[test]
    fn a_heartbeat_with_no_pid_still_reads_as_a_sentence() {
        assert_eq!(
            daemon_not_responding(None, "5m ago"),
            "daemon not responding (last heartbeat 5m ago); it may have crashed, so \
             restart it with `nightjar daemon`, or check `nightjar service status`"
        );
    }
}
