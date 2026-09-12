use std::process::{Child, Command, Stdio};

use anyhow::{Context, Result, bail};
use nightjar_core::guidance::no_such_job;
use nightjar_core::paths::Paths;

use crate::term::{RawTerminal, TerminalGuard};

/// # Errors
/// fails if the job does not exist or parse, or the `exec` child cannot be spawned
pub fn run_now(paths: &Paths, job: &str) -> Result<Child> {
    let path = paths.job_file(job)?;
    if !path.exists() {
        bail!("{}", no_such_job(job, &path));
    }
    nightjar_config::Job::load(&path)?;

    let run_id = uuid::Uuid::now_v7().to_string();
    let exe = std::env::current_exe().context("locating own executable")?;

    Command::new(exe)
        .arg("exec")
        .arg(format!("--job={job}"))
        .arg(format!("--run={run_id}"))
        .arg("--trigger=manual")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .with_context(|| format!("spawning exec for job {job:?}"))
}

/// # Errors
/// fails if the terminal cannot be left or re-entered, or the edit itself fails
pub fn suspend_and_edit<T: RawTerminal>(
    guard: &mut TerminalGuard<T>,
    mut edit: impl FnMut() -> Result<()>,
) -> Result<()> {
    guard.leave()?;
    let outcome = edit();
    guard.re_enter()?;
    outcome
}

/// # Errors
/// fails if `$EDITOR` is unset, the editor exits non-zero, or the file no longer parses
pub fn edit(job: &str) -> Result<()> {
    nightjar_config::jobfile::cmd_edit(job)?;
    Ok(())
}

/// # Errors
/// fails if the job does not exist, or its `enabled` key cannot be rewritten
pub fn toggle_enabled(paths: &Paths, job: &str) -> Result<bool> {
    let path = paths.job_file(job)?;
    if !path.exists() {
        bail!("{}", no_such_job(job, &path));
    }
    let currently_enabled = nightjar_config::jobfile::read_enabled(&path)?;
    let target = !currently_enabled;
    nightjar_config::jobfile::write_enabled(&path, target)?;
    Ok(target)
}

/// # Errors
/// fails if the pid does not fit `pid_t`, or `kill` fails for anything but a gone process
pub fn kill_run(pid: u32) -> Result<()> {
    let raw = libc::pid_t::try_from(pid)
        .with_context(|| format!("pid {pid} does not fit the platform's process id type"))?;
    if unsafe { libc::kill(raw, libc::SIGTERM) } == 0 {
        return Ok(());
    }
    let err = std::io::Error::last_os_error();
    if err.raw_os_error() == Some(libc::ESRCH) {
        return Ok(());
    }
    Err(err).with_context(|| format!("sending SIGTERM to pid {pid}"))
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::io;
    use std::rc::Rc;

    use super::*;

    #[derive(Default)]
    struct RecordingTerminal {
        log: Rc<RefCell<Vec<&'static str>>>,
    }

    impl RawTerminal for RecordingTerminal {
        fn enter(&mut self) -> io::Result<()> {
            self.log.borrow_mut().push("enter");
            Ok(())
        }

        fn leave(&mut self) -> io::Result<()> {
            self.log.borrow_mut().push("leave");
            Ok(())
        }
    }

    #[test]
    fn suspend_path_re_enters_unconditionally_when_the_editor_fails() {
        let log = Rc::new(RefCell::new(Vec::new()));
        let mut guard = TerminalGuard::new(RecordingTerminal {
            log: Rc::clone(&log),
        })
        .unwrap();

        let result = suspend_and_edit(&mut guard, || Err(anyhow::anyhow!("editor crashed")));

        assert!(result.is_err(), "the editor's own failure must surface");
        assert_eq!(
            *log.borrow(),
            vec!["enter", "leave", "enter"],
            "re-entry must happen even though the edit closure failed"
        );
    }

    #[test]
    fn toggling_enabled_writes_nothing_when_the_job_file_is_unparseable() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("bad.toml");
        let original = "command = = =\nenabled = true\n";
        std::fs::write(&path, original).unwrap();

        let err = nightjar_config::jobfile::write_enabled(&path, false).unwrap_err();
        assert!(!format!("{err:#}").is_empty());
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            original,
            "a file that failed to parse must not be written to"
        );
    }

    #[test]
    fn toggling_enabled_preserves_comments_and_formatting() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("job.toml");
        let original =
            "# important comment\ncommand = \"true\"\nschedule = \"hourly\"\nenabled = true\n";
        std::fs::write(&path, original).unwrap();

        nightjar_config::jobfile::write_enabled(&path, false).unwrap();

        let after = std::fs::read_to_string(&path).unwrap();
        assert!(after.contains("# important comment"), "got: {after}");
        assert!(after.contains("enabled = false"), "got: {after}");
    }

    #[test]
    fn toggle_enabled_flips_the_jobs_actual_state() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = Paths::for_root(tmp.path());
        paths.ensure_dirs().unwrap();
        std::fs::write(
            paths.jobs_dir.join("j.toml"),
            "command = \"true\"\nschedule = \"hourly\"\nenabled = true\n",
        )
        .unwrap();

        let now_enabled = toggle_enabled(&paths, "j").unwrap();
        assert!(!now_enabled);
        assert!(
            !nightjar_config::Job::load(&paths.jobs_dir.join("j.toml"))
                .unwrap()
                .enabled
        );
    }

    #[test]
    fn kill_run_is_not_an_error_when_the_pid_is_already_gone() {
        kill_run(u32::try_from(i32::MAX).unwrap() - 1).unwrap();
    }

    #[test]
    fn run_now_refuses_the_job_and_spawns_nothing_when_it_does_not_parse() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = Paths::for_root(tmp.path());
        paths.ensure_dirs().unwrap();
        std::fs::write(paths.jobs_dir.join("broken.toml"), "command = = =\n").unwrap();

        let err = run_now(&paths, "broken").unwrap_err();
        assert!(
            !format!("{err:#}").is_empty(),
            "the parse error must be surfaced, not swallowed"
        );
    }

    #[test]
    fn run_now_refuses_the_job_when_job_load_rejects_its_schedule() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = Paths::for_root(tmp.path());
        paths.ensure_dirs().unwrap();
        std::fs::write(
            paths.jobs_dir.join("badsched.toml"),
            "command = \"true\"\nschedule = \"@nonsense\"\n",
        )
        .unwrap();

        assert!(run_now(&paths, "badsched").is_err());
    }

    #[test]
    fn kill_run_terminates_a_real_child() {
        let mut child = Command::new("sleep").arg("30").spawn().unwrap();
        let pid = child.id();

        kill_run(pid).unwrap();

        let status = child.wait().unwrap();
        assert!(!status.success(), "SIGTERM must actually have reached it");
    }
}
