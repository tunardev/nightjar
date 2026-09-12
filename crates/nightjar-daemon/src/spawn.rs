use std::process::{Child, Command, ExitStatus, Stdio};

use anyhow::{Context, Result};
use jiff::Timestamp;
use nightjar_config::Job;
use nightjar_store::run::{RunStatus, Trigger};

use crate::daemon::Daemon;

pub trait Spawner: Send + Sync {
    /// # Errors
    /// fails if an exec process cannot be started for the run
    fn spawn(&self, job: &str, run_id: &str, trigger: Trigger) -> Result<Child>;
}

pub struct ExecSpawner;

impl Spawner for ExecSpawner {
    fn spawn(&self, job: &str, run_id: &str, trigger: Trigger) -> Result<Child> {
        let exe = std::env::current_exe().context("locating own executable")?;
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

pub struct SpawnedExec {
    pub(crate) job: String,
    pub(crate) run_id: String,
    child: Child,
    trigger: Trigger,
    forked_at: Timestamp,
}

impl Daemon {
    pub(crate) fn spawn(&mut self, job: &Job, trigger: Trigger) -> Result<()> {
        self.spawn_as(job, uuid::Uuid::now_v7().to_string(), trigger)
    }

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

    pub(crate) fn in_flight_count(&self, job: &str) -> Result<usize> {
        let mut in_flight = self.store.running_count(job)?;
        for child in self.children.iter().filter(|c| c.job == job) {
            if self.is_awaiting_its_run_row(child)? {
                in_flight += 1;
            }
        }
        Ok(in_flight)
    }

    fn is_awaiting_its_run_row(&self, child: &SpawnedExec) -> Result<bool> {
        Ok(match self.store.get_run(&child.run_id)? {
            None => true,
            Some(run) => run.status == RunStatus::Missed,
        })
    }

    pub(crate) fn reap(&mut self) {
        let mut unfinished = Vec::with_capacity(self.children.len());
        for mut spawned in std::mem::take(&mut self.children) {
            let finished = spawned.child.try_wait();
            match finished {
                Ok(None) => unfinished.push(spawned),
                Ok(Some(status)) => self.report_exec_exit(&spawned, status),
                Err(e) => nightjar_core::log!(
                    "job {:?}: cannot reap run {}: {e}",
                    spawned.job,
                    spawned.run_id
                ),
            }
        }
        self.children = unfinished;
    }

    fn report_exec_exit(&self, spawned: &SpawnedExec, status: ExitStatus) {
        if status.success() {
            self.prune_job_now(&spawned.job);
            return;
        }
        let outcome = match self.store.get_run(&spawned.run_id) {
            Ok(Some(run)) if run.finished_at.is_some() => {
                self.prune_job_now(&spawned.job);
                return;
            }
            Ok(Some(_)) => {
                nightjar_core::log!(
                    "job {:?}: exec exited {status} without finishing run {}, \
                     recording it unknown",
                    spawned.job,
                    spawned.run_id
                );
                self.finish_unknown(
                    &spawned.run_id,
                    &format!("nightjar exec exited {status} before recording an outcome"),
                )
            }
            Ok(None) => {
                nightjar_core::log!(
                    "job {:?}: exec exited {status} without recording run {}; the run never \
                     reached the store, so it is recorded unknown",
                    spawned.job,
                    spawned.run_id
                );
                self.record_unstarted(spawned, status)
            }
            Err(e) => {
                nightjar_core::log!(
                    "job {:?}: cannot check run {}: {e:#}",
                    spawned.job,
                    spawned.run_id
                );
                return;
            }
        };
        match &outcome {
            Ok(_) => self.prune_job_now(&spawned.job),
            Err(e) => nightjar_core::log!(
                "job {:?}: cannot record run {} as unknown: {e:#}",
                spawned.job,
                spawned.run_id
            ),
        }
    }

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
