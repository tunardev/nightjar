use anyhow::{Context, Result};
use jiff::Timestamp;
use nightjar_config::{Catchup, Job, Overlap};
use nightjar_core::format::quantity;
use nightjar_core::limits::MAX_SLEEP;
use nightjar_store::run::Trigger;

use crate::daemon::Daemon;
use crate::secs_i64;
use crate::tick::overlap_allows;

pub const MAX_NORMAL_HEARTBEAT_GAP: std::time::Duration =
    std::time::Duration::from_secs(MAX_SLEEP.as_secs() * 2);

const RECONCILE_PID_GRACE: std::time::Duration = std::time::Duration::from_secs(60);

impl Daemon {
    /// # Errors
    /// fails if the running runs cannot be read, or a dead one cannot be marked unknown
    pub fn reconcile(&self) -> Result<usize> {
        let stale = self.store.running_runs()?;
        let now = self.clock.now();
        let mut reconciled = 0usize;
        for run in stale {
            if !self.is_provably_dead(&run, now) {
                continue;
            }
            let reason = run.pid.map_or_else(
                || "nightjar exec never recorded a pid or an outcome".to_string(),
                |pid| format!("nightjar exec (pid {pid}) is gone and never recorded an outcome"),
            );
            if self.finish_unknown(&run.id, &reason)? {
                reconciled += 1;
                self.prune_job_now(&run.job);
            }
        }
        if reconciled > 0 {
            nightjar_core::log!(
                "reconciled {} as unknown",
                quantity(reconciled, "stale run", "stale runs")
            );
        }
        Ok(reconciled)
    }

    fn is_provably_dead(&self, run: &nightjar_store::run::Run, now: Timestamp) -> bool {
        if self.children.iter().any(|c| c.run_id == run.id) {
            return false;
        }
        run.pid.map_or_else(
            || now.as_second() - run.started_at.as_second() >= secs_i64(RECONCILE_PID_GRACE),
            |pid| !process_is_alive(pid),
        )
    }

    pub(crate) fn accounted_through(&self) -> Result<Option<Timestamp>> {
        Ok(self
            .store
            .daemon_heartbeat()?
            .map(|beat| beat.caught_up_through.unwrap_or(beat.at)))
    }

    pub(crate) fn pin_accounted_through(&self) -> Result<Option<Timestamp>> {
        let accounted = self.accounted_through()?;
        if let Some(at) = accounted {
            self.store.set_caught_up_through(at)?;
        }
        Ok(accounted)
    }

    pub(crate) fn gap_since(
        &self,
        accounted: Option<Timestamp>,
        now: Timestamp,
    ) -> Option<Timestamp> {
        let accounted = accounted?;
        let elapsed = now.as_second() - accounted.as_second();
        (elapsed > self.ordinary_tick_ceiling()).then_some(accounted)
    }

    fn ordinary_tick_ceiling(&self) -> i64 {
        if self.has_watched_the_clock {
            secs_i64(MAX_NORMAL_HEARTBEAT_GAP)
        } else {
            0
        }
    }

    pub(crate) fn catch_up_gap(
        &mut self,
        live: &[Job],
        since: Timestamp,
        now: Timestamp,
    ) -> Result<Vec<String>> {
        let mut held_back: Vec<(&Job, Vec<String>)> = Vec::with_capacity(live.len());
        {
            let txn = self.store.transaction()?;
            for job in live {
                let from = self.catch_up_floor(job, since);
                held_back.push((job, self.record_gap_occurrences(job, from, now)?));
            }
            self.store.set_caught_up_through(now)?;
            txn.commit()?;
        }

        self.has_watched_the_clock = true;

        for job in live {
            self.memory_for(&job.name).accounted_through = Some(now);
            if let Err(e) = self.rearm(job, now) {
                nightjar_core::log!("job {:?}: cannot re-arm after catch-up: {e:#}", job.name);
            }
        }

        let mut fired = Vec::new();
        for (job, run_ids) in held_back {
            match self.spawn_make_up_runs(job, &run_ids) {
                Ok(n) if n > 0 => fired.push(job.name.clone()),
                Ok(_) => {}
                Err(e) => nightjar_core::log!("{e:#}"),
            }
        }

        Ok(fired)
    }

    fn catch_up_floor(&self, job: &Job, gap_start: Timestamp) -> Timestamp {
        self.job_memory
            .get(&job.name)
            .and_then(|m| m.accounted_through)
            .map_or(gap_start, |accounted| gap_start.max(accounted))
    }

