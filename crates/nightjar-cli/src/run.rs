use std::io::Write;
use std::process::{Child, Command};

use anyhow::{Context, Result, bail};
use nightjar_config::Job;
use nightjar_core::clock::SystemClock;
use nightjar_core::format::quantity;
use nightjar_core::guidance::no_such_job;
use nightjar_core::paths::Paths;
use nightjar_daemon::overlap_allows;
use nightjar_runner::exec::{install_signal_handlers, reraise};
use nightjar_runner::execute;
use nightjar_runner::notify::DetachedNotifier;
use nightjar_store::Store;
use nightjar_store::run::{RunStatus, Trigger};

use crate::read_captured;

fn load_job(paths: &Paths, name: &str) -> Result<Job> {
    let path = paths.job_file(name)?;
    if !path.exists() {
        bail!("{}", no_such_job(name, &path));
    }
    Job::load(&path)
}

fn exit_code_for(status: RunStatus, exit_code: Option<i32>) -> i32 {
    match status {
        RunStatus::Success => 0,
        _ => exit_code.filter(|c| *c != 0).unwrap_or(1),
    }
}

fn exit_code_from_row(paths: &Paths, run_id: &str) -> Option<i32> {
    let store = Store::open(&paths.db_path).ok()?;
    let run = store.get_run(run_id).ok()??;
    run.finished_at?;
    Some(exit_code_for(run.status, run.exit_code))
}

/// # Errors
/// fails if the job does not exist or parse, or the run cannot be started
pub fn cmd_run(name: &str) -> Result<i32> {
    let paths = Paths::resolve()?;
    paths.ensure_dirs()?;
    let job = load_job(&paths, name)?;

    let store = Store::open(&paths.db_path)?;
    let in_flight = store.running_count(&job.name)?;
    if !overlap_allows(job.overlap, in_flight) {
        bail!(
            "{name}: already {} in flight and overlap = {} does not allow another",
            quantity(in_flight, "run", "runs"),
            job.overlap.as_str()
        );
    }

    let run_id = uuid::Uuid::now_v7().to_string();
    let exe = std::env::current_exe().context("locating own executable")?;

    let mut child: Child = Command::new(exe)
        .arg("exec")
        .arg(format!("--job={}", job.name))
        .arg(format!("--run={run_id}"))
        .arg("--trigger=manual")
        .spawn()
        .context("spawning nightjar exec")?;

    let status = child.wait().context("waiting for nightjar exec")?;

    let (out_path, err_path) = paths.run_output(&job.name, &run_id);
    if let Some(bytes) = read_captured(&out_path)? {
        std::io::stdout().write_all(&bytes)?;
    }
    if let Some(bytes) = read_captured(&err_path)? {
        std::io::stderr().write_all(&bytes)?;
    }

    Ok(exit_code_from_row(&paths, &run_id).unwrap_or_else(|| status.code().unwrap_or(1)))
}

/// # Errors
/// fails if the job does not exist or parse, or the run cannot be recorded
pub fn cmd_exec(name: &str, run_id: &str, trigger: &str) -> Result<i32> {
    let paths = Paths::resolve()?;
    paths.ensure_dirs()?;
    let config = nightjar_config::Config::load(&paths)?;
    let mut job = load_job(&paths, name)?;
    config.apply_defaults(&mut job);
    let store = Store::open(&paths.db_path)?;
    let trigger = Trigger::parse(trigger)?;

    install_signal_handlers();

    let outcome = execute(
        &job,
        run_id,
        trigger,
        &paths,
        &store,
        &SystemClock,
        config.output_cap,
        &DetachedNotifier,
        config.secrets_resolver.as_deref(),
    )?;

    if let Some(sig) = outcome.caught_signal {
        reraise(sig);
    }

    Ok(exit_code_for(outcome.status, outcome.exit_code))
}

#[cfg(test)]
mod tests {
    use nightjar_store::run::Trigger;

    use super::*;

    #[test]
    fn exit_code_for_is_always_zero_when_status_is_success() {
        assert_eq!(exit_code_for(RunStatus::Success, Some(1)), 0);
    }

    #[test]
    fn exit_code_for_prefers_nonzero_code_or_falls_back_to_one_when_status_is_not_success() {
        assert_eq!(exit_code_for(RunStatus::Failure, Some(7)), 7);
        assert_eq!(exit_code_for(RunStatus::Failure, Some(0)), 1);
        assert_eq!(exit_code_for(RunStatus::Failure, None), 1);
        assert_eq!(exit_code_for(RunStatus::Timeout, Some(0)), 1);
    }

    #[test]
    fn exit_code_from_row_is_none_when_store_cannot_be_opened() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = Paths::for_root(tmp.path());
        paths.ensure_dirs().unwrap();
        std::fs::create_dir_all(&paths.db_path).unwrap();

        assert_eq!(exit_code_from_row(&paths, "whatever"), None);
    }

    #[test]
    fn exit_code_from_row_is_none_when_run_id_is_unknown() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = Paths::for_root(tmp.path());
        paths.ensure_dirs().unwrap();
        Store::open(&paths.db_path).unwrap();

        assert_eq!(exit_code_from_row(&paths, "never-recorded"), None);
    }

    #[test]
    fn exit_code_from_row_is_none_when_row_is_still_running() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = Paths::for_root(tmp.path());
        paths.ensure_dirs().unwrap();
        let store = Store::open(&paths.db_path).unwrap();
        let t: jiff::Timestamp = "2026-06-01T00:00:00Z".parse().unwrap();
        store
            .start_run(
                "r1",
                "job",
                Trigger::Manual,
                t,
                std::path::Path::new("/tmp/o"),
                std::path::Path::new("/tmp/e"),
            )
            .unwrap();

        assert_eq!(
            exit_code_from_row(&paths, "r1"),
            None,
            "a row not yet finished must not be read as a final answer"
        );
    }

    #[test]
    fn exit_code_from_row_reflects_terminal_row() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = Paths::for_root(tmp.path());
        paths.ensure_dirs().unwrap();
        let store = Store::open(&paths.db_path).unwrap();
        let t: jiff::Timestamp = "2026-06-01T00:00:00Z".parse().unwrap();
        store
            .start_run(
                "r1",
                "job",
                Trigger::Manual,
                t,
                std::path::Path::new("/tmp/o"),
                std::path::Path::new("/tmp/e"),
            )
            .unwrap();
        store
            .finish_run("r1", RunStatus::Failure, Some(9), t, 0)
            .unwrap();

        assert_eq!(exit_code_from_row(&paths, "r1"), Some(9));
    }
}
