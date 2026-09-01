use crate::read_captured;
use anyhow::{Context, Result, bail};
use nightjar_config::Job;
use nightjar_core::clock::SystemClock;
use nightjar_core::paths::Paths;
use nightjar_daemon::overlap_allows;
use nightjar_runner::exec::{install_signal_handlers, reraise};
use nightjar_runner::execute;
use nightjar_runner::notify::DetachedNotifier;
use nightjar_store::Store;
use nightjar_store::run::{RunStatus, Trigger};
use std::io::Write;
use std::process::{Child, Command};

fn load_job(paths: &Paths, name: &str) -> Result<Job> {
    let path = paths.job_file(name)?;
    if !path.exists() {
        bail!("no such job: {name} (expected {})", path.display());
    }
    Job::load(&path)
}

/// Status decides, not the raw process code. A timed-out run's shell can
/// still exit 0 (`sleep 15 & echo x`).
fn exit_code_for(status: RunStatus, exit_code: Option<i32>) -> i32 {
    match status {
        RunStatus::Success => 0,
        _ => exit_code.filter(|c| *c != 0).unwrap_or(1),
    }
}

/// The wrapper can die from a signal unrelated to how the job finished.
/// So the row's own recorded outcome is preferred over its raw exit
/// status. `None` here means the caller falls back to that raw status.
fn exit_code_from_row(paths: &Paths, run_id: &str) -> Option<i32> {
    let store = Store::open(&paths.db_path).ok()?;
    let run = store.get_run(run_id).ok()??;
    run.finished_at?;
    Some(exit_code_for(run.status, run.exit_code))
}

/// The child records the run, so a crash in this parent can't lose it.
///
/// Waits for the child's actual exit, not just the row going terminal.
/// A descendant that outlives `PUMP_DRAIN` can still write capture files
/// after the row is finalized.
pub fn cmd_run(name: &str) -> Result<i32> {
    let paths = Paths::resolve()?;
    paths.ensure_dirs()?;
    let job = load_job(&paths, name)?;

    // The daemon could have a run of this job in flight right now. Overlap
    // must refuse a manual run exactly like it would the daemon's own next.
    let store = Store::open(&paths.db_path)?;
    let in_flight = store.running_count(&job.name)?;
    if !overlap_allows(job.overlap, in_flight) {
        bail!(
            "{name}: already {in_flight} run(s) in flight and overlap = {:?} does not allow another",
            job.overlap
        );
    }

    let run_id = uuid::Uuid::now_v7().to_string();
    let exe = std::env::current_exe().context("locating own executable")?;

    // `--job=<name>`, not `--job <name>`. A job name beginning with `-` is
    // a legal filename, and the child would read it as a flag instead.
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

pub fn cmd_exec(name: &str, run_id: &str, trigger: &str) -> Result<i32> {
    let paths = Paths::resolve()?;
    paths.ensure_dirs()?;
    // `exec` is spawned fresh per run, so a malformed config fails here too.
    let config = nightjar_config::Config::load(&paths)?;
    let mut job = load_job(&paths, name)?;
    config.apply_defaults(&mut job);
    let store = Store::open(&paths.db_path)?;
    let trigger = Trigger::parse(trigger)?;

    // Without this, the wrapper stays in the terminal's foreground group.
    // SIGINT, SIGTERM, or SIGHUP would kill it and leave the `running` row
    // stale. This runs here, not in `execute`, so tests calling `execute`
    // directly keep their own signal disposition.
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

    // If `execute` failed, the signal is lost, but the row stays terminal.
    if let Some(sig) = outcome.caught_signal {
        reraise(sig);
    }

    Ok(exit_code_for(outcome.status, outcome.exit_code))
}

#[cfg(test)]
mod tests {
    use super::*;
    use nightjar_store::run::Trigger;

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
