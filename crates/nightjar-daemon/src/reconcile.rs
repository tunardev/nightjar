use crate::daemon::Daemon;
use crate::secs_i64;
use crate::tick::overlap_allows;
use anyhow::{Context, Result};
use jiff::Timestamp;
use nightjar_config::{Catchup, Job, Overlap};
use nightjar_core::limits::MAX_SLEEP;
use nightjar_store::run::Trigger;

/// The largest gap `tick` accepts as an ordinary tick of this process, not
/// an outage. Two `MAX_SLEEP`s of slack covers a slow tick, a busy store,
/// or a late scheduler.
///
/// This is this process's own cadence, so it applies only after it has
/// watched one interval go by. See `ordinary_tick_ceiling`.
pub(crate) const MAX_NORMAL_HEARTBEAT_GAP: std::time::Duration =
    std::time::Duration::from_secs(MAX_SLEEP.as_secs() * 2);

/// How long a pid-less `running` row is left alone by `reconcile`.
/// `runner::exec` writes the row before it forks, then fills in the pid.
/// A row pid-less this long means the wrapper died in that window.
const RECONCILE_PID_GRACE: std::time::Duration = std::time::Duration::from_secs(60);

impl Daemon {
    /// Marks every `running` row this process can't prove is alive as
    /// `unknown`. Left alone, such a row silences an `overlap = "skip"` job
    /// forever. `status` would still show it green.
    ///
    /// Runs every tick, not just at startup: a stale row can appear at any
    /// moment from a process this daemon never spawned.
    pub fn reconcile(&self) -> Result<usize> {
        let stale = self.store.running_runs()?;
        let now = self.clock.now();
        let mut reconciled = 0usize;
        for run in stale {
            if !self.is_provably_dead(&run, now) {
                continue;
            }
            // Not `finish_run`: `report_exec_exit` can reach the same row
            // in the same tick and must not overwrite it.
            let reason = match run.pid {
                Some(pid) => {
                    format!("nightjar exec (pid {pid}) is gone and never recorded an outcome")
                }
                None => "nightjar exec never recorded a pid or an outcome".to_string(),
            };
            if self.finish_unknown(&run.id, &reason)? {
                reconciled += 1;
                self.prune_job_now(&run.job);
            }
        }
        if reconciled > 0 {
            eprintln!("nightjar: reconciled {reconciled} stale run(s) as unknown");
        }
        Ok(reconciled)
    }

    /// Whether a `running` row belongs to a process this daemon can prove
    /// is gone. Biased toward "no": leaving a stale row alone is
    /// recoverable, declaring a live run dead is not.
    fn is_provably_dead(&self, run: &nightjar_store::run::Run, now: Timestamp) -> bool {
        // A run in `self.children` is alive by construction, pid on the
        // row or not. `reap` owns these.
        if self.children.iter().any(|c| c.run_id == run.id) {
            return false;
        }
        // The pid is the `nightjar exec` wrapper's own, not the job's. It
        // may briefly outlive a terminal row to dispatch alerts, so this
        // check only matters while the row is still `running`.
        match run.pid {
            Some(pid) => !process_is_alive(pid),
            // A pid-less row is a wrapper still inside the `start_run` /
            // `set_run_pid` window, or one that died there. Only age tells
            // them apart.
            None => now.as_second() - run.started_at.as_second() >= secs_i64(RECONCILE_PID_GRACE),
        }
    }

    /// The instant everything up to it is on record. `None` if no daemon
    /// has ever written to the store.
    ///
    /// Reads `caught_up_through`, falling back to `heartbeat_at` only for a
    /// store from before that column existed. Call this before `tick`
    /// writes anything, since `write_heartbeat` overwrites the column the
    /// fallback reads.
    pub(crate) fn accounted_through(&self) -> Result<Option<Timestamp>> {
        Ok(self
            .store
            .daemon_heartbeat()?
            .map(|beat| beat.caught_up_through.unwrap_or(beat.at)))
    }

