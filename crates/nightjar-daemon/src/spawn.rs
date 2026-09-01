use crate::daemon::Daemon;
use anyhow::{Context, Result};
use jiff::Timestamp;
use nightjar_config::Job;
use nightjar_store::run::{RunStatus, Trigger};
use std::process::{Child, Command, ExitStatus, Stdio};

/// The daemon's fork-per-run boundary, injected like `Clock`. A failing
/// spawn is a real failure mode with consequences for `next_run_at` (see
/// `evaluate`). `std::env::current_exe()` gives no seam to test that.
pub trait Spawner: Send + Sync {
    /// Starts `nightjar exec` for one run. The caller must reap the
    /// handle: `Child` does not wait on its process when dropped. The
    /// child writes its own run row, so `trigger` is forwarded verbatim.
    fn spawn(&self, job: &str, run_id: &str, trigger: Trigger) -> Result<Child>;
}

/// Spawns the daemon's own binary. The child owns the run and records it, so
/// a daemon that dies mid-run cannot take the record with it.
pub struct ExecSpawner;

impl Spawner for ExecSpawner {
    fn spawn(&self, job: &str, run_id: &str, trigger: Trigger) -> Result<Child> {
        let exe = std::env::current_exe().context("locating own executable")?;
        // `--job=<name>`, not `--job <name>`. A job named `-n` is a legal
        // filename, but as a separate argument it would be read as a flag.
        Command::new(exe)
            .arg("exec")
            .arg(format!("--job={job}"))
            .arg(format!("--run={run_id}"))
            .arg(format!("--trigger={}", trigger.to_db_string()))
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .with_context(|| format!("spawning exec for job {job:?}"))
    }
}

/// A firing whose `nightjar exec` child hasn't been waited on yet. Carries
/// enough to reconstruct the run row if the child dies before writing one.
/// See `Daemon::report_exec_exit`.
pub(crate) struct SpawnedExec {
    job: String,
    pub(crate) run_id: String,
    child: Child,
    trigger: Trigger,
    /// The run's start time, even if the wrapper never got far enough to record it.
    forked_at: Timestamp,
}

impl Daemon {
    pub(crate) fn spawn(&mut self, job: &Job, trigger: Trigger) -> Result<()> {
        self.spawn_as(job, uuid::Uuid::now_v7().to_string(), trigger)
    }

    /// `spawn` under an id the caller chose. Catch-up needs this: the run's
    /// row already exists as a `missed` placeholder, and the child
    /// supersedes it by writing under the same id.
    pub(crate) fn spawn_as(&mut self, job: &Job, run_id: String, trigger: Trigger) -> Result<()> {
        let forked_at = self.clock.now();
        let child = self.spawner.spawn(&job.name, &run_id, trigger.clone())?;
        self.children.push(SpawnedExec {
            job: job.name.clone(),
            run_id,
            child,
            trigger,
            forked_at,
        });
        Ok(())
    }

    /// Waits on every spawned `nightjar exec` that has finished.
    ///
    /// `Child` does not wait on its process when dropped. Without this, an
    /// `every 1 minute` job leaks 1,440 zombies a day. `try_wait` keeps
    /// this off the blocking path.
    pub(crate) fn reap(&mut self) {
        let mut unfinished = Vec::with_capacity(self.children.len());
        for mut spawned in std::mem::take(&mut self.children) {
            let finished = spawned.child.try_wait();
            match finished {
                Ok(None) => unfinished.push(spawned),
                Ok(Some(status)) => self.report_exec_exit(&spawned, status),
                Err(e) => eprintln!(
                    "nightjar: job {:?}: cannot reap run {}: {e}",
                    spawned.job, spawned.run_id
                ),
            }
        }
        self.children = unfinished;
    }

    /// A non-zero exit from `nightjar exec` means the job failed (already
    /// recorded in its own terminal row), or the wrapper died before it
    /// could record anything. Only the second case is this function's.
    /// The child's stdio is nulled and it's not coming back, so nothing
    /// else can report it.
    ///
    /// This records the outcome, not just logs it: an unwritten or
    /// `running` row either loses the occurrence silently or silences an
    /// `overlap = "skip"` job forever.
    fn report_exec_exit(&self, spawned: &SpawnedExec, status: ExitStatus) {
        // `reap` is learning that `spawned.job`'s run just went terminal,
        // whether or not this function writes the row. A success already
        // has its row written by `nightjar exec` on its own connection.
        // Pruned only once the row is terminal, matching `reconcile`'s
        // order: pruning first would count that row one pass too late.
        if status.success() {
            self.prune_job_now(&spawned.job);
            return;
        }
        let outcome = match self.store.get_run(&spawned.run_id) {
            // Either the job failed and said so, or this is a make-up run
            // that never superseded its `missed` row. Either way,
            // "unknown" is accurate here.
            Ok(Some(run)) if run.finished_at.is_some() => {
                self.prune_job_now(&spawned.job);
                return;
            }
            Ok(Some(_)) => {
                eprintln!(
                    "nightjar: job {:?}: exec exited {status} without finishing run {} \
                     — recording it unknown",
                    spawned.job, spawned.run_id
                );
                self.finish_unknown(
                    &spawned.run_id,
                    &format!("nightjar exec exited {status} before recording an outcome"),
                )
            }
            Ok(None) => {
                eprintln!(
                    "nightjar: job {:?}: exec exited {status} without recording run {} \
                     — the run did not reach the store; recording it unknown",
                    spawned.job, spawned.run_id
                );
                self.record_unstarted(spawned, status)
            }
            Err(e) => {
                eprintln!(
                    "nightjar: job {:?}: cannot check run {}: {e:#}",
                    spawned.job, spawned.run_id
                );
                return;
            }
        };
        match &outcome {
            Ok(_) => self.prune_job_now(&spawned.job),
            Err(e) => eprintln!(
                "nightjar: job {:?}: cannot record run {} as unknown: {e:#}",
                spawned.job, spawned.run_id
            ),
        }
    }

    /// Writes the row a wrapper never got to, because it died before
    /// `start_run`. Uses the spawn time, not now, so the occurrence lands
    /// where it belongs on the job's timeline.
    fn record_unstarted(&self, spawned: &SpawnedExec, status: ExitStatus) -> Result<bool> {
        let (stdout, stderr) = self.paths.run_output(&spawned.job, &spawned.run_id);
        self.store.start_run(
            &spawned.run_id,
            &spawned.job,
            spawned.trigger.clone(),
            spawned.forked_at,
            &stdout,
            &stderr,
        )?;
        self.finish_unknown(
            &spawned.run_id,
            &format!("nightjar exec exited {status} before it could record the run"),
        )
    }

    /// Terminal `unknown` plus the reason, so `status` and `logs` can say
    /// why instead of leaving the user to guess.
    pub(crate) fn finish_unknown(&self, run_id: &str, reason: &str) -> Result<bool> {
        let finished = self.store.finish_unfinished_run(
            run_id,
            RunStatus::Unknown,
            None,
            self.clock.now(),
            0,
        )?;
        if finished {
            self.store.set_run_message(run_id, reason)?;
        }
        Ok(finished)
    }
}