    fn record_gap_occurrences(
        &self,
        job: &Job,
        accounted_through: Timestamp,
        now: Timestamp,
    ) -> Result<Vec<String>> {
        let Some(schedule) = &job.schedule else {
            return Ok(Vec::new());
        };

        let budget = self.make_up_budget(job)?;

        let mut cursor = accounted_through;
        let mut window: std::collections::VecDeque<Timestamp> = std::collections::VecDeque::new();
        let mut missed = 0usize;

        loop {
            let Some(occurrence) = schedule.next_after(cursor, &self.tz)? else {
                break;
            };
            if occurrence > now {
                break;
            }
            cursor = occurrence;

            window.push_back(occurrence);
            if window.len() > budget {
                let evicted = window.pop_front().expect("just grew past budget");
                self.record_missed(job, evicted, Trigger::Catchup)?;
                missed += 1;
            }
        }

        if missed > 0 {
            nightjar_core::log!("job {:?}: catch-up recorded {missed} missed", job.name);
        }

        window
            .into_iter()
            .map(|occurrence| self.record_missed(job, occurrence, Trigger::Catchup))
            .collect()
    }

    fn make_up_budget(&self, job: &Job) -> Result<usize> {
        let ceiling = match job.catchup {
            Catchup::None => 0,
            Catchup::Once => 1,
            Catchup::All => self.config.catchup_max,
        };
        if matches!(job.overlap, Overlap::Parallel) {
            return Ok(ceiling);
        }
        let one_at_a_time = ceiling.min(1);
        if one_at_a_time > 0 && !overlap_allows(job.overlap, self.in_flight_count(&job.name)?) {
            return Ok(0);
        }
        Ok(one_at_a_time)
    }

    fn spawn_make_up_runs(&mut self, job: &Job, held_back: &[String]) -> Result<usize> {
        for (started, run_id) in held_back.iter().enumerate() {
            let Err(e) = self.spawn_as(job, run_id.clone(), Trigger::Catchup) else {
                continue;
            };
            nightjar_core::log!(
                "job {:?}: catch-up spawned {started}, then failed, leaving {} missed: {e:#}",
                job.name,
                quantity(held_back.len() - started, "occurrence", "occurrences")
            );
            return Err(e).with_context(|| format!("catch-up spawn for job {:?}", job.name));
        }
        if !held_back.is_empty() {
            nightjar_core::log!("job {:?}: catch-up spawned {}", job.name, held_back.len());
        }
        Ok(held_back.len())
    }

    pub(crate) fn record_missed(
        &self,
        job: &Job,
        occurrence: Timestamp,
        trigger: Trigger,
    ) -> Result<String> {
        let run_id = uuid::Uuid::now_v7().to_string();
        self.store
            .record_missed_run(&run_id, &job.name, trigger, occurrence)
    }
}

fn process_is_alive(pid: u32) -> bool {
    signalable_pid(pid).is_some_and(|p| (unsafe { libc::kill(p, 0) }) == 0)
}

fn signalable_pid(pid: u32) -> Option<libc::pid_t> {
    match libc::pid_t::try_from(pid) {
        Ok(p) if p > 1 => Some(p),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::{process_is_alive, signalable_pid};

    #[test]
    fn our_own_pid_is_alive() {
        assert!(process_is_alive(std::process::id()));
    }

    #[test]
    fn pid_zero_is_never_treated_as_alive() {
        assert!(!process_is_alive(0));
    }

    #[test]
    fn pid_is_treated_as_dead_when_it_cannot_exist_on_a_real_os() {
        assert!(!process_is_alive(u32::MAX));
    }

    #[test]
    fn pid_one_and_zero_are_never_signalable_even_though_pid_one_is_always_alive() {
        assert_eq!(signalable_pid(0), None);
        assert_eq!(signalable_pid(1), None);
    }

    #[test]
    fn pid_is_signalable_when_it_is_ordinary() {
        assert_eq!(signalable_pid(2), Some(2));
        assert_eq!(
            signalable_pid(std::process::id()),
            Some(libc::pid_t::try_from(std::process::id()).unwrap())
        );
    }

    #[test]
    fn pid_is_not_signalable_when_it_is_too_large_for_pid_t() {
        assert_eq!(signalable_pid(u32::MAX), None);
    }
}