    /// The start of an unaccounted-for gap ending at `now`, if there is one.
    ///
    /// No daemon row means no gap, not an infinite one. A watermark at or
    /// ahead of `now` is also not a gap: that's an NTP step backwards on a
    /// resuming machine.
    pub(crate) fn gap_since(
        &self,
        accounted: Option<Timestamp>,
        now: Timestamp,
    ) -> Option<Timestamp> {
        let accounted = accounted?;
        let elapsed = now.as_second() - accounted.as_second();
        (elapsed > self.ordinary_tick_ceiling()).then_some(accounted)
    }

    /// The longest elapsed stretch this tick may treat as an ordinary tick,
    /// not a gap.
    ///
    /// Zero until this process has watched one interval go by. Applying
    /// `MAX_NORMAL_HEARTBEAT_GAP`'s slack across a restart would swallow
    /// real gaps shorter than it, since a heartbeat is at most `MAX_SLEEP`
    /// old when its daemon dies.
    fn ordinary_tick_ceiling(&self) -> i64 {
        if self.has_watched_the_clock {
            secs_i64(MAX_NORMAL_HEARTBEAT_GAP)
        } else {
            0
        }
    }

    /// Accounts for every occurrence that elapsed inside a gap. `catchup`
    /// decides how many of them actually run; every one gets a row either way,
    /// `missed` or otherwise.
    ///
    /// Returns the names of jobs a make-up run was spawned for.
    pub(crate) fn catch_up_gap(
        &mut self,
        live: &[Job],
        since: Timestamp,
        now: Timestamp,
    ) -> Result<Vec<String>> {
        // Rows and watermark move as one unit: a daemon killed between them
        // replays the whole gap, and a watermark past a row-less occurrence
        // loses it for good. The heartbeat stays outside — a rollback that
        // took liveness with it made `status` report a daemon demonstrably
        // running jobs on schedule as not responding.
        let mut plans: Vec<Vec<String>> = Vec::with_capacity(live.len());
        {
            let txn = self.store.transaction()?;
            for job in live {
                plans.push(self.plan_catch_up(job, since, now)?);
            }
            self.store.set_caught_up_through(now)?;
            txn.commit()?;
        }

        // Latched at the commit, not on the `Result` below: the gap is
        // consumed once the watermark moves, so failures below are about a
        // gap that's already retired. Without this, the next ordinary wake
        // misreads as an outage.
        self.has_watched_the_clock = true;

        // Both loops visit every job even after a failure. Nothing else
        // will revisit these occurrences, so a bare `?` would abandon every
        // job after the failing one.
        //
        // These failures are per-job, not the whole function's: the
        // transaction above already committed, so the gap is retired
        // either way. That's why they're logged, not returned.
        //
        // Re-armed from `now`: everything up to it was just accounted for.
        // Leaving `next_run_at` in the past would make `evaluate` fire the
        // same occurrence again later in this tick.
        for job in live {
            if let Err(e) = self.rearm(job, now) {
                eprintln!(
                    "nightjar: job {:?}: cannot re-arm after catch-up: {e:#}",
                    job.name
                );
            }
        }

        // Only after the commit. The child writes its own `start_run` row
        // on a separate connection and must not contend with the write
        // transaction held here.
        let mut fired = Vec::new();
        for (job, held) in live.iter().zip(&plans) {
            match self.spawn_make_up_runs(job, held) {
                Ok(n) if n > 0 => fired.push(job.name.clone()),
                Ok(_) => {}
                Err(e) => eprintln!("nightjar: {e:#}"),
            }
        }

        Ok(fired)
    }

    /// Walks `job`'s occurrences one at a time via `next_after`, never
    /// collecting them into a `Vec`. A per-second schedule down for a week
    /// is 604,800 of them.
    ///
    /// Writes a `missed` row for every occurrence, including ones held
    /// back to run later. They're written now, not after commit, because
    /// the watermark would otherwise pass them with nothing on record.
    ///
    /// Takes `&self`: `catch_up_gap` still borrows `self.store` in its
    /// transaction when this runs.
    fn plan_catch_up(&self, job: &Job, since: Timestamp, now: Timestamp) -> Result<Vec<String>> {
        // A triggered job has no schedule to catch up on. It only fires
        // from its parent's own success.
        let Some(schedule) = &job.schedule else {
            return Ok(Vec::new());
        };

        let mut budget = match job.catchup {
            Catchup::None => 0,
            Catchup::Once => 1,
            Catchup::All => self.config.catchup_max,
        };

        // `spawn` forks and returns immediately, so back-to-back make-up
        // runs would be in flight together. Capping the budget at one is
        // what makes catch-up honor `skip`/`queue`.
        if !matches!(job.overlap, Overlap::Parallel) {
            budget = budget.min(1);
        }

        // Checked once, not per spawn: a forked child's `start_run` lands
        // arbitrarily later, so re-reading would see nothing new. `tick`
        // reconciles before catch-up, so a stale `running` row can't read
        // as "in flight" here.
        //
        // Routed through `overlap_allows`, not a second check of its own,
        // so `skip`/`queue`/`parallel` keeps one shared definition.
        if budget > 0 && !matches!(job.overlap, Overlap::Parallel) {
            let in_flight = self.store.running_count(&job.name)?;
            if !overlap_allows(job.overlap, in_flight) {
                budget = 0;
            }
        }

        // Occurrences run by recency, not discovery order: a backup or a
        // sync is more useful caught up from `now` backward than from the
        // start of the outage forward. The window holds the newest
        // `budget` occurrences seen so far. Anything evicted from it is
        // recorded `missed` immediately, since it can never re-enter.
        let mut cursor = since;
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
            eprintln!(
                "nightjar: job {:?}: catch-up recorded {missed} missed",
                job.name
            );
        }

        // At most `budget` rows. The streaming walk above is the only
        // thing that scales with the length of the gap.
        window
            .into_iter()
            .map(|occurrence| self.record_missed(job, occurrence, Trigger::Catchup))
            .collect()
    }

    /// Starts one make-up run per id `plan_catch_up` held back. Returns how
    /// many started.
    ///
    /// Each run reuses the id of its committed `missed` row, so the
    /// child's `start_run` supersedes that row instead of adding a new
    /// one. A failed spawn needs no compensating write: that occurrence,
    /// and every one still queued, is already `missed` on record.
    fn spawn_make_up_runs(&mut self, job: &Job, held: &[String]) -> Result<usize> {
        for (started, run_id) in held.iter().enumerate() {
            let Err(e) = self.spawn_as(job, run_id.clone(), Trigger::Catchup) else {
                continue;
            };
            eprintln!(
                "nightjar: job {:?}: catch-up spawned {started}, then failed; the remaining {} \
                 occurrence(s) stay missed: {e:#}",
                job.name,
                held.len() - started
            );
            return Err(e).with_context(|| format!("catch-up spawn for job {:?}", job.name));
        }
        if !held.is_empty() {
            eprintln!(
                "nightjar: job {:?}: catch-up spawned {}",
                job.name,
                held.len()
            );
        }
        Ok(held.len())
    }

    /// Records one occurrence nothing ran. Returns its row id. Stamped
    /// with the occurrence's own time, not `now`, so the row says when the
    /// job should have run.
    ///
    /// Takes `&self`: `plan_catch_up` calls this while its transaction
    /// still borrows `self.store`.
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

/// A pid that can't be signalled belongs to a process that's gone. The
/// converse doesn't hold, since pids get recycled: this only ever proves
/// death, never life.
///
/// Known gap: a zombie still answers `kill(pid, 0)`, so a finished but
/// unreaped run reads as alive. This self-heals once it's reaped, and
/// never wrongly declares a live run dead.
fn process_is_alive(pid: u32) -> bool {
    match signalable_pid(pid) {
        Some(p) => (unsafe { libc::kill(p, 0) }) == 0,
        None => false,
    }
}

/// Pids a run this daemon spawned could plausibly have. Excludes 0: `kill`
/// reads that as the caller's process group, so it always "succeeds" no
/// matter what the row names. Excludes 1 (init/launchd) too, since a
/// corrupt row naming it would block `overlap = "skip"` forever.
///
/// Split out from `process_is_alive` so the boundary can be tested
/// without `kill`, whose pid-1 answer depends on whether the test runs
/// as root.
fn signalable_pid(pid: u32) -> Option<libc::pid_t> {
    match libc::pid_t::try_from(pid) {
        Ok(p) if p > 1 => Some(p),
        _ => None,
    }
}

#[cfg(test)]
mod process_is_alive_tests {
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
