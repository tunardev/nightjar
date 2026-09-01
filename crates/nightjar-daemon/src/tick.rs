use crate::daemon::Daemon;
use crate::reconcile::MAX_NORMAL_HEARTBEAT_GAP;
use crate::{secs_i64, sleep_unless_stopping, stop_requested};
use anyhow::{Context, Result};
use jiff::Timestamp;
use nightjar_config::job::{JobsDirState, probe_jobs_dir};
use nightjar_config::{Job, Overlap};
use nightjar_core::limits::MAX_SLEEP;
use nightjar_runner::exec::cooldown_expired;
use nightjar_runner::notify::Alert;
use nightjar_store::run::Trigger;
use nightjar_store::{Store, overdue_since};
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};

/// Minimum pause after a failed tick, not `sleep_for`. A failed tick may
/// not have advanced `self.next_run_at`, so `sleep_for` would return zero
/// and spin the loop.
const ERROR_BACKOFF: std::time::Duration = std::time::Duration::from_secs(1);

impl Daemon {
    /// One scheduling pass. Returns the names of jobs a run was started for.
    ///
    /// Reloads the jobs directory every tick instead of watching it. The
    /// loop already wakes at least every `MAX_SLEEP`, and reading a
    /// handful of small files is cheaper than a platform-specific watcher.
    ///
    /// The watermark comparison at the start of each pass covers both ways
    /// a gap arises: a restarted daemon, or a laptop that suspended this
    /// process and resumed it. The store can't tell those apart.
    pub fn tick(&mut self) -> Result<Vec<String>> {
        self.reap();
        let now = self.clock.now();

        // Read before anything overwrites it. The stored watermark is the
        // only evidence a gap existed.
        let accounted = self.accounted_through()?;
        let gap_since = self.gap_since(accounted, now);

        // On an old store, the watermark is NULL and `accounted_through`
        // falls back to `heartbeat_at`, which the write below overwrites.
        // Pinning it into its own column first keeps that evidence alive,
        // so a tick that fails here still leaves the same window to be
        // found on the next pass.
        if let Some(at) = accounted {
            self.store.set_caught_up_through(at)?;
        }

        // Liveness, written whether or not a gap was found: `status` tells
        // "no runs yet" from "nothing is scheduling these jobs" apart
        // using only this row. Must come after the pin above, or a
        // heartbeat of `now` over a NULL watermark reads, through the
        // fallback, as a store with no gap at all.
        self.store
            .write_heartbeat(now, std::process::id(), env!("CARGO_PKG_VERSION"))?;

        // Before catch-up: a stale `running` row would otherwise read as
        // "in flight" and suppress the one make-up run the overlap policy
        // allows. Logged, not propagated: maintenance must not stop a tick.
        if let Err(e) = self.reconcile() {
            eprintln!("nightjar: reconcile failed: {e:#}");
        }

        // `Job::load_all` returns an empty `Vec` for both a missing and an
        // unreadable directory. A permissions problem or a bad mount would
        // otherwise silently wipe every armed schedule via the `retain` below.
        let dir_state = probe_jobs_dir(&self.paths.jobs_dir)?;
        let loaded = match dir_state {
            JobsDirState::Missing => Vec::new(),
            JobsDirState::Present => Job::load_all(&self.paths.jobs_dir),
        };

        // Every name with a file on disk, parsing or not. A job with a
        // typo still exists, and the sweep below must not treat its
        // `job_state` row as a deleted job's.
        let on_disk_names: Vec<String> = loaded.iter().map(|(n, _)| n.clone()).collect();

        let live: Vec<Job> = loaded
            .into_iter()
            .filter_map(|(_, r)| r.ok())
            .filter(|j| j.enabled)
            .collect();

        for warning in newly_seen(
            &mut self.logged_warnings,
            live.iter().flat_map(|j| j.warnings.iter()),
        ) {
            eprintln!("nightjar: {warning}");
        }

        let live_names: std::collections::HashSet<&str> =
            live.iter().map(|j| j.name.as_str()).collect();
        self.next_run_at
            .retain(|k, _| live_names.contains(k.as_str()));
        self.overdue_last_attempt
            .retain(|k, _| live_names.contains(k.as_str()));

        // See `check_overdue`'s own doc for why this must run from exactly
        // this position, before catch-up or `evaluate` can re-arm anything.
        self.check_overdue(&live, now);

        // Before `evaluate`. An occurrence `evaluate` can't fit this tick
        // still goes to the back of the same queue, so draining first
        // keeps it FIFO instead of always losing to whatever just became
        // due.
        self.drain_queues(&live);

        // After `reap` (top of this tick) has had its chance to see a
        // parent finish. A run that succeeded moments ago fires its child
        // this pass, not next.
        self.fire_after_triggers(&live);

        // After the triggers above have stamped every handled parent.
        // Retention spares an unstamped success (see `Store::prune_runs`),
        // so sweeping first would leave those rows for the next sweep.
        if self.sweep_due(now) {
            self.sweep(dir_state, &on_disk_names);
            self.last_sweep = Some(now);
        }

        let mut fired = Vec::new();
        let mut failure: Option<anyhow::Error> = None;
        // Distinct from `failure`. A tick's return value goes `Err` only
        // for a failure that isn't one job's own problem. The per-job loop
        // below never sets this, so a permanently broken job can't push
        // `Daemon::run` into ever-growing backoff.
        let mut systemic_failure = false;

        if let Some(since) = gap_since {
            // The rest of this tick still runs after a failure here. A
            // daemon whose store is failing must degrade loudly, not stop
            // scheduling.
            //
            // A post-commit failure reports on a gap already retired, so
            // `has_watched_the_clock` latches at the commit, not here.
            match self.catch_up_gap(&live, since, now) {
                Ok(names) => fired.extend(names),
                Err(e) => {
                    eprintln!("nightjar: catch-up failed: {e:#}");
                    systemic_failure = true;
                    failure.get_or_insert(e);
                }
            }
        }

        // Everything up to here already has a row, so `evaluate` must not
        // write a second one for the same occurrence. That's the
        // tick-start watermark on an ordinary pass, or `now` once
        // `catch_up_gap` ran: it either accounts for the whole gap, or
        // rolls back and leaves nothing for `evaluate` to retire either.
        let recorded_through = match gap_since {
            Some(_) => now,
            None => accounted.unwrap_or(now),
        };

        for job in &live {
            // One job's failure must not stop the others in this tick from
            // being evaluated. It's still remembered and reported once
            // every job has had its turn.
            match self.evaluate(job, now, recorded_through) {
                Ok(Some(name)) => fired.push(name),
                Ok(None) => {}
                Err(e) => {
                    eprintln!("nightjar: job {:?}: {e:#}", job.name);
                    failure.get_or_insert(e);
                }
            }
        }

        // Last, and only over occurrences this pass put on record: every
        // live job has fired, been recorded `missed`, or is armed past
        // `now`. A job this tick couldn't finish leaves the watermark
        // where it was, so the window is re-read, not retired unrecorded —
        // the same invariant `catch_up_gap` holds in its transaction.
        if failure.is_none() {
            match self.store.set_caught_up_through(now) {
                Ok(()) => self.has_watched_the_clock = true,
                Err(e) => {
                    systemic_failure = true;
                    failure = Some(e.context("advancing the catch-up watermark"));
                }
            }
        }

        fired.sort();
        match failure {
            Some(e) if systemic_failure => {
                Err(e).context("one or more jobs failed to evaluate this tick")
            }
            _ => Ok(fired),
        }
    }

    /// Handles every successful run whose `after` children are still owed
    /// something (the set `unfired_successful_runs` returns).
    ///
    /// A run this daemon saw finish fires its children now. A run that
    /// finished before this process started means a daemon died in
    /// between: those children are recorded `missed` instead, since
    /// running a job nobody chose is worse than not running it.
    ///
    /// Either way, the parent is stamped handled exactly once.
    fn fire_after_triggers(&mut self, live: &[Job]) {
        let parents = match self.store.unfired_successful_runs() {
            Ok(p) => p,
            Err(e) => {
                eprintln!("nightjar: after: cannot read unfired successful runs: {e:#}");
                return;
            }
        };

        for parent in parents {
            // `finished_at` is always `Some` here: `unfired_successful_runs`
            // filters on it. A missing one would mean "not finished", not
            // this pass's problem either way.
            let Some(finished_at) = parent.finished_at else {
                continue;
            };
            let lost_to_a_restart = finished_at < self.started_at;

            for child in live
                .iter()
                .filter(|c| c.after.as_deref() == Some(&*parent.job))
            {
                if lost_to_a_restart {
                    eprintln!(
                        "nightjar: job {:?}: its trigger from {:?} was lost to a daemon \
                         restart; recording it missed rather than running it now",
                        child.name, parent.job
                    );
                    if let Err(e) =
                        self.record_missed(child, finished_at, Trigger::After(parent.job.clone()))
                    {
                        eprintln!(
                            "nightjar: job {:?}: cannot record the lost trigger missed: {e:#}",
                            child.name
                        );
                    }
                    continue;
                }

                let in_flight = match self.store.running_count(&child.name) {
                    Ok(n) => n,
                    Err(e) => {
                        eprintln!(
                            "nightjar: job {:?}: cannot check in-flight count: {e:#}",
                            child.name
                        );
                        continue;
                    }
                };
                // The same policy a scheduled occurrence follows: being
                // triggered isn't a license to run concurrently with itself.
                if !overlap_allows(child.overlap, in_flight) {
                    eprintln!(
                        "nightjar: job {:?}: triggered by {:?} but skipped, \
                         {in_flight} run(s) already in flight",
                        child.name, parent.job
                    );
                    if let Err(e) =
                        self.record_missed(child, finished_at, Trigger::After(parent.job.clone()))
                    {
                        eprintln!(
                            "nightjar: job {:?}: cannot record the skipped trigger: {e:#}",
                            child.name
                        );
                    }
                    continue;
                }

                let run_id = uuid::Uuid::now_v7().to_string();
                if let Err(e) = self.spawn_as(child, run_id, Trigger::After(parent.job.clone())) {
                    eprintln!("nightjar: job {:?}: cannot spawn: {e:#}", child.name);
                }
            }

            // Last, and unconditional. A parent left unstamped would have
            // every following tick reach the same decision again.
            if let Err(e) = self.store.set_after_fired_at(&parent.id, self.clock.now()) {
                eprintln!(
                    "nightjar: job {:?}: cannot mark run {} handled: {e:#}",
                    parent.job, parent.id
                );
            }
        }
    }

    /// Starts the oldest queued occurrence of every live job whose
    /// in-flight run has since finished. Scoped to `live`, not every job
    /// with anything queued: a disabled or deleted job must not have the
    /// daemon spawn it behind the user's back. Failures are logged, not
    /// propagated: a dequeue this tick can't manage still sits there for
    /// the next one to retry.
    fn drain_queues(&mut self, live: &[Job]) {
        for job in live {
            let queued = match self.store.queued_count(&job.name) {
                Ok(n) => n,
                Err(e) => {
                    eprintln!(
                        "nightjar: queue: job {:?}: cannot check queue: {e:#}",
                        job.name
                    );
                    continue;
                }
            };
            if queued == 0 {
                continue;
            }
            let in_flight = match self.store.running_count(&job.name) {
                Ok(n) => n,
                Err(e) => {
                    eprintln!(
                        "nightjar: queue: job {:?}: cannot check in-flight count: {e:#}",
                        job.name
                    );
                    continue;
                }
            };
            if !overlap_allows(job.overlap, in_flight) {
                continue;
            }
            match self.store.dequeue_oldest(&job.name) {
                Ok(Some((run_id, _due_at))) => {
                    if let Err(e) = self.spawn_as(job, run_id, Trigger::Schedule) {
                        eprintln!(
                            "nightjar: queue: job {:?}: cannot spawn dequeued run: {e:#}",
                            job.name
                        );
                    }
                }
                Ok(None) => {}
                Err(e) => eprintln!("nightjar: queue: job {:?}: cannot dequeue: {e:#}", job.name),
            }
        }
    }

    /// Isolated from `tick`'s loop, so one job's error can't block the
    /// others from being scheduled this pass.
    ///
    /// At most one occurrence runs per pass. `rearm` retires the rest of
    /// the window with it. Returning `Ok` lets `tick` move the watermark
    /// past `now`, so everything retired without running gets `missed`
    /// first — behind the watermark, nothing looks again.
    fn evaluate(
        &mut self,
        job: &Job,
        now: Timestamp,
        recorded_through: Timestamp,
    ) -> Result<Option<String>> {
        let Some(&due_at) = self.next_run_at.get(&job.name) else {
            // First sighting: arm it, never fire on the same tick. Its
            // earlier occurrences aren't this daemon's to account for: the
            // job was absent, disabled, or unparseable for all of them.
            self.rearm(job, now)?;
            return Ok(None);
        };

        if due_at > now {
            return Ok(None);
        }

        let in_flight = self.store.running_count(&job.name)?;
        let allowed = overlap_allows(job.overlap, in_flight);

        if !allowed {
            // `running_count` counts rows nobody finished, not live
            // processes. `tick` reconciles every pass, so a row whose
            // process is gone can't reach this branch.
            eprintln!(
                "nightjar: job {:?}: skipped, {in_flight} run(s) already in flight",
                job.name
            );
            self.rearm(job, now)?;
            // The due occurrence itself, excluded from the shared lower
            // bound below: skipping loses it silently, and a stderr line
            // alone won't show in `status`, `list`, or `logs`.
            //
            // `self.next_run_at`, not the watermark, says what's already
            // accounted for. A watermark another job's failure held back
            // must not make this job record twice.
            if due_at > recorded_through {
                // `overlap = "queue"` gets one more chance: held in
                // `queued_runs`, up to `queue_depth` deep, for
                // `drain_queues` to start once the slot frees. Past that
                // cap, it's `missed`, like `skip` always is.
                let queued = job.overlap == Overlap::Queue
                    && self.store.queued_count(&job.name)? < self.config.queue_depth;
                if queued {
                    self.store.enqueue_run(&job.name, due_at)?;
                } else {
                    self.record_missed(job, due_at, Trigger::Schedule)?;
                }
            }
            self.retire(job, recorded_through.max(due_at), now)?;
            self.prune_job_now(&job.name);
            return Ok(None);
        }

        // Re-arm only after `spawn` succeeds. A failed spawn leaves no run
        // row behind, so leaving `next_run_at` unadvanced is what makes the
        // occurrence due again instead of vanishing. The held watermark
        // hands it to catch-up if the failure persists.
        self.spawn(job, Trigger::Schedule)?;
        self.rearm(job, now)?;

        // Last in both branches. Until the schedule moves, a retry after a
        // failure here would walk the same occurrences twice.
        self.retire(job, recorded_through.max(due_at), now)?;
        self.prune_job_now(&job.name);
        Ok(Some(job.name.clone()))
    }

    /// Records every occurrence of `job` in `(after, now]` as `missed`.
    /// Returns how many there were.
    ///
    /// Streamed through `next_after` one at a time, like `plan_catch_up`.
    /// An ordinary tick's window is bounded by `MAX_NORMAL_HEARTBEAT_GAP`,
    /// though nothing in the signature says so.
    fn retire(&self, job: &Job, after: Timestamp, now: Timestamp) -> Result<usize> {
        // A triggered job has no schedule of its own to retire occurrences
        // against.
        let Some(schedule) = &job.schedule else {
            return Ok(0);
        };

        let mut cursor = after;
        let mut retired = 0usize;
        loop {
            let Some(occurrence) = schedule.next_after(cursor, &self.tz)? else {
                break;
            };
            if occurrence > now {
                break;
            }
            cursor = occurrence;
            self.record_missed(job, occurrence, Trigger::Schedule)?;
            retired += 1;
        }
        if retired > 0 {
            eprintln!(
                "nightjar: job {:?}: recorded {retired} occurrence(s) missed",
                job.name
            );
        }
        Ok(retired)
    }

    pub(crate) fn rearm(&mut self, job: &Job, now: Timestamp) -> Result<()> {
        // A triggered job has no schedule, so it has no next occurrence.
        // That's the same "won't fire again" path an exhausted one-shot
        // schedule already takes below.
        let next = match &job.schedule {
            Some(s) => s.next_after(now, &self.tz)?,
            None => None,
        };
        match next {
            Some(t) => {
                self.next_run_at.insert(job.name.clone(), t);
            }
            None => {
                self.next_run_at.remove(&job.name);
            }
        }
        // `status` reads only the store, so the daemon's intent must live
        // there too, or a stopped daemon leaves `status` unable to tell an
        // imminent run from one that will never happen.
        //
        // Memory first, store second: a failing `set_next_run` shows a
        // false OVERDUE until the next write succeeds. The reverse order
        // would fire a spawned occurrence twice.
        self.store.set_next_run(&job.name, next)?;
        Ok(())
    }

    fn sleep_for(&self) -> std::time::Duration {
        let ceiling = self.config.heartbeat_interval.min(MAX_SLEEP);
        let now = self.clock.now();
        let soonest = self.next_run_at.values().min().copied();
        match soonest {
            Some(t) => {
                let secs = u64::try_from((t.as_second() - now.as_second()).max(0)).unwrap_or(0);
                std::time::Duration::from_secs(secs).min(ceiling)
            }
            None => ceiling,
        }
    }

    pub fn run(&mut self) -> Result<()> {
        loop {
            if stop_requested() {
                eprintln!("nightjar: daemon stopping");
                return Ok(());
            }
            match self.tick() {
                Ok(fired) => {
                    self.consecutive_failures = 0;
                    for name in fired {
                        eprintln!("nightjar: started {name}");
                    }
                    sleep_unless_stopping(self.sleep_for());
                }
                Err(e) => {
                    // Deliberately not `sleep_for()`. See `ERROR_BACKOFF`.
                    // The shift is capped separately so the doubling can't
                    // overflow.
                    self.consecutive_failures = self.consecutive_failures.saturating_add(1);
                    eprintln!("nightjar: tick failed: {e:#}");
                    let shift = self.consecutive_failures.min(5);
                    let backoff = (ERROR_BACKOFF * (1u32 << shift)).min(MAX_SLEEP);
                    sleep_unless_stopping(backoff);
                }
            }
        }
    }
}

/// Returns the entries of `current` not in `seen`, then replaces `seen`
/// with `current`. An entry that disappears and later returns is reported
/// again: the condition it describes really did recur.
fn newly_seen<'a>(
    seen: &mut HashSet<String>,
    current: impl Iterator<Item = &'a String>,
) -> Vec<String> {
    let current: HashSet<String> = current.cloned().collect();
    let mut fresh: Vec<String> = current.difference(seen).cloned().collect();
    fresh.sort();
    *seen = current;
    fresh
}

/// Whether a new run of a job may start, given how many are already in
/// flight. Shared by `evaluate` and `nightjar run`, so a manual invocation
/// can't start a second instance a `skip` or `queue` schedule would have
/// refused.
///
/// `queue` behaves like `skip` until persistent queue state exists.
/// `parallel` here would let a slow job pile up unbounded.
pub fn overlap_allows(overlap: Overlap, in_flight: usize) -> bool {
    match overlap {
        Overlap::Parallel => true,
        Overlap::Skip | Overlap::Queue => in_flight == 0,
    }
}

/// How many `missed` rows `sweep` keeps per job, separate from
/// `Config::retention_runs`. `record_missed` stamps each row with its
/// occurrence's own time, so a gap's rows are always newer than real
/// runs. A shared keep-count would let them evict a job's entire history,
/// output files included. Not itself user-configurable.
const RETENTION_MISSED: usize = 50;

/// How often `tick` performs a retention sweep. Pruning has no per-tick
/// urgency. Doing it as often as `tick` runs would be needless I/O for a
/// bound that only matters over months.
const RETENTION_SWEEP: std::time::Duration = std::time::Duration::from_secs(3600);

/// How long a freshly started daemon waits before its first sweep. Far
/// shorter than `RETENTION_SWEEP`, so a daemon restarting more often than
/// an hour (crash loops, upgrades, container churn) still sweeps at all.
const RETENTION_STARTUP_DEFER: std::time::Duration = std::time::Duration::from_secs(300);

impl Daemon {
    pub(crate) fn sweep_due(&self, now: Timestamp) -> bool {
        match self.last_sweep {
            None => {
                now.as_second() - self.started_at.as_second() >= secs_i64(RETENTION_STARTUP_DEFER)
            }
            Some(last) => now.as_second() - last.as_second() >= secs_i64(RETENTION_SWEEP),
        }
    }

    /// Prunes every job's run history and unlinks output files that fall
    /// out of the keep window. Driven by the store's run history, not the
    /// jobs directory: a job removed from disk still has years of output
    /// files nothing else will ever prune.
    ///
    /// Failures are logged, not propagated. Retention is maintenance and
    /// must not stop this tick's jobs from being evaluated.
    pub(crate) fn sweep(&self, dir_state: JobsDirState, on_disk: &[String]) {
        match self.store.distinct_run_jobs() {
            Ok(jobs) => {
                for job in jobs {
                    self.prune_job_now(&job);
                }
            }
            Err(e) => eprintln!("nightjar: retention: cannot list jobs: {e:#}"),
        }

        // `Missing` must never mean "no jobs exist". An unreadable or
        // unmounted directory looks identical to every job being deleted,
        // and would wipe every `job_state` row over a transient
        // permissions problem.
        if matches!(dir_state, JobsDirState::Present) {
            self.prune_orphaned_job_state(on_disk);
        }
    }

    /// Prunes one job's run history to its configured bounds right now.
    /// Closes the window a busy job could otherwise outgrow
    /// `retention_runs`/`retention_age` between hourly sweeps. Failures
    /// are logged, not propagated, like `sweep`.
    pub(crate) fn prune_job_now(&self, job: &str) {
        let cutoff =
            self.clock
                .now()
                .checked_sub(jiff::Span::new().seconds(
                    i64::try_from(self.config.retention_age.as_secs()).unwrap_or(i64::MAX),
                ))
                .ok();
        match self
            .store
            .prune_runs(job, self.config.retention_runs, RETENTION_MISSED)
        {
            Ok(orphaned) => self.unlink_orphaned(&orphaned),
            Err(e) => eprintln!("nightjar: retention: job {job:?}: prune failed: {e:#}"),
        }
        // After the keep-counts, not instead of them. The two bounds are
        // independent: a row breaching either one goes.
        if let Some(cutoff) = cutoff {
            match self.store.prune_older_than(job, cutoff) {
                Ok(orphaned) => self.unlink_orphaned(&orphaned),
                Err(e) => {
                    eprintln!("nightjar: retention: job {job:?}: age prune failed: {e:#}");
                }
            }
        }
    }

    /// Deletes `job_state` for any job with no file in the jobs directory.
    /// Without this, a job recreated under a deleted one's filename reads
    /// the deleted job's `next_run_at` until the next tick overwrites it —
    /// a false OVERDUE in the meantime.
    fn prune_orphaned_job_state(&self, on_disk: &[String]) {
        let states = match self.store.all_job_states() {
            Ok(s) => s,
            Err(e) => {
                eprintln!("nightjar: retention: cannot list job_state: {e:#}");
                return;
            }
        };
        for state in states {
            if on_disk.iter().any(|n| n == &state.job) {
                continue;
            }
            if let Err(e) = self.store.delete_job_state(&state.job) {
                eprintln!(
                    "nightjar: retention: job {:?}: cannot delete job_state: {e:#}",
                    state.job
                );
            }
        }
    }

    /// Unlinks the output files `prune_runs` reports as orphaned.
    /// `NotFound` is expected and silent, since a file may already be
    /// gone. Anything else is logged without aborting the sweep or tick.
    fn unlink_orphaned(&self, paths: &[PathBuf]) {
        for path in paths {
            if !is_within_runs_dir(path, &self.paths.runs_dir) {
                // A path from the database is data, not a constant this
                // binary controls. A corrupted or hand-edited row must not
                // turn retention into a delete-anything primitive.
                eprintln!(
                    "nightjar: retention: refusing to remove {} — outside the runs directory",
                    path.display()
                );
                continue;
            }
            match std::fs::remove_file(path) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => {
                    eprintln!("nightjar: retention: cannot remove {}: {e}", path.display());
                }
            }
        }
    }
}

/// True only if `path` lexically resolves to somewhere inside `runs_dir`.
/// Guards `unlink_orphaned` against a relative path or one that climbs
/// out via `..`, both reachable from a corrupted or hand-edited row.
///
/// Lexical, not a filesystem check: a symlink planted inside `runs_dir`
/// would still pass. Nothing on the write path lets a job create one there.
fn is_within_runs_dir(path: &Path, runs_dir: &Path) -> bool {
    path.is_absolute() && normalize_lexically(path).starts_with(normalize_lexically(runs_dir))
}

/// Resolves `.` and `..` without touching the filesystem. `Path::canonicalize`
/// requires the path to exist, but retention deletes files that may
/// already be gone. `PathBuf::pop` at the root is a no-op, so a `..` past
/// `/` clamps there, matching a real filesystem.
fn normalize_lexically(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            std::path::Component::ParentDir => {
                out.pop();
            }
            std::path::Component::CurDir => {}
            other => out.push(other.as_os_str()),
        }
    }
    out
}

/// Removes a job from the in-flight dispatch set on drop, unwind included.
/// See `dispatch_overdue_alert`'s spawn for why that matters.
struct InFlightGuard {
    in_flight: Arc<Mutex<HashSet<String>>>,
    job: String,
}

impl Drop for InFlightGuard {
    fn drop(&mut self) {
        // Poison-tolerant: a panic on another dispatch thread must not turn
        // this drop into a second panic, which would abort the daemon.
        self.in_flight
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(&self.job);
    }
}

impl Daemon {
    /// Only the daemon can know a job never ran at all, so this is the one
    /// alert `runner::exec` can't raise itself. Uses the same
    /// `overdue_since` as `status`, so "overdue" can't drift between them.
    ///
    /// Runs while `next_run_at` still holds what the previous tick armed:
    /// catch-up and `evaluate` below can both re-arm it before this tick
    /// returns. Even so, the occurrence `evaluate` is about to run reads
    /// as overdue for the instant before it runs — the grace margin in
    /// the loop covers that.
    fn check_overdue(&mut self, live: &[Job], now: Timestamp) {
        for job in live {
            let state = match self.store.job_state(&job.name) {
                Ok(s) => s,
                Err(e) => {
                    eprintln!(
                        "nightjar: job {:?}: cannot read state for overdue check: {e:#}",
                        job.name
                    );
                    continue;
                }
            };
            let last = match self.store.last_run(&job.name) {
                Ok(l) => l,
                Err(e) => {
                    eprintln!(
                        "nightjar: job {:?}: cannot read last run for overdue check: {e:#}",
                        job.name
                    );
                    continue;
                }
            };
            let Some(since) = overdue_since(state.as_ref(), last.as_ref(), now) else {
                // Resolved: it ran, or `next_run_at` moved on. Drop the
                // stale attempt stamp so a new overdue occurrence isn't
                // throttled by the old one's cooldown.
                self.overdue_last_attempt.remove(&job.name);
                continue;
            };

            // See this function's doc for why the margin exists.
            // `MAX_NORMAL_HEARTBEAT_GAP` is reused here because both
            // describe the same thing: how late this process's cadence
            // can run before it counts as a gap.
            let overdue_for = now.as_second() - since.as_second();
            if overdue_for < secs_i64(MAX_NORMAL_HEARTBEAT_GAP) {
                continue;
            }

            self.dispatch_overdue_alert(job, since, now);
        }
    }

    /// Sends one overdue alert off the tick thread and returns
    /// immediately. The daemon has no use for the outcome, and waiting on
    /// it is the exact tick-thread stall a notification channel must never
    /// cause.
    ///
    /// `attempted_recently` exists because a failed send never reaches the
    /// store's `last_overdue_alert_at`. Without it, a down channel would
    /// redispatch every tick instead of backing off.
    fn dispatch_overdue_alert(&mut self, job: &Job, since: Timestamp, now: Timestamp) {
        if !job.on_failure.has_channel() {
            return;
        }

        // A read failure fails open. A duplicate page costs less than a
        // silently missed outage.
        let notified_recently = match self.store.last_overdue_alert_at(&job.name) {
            Ok(last) => !cooldown_expired(last, now),
            Err(e) => {
                eprintln!(
                    "nightjar: job {:?}: cannot read overdue alert cooldown: {e:#}",
                    job.name
                );
                false
            }
        };
        if notified_recently {
            return;
        }

        let attempted_recently =
            !cooldown_expired(self.overdue_last_attempt.get(&job.name).copied(), now);
        if attempted_recently {
            return;
        }

        {
            let mut in_flight = self
                .overdue_dispatch_in_flight
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            if !in_flight.insert(job.name.clone()) {
                return;
            }
        }
        self.overdue_last_attempt.insert(job.name.clone(), now);

        let alert = Alert::Overdue {
            job: job.name.clone(),
            since,
        };
        let on_failure = job.on_failure.clone();
        let notifier = Arc::clone(&self.notifier);
        let db_path = self.paths.db_path.clone();
        let job_name = job.name.clone();
        let in_flight = Arc::clone(&self.overdue_dispatch_in_flight);

        // A fresh connection, not `self.store`. This thread can outlive
        // the tick that spawned it, and `Store` wraps a
        // `rusqlite::Connection`, which isn't `Sync`.
        std::thread::spawn(move || {
            // Guards the `remove` with `Drop`, not a line at the end of
            // this closure. A panic inside a third-party `notifier.send`
            // would otherwise skip it, leaving the job unable to alert
            // again for the daemon's whole life.
            let _guard = InFlightGuard {
                in_flight,
                job: job_name.clone(),
            };
            // Empty: an overdue job never started, so it has no secret of
            // its own for this alert to guard.
            let outcomes = notifier.send(&alert, &on_failure, &[]);
            for outcome in &outcomes {
                if let Err(e) = &outcome.result {
                    eprintln!(
                        "nightjar: {} overdue alert failed for job {:?}: {e:#}",
                        outcome.channel, job_name
                    );
                }
            }
            if outcomes.iter().any(|o| o.result.is_ok()) {
                match Store::open(&db_path) {
                    Ok(store) => {
                        let _ = store.set_last_overdue_alert_at(&job_name, now);
                    }
                    Err(e) => eprintln!(
                        "nightjar: job {job_name:?}: cannot record overdue alert cooldown: {e:#}"
                    ),
                }
            }
        });
    }
}

#[cfg(test)]
mod newly_seen_tests {
    use super::newly_seen;
    use std::collections::HashSet;

    #[test]
    fn a_warning_is_reported_once_until_it_goes_away_and_returns() {
        let mut seen = HashSet::new();
        let w = ["b: parent disabled".to_string()];

        assert_eq!(newly_seen(&mut seen, w.iter()), w);
        assert!(
            newly_seen(&mut seen, w.iter()).is_empty(),
            "same tick again"
        );
        assert!(newly_seen(&mut seen, [].iter()).is_empty(), "resolved");
        assert_eq!(newly_seen(&mut seen, w.iter()), w, "recurred");
    }

    #[test]
    fn only_the_new_entries_are_reported_when_the_set_grows() {
        let mut seen = HashSet::new();
        let first = ["one".to_string()];
        let both = ["one".to_string(), "two".to_string()];
        newly_seen(&mut seen, first.iter());
        assert_eq!(newly_seen(&mut seen, both.iter()), ["two".to_string()]);
    }
}

#[cfg(test)]
mod path_safety_tests {
    use super::{is_within_runs_dir, normalize_lexically};
    use std::path::Path;

    #[test]
    fn ordinary_child_path_is_within_the_runs_dir() {
        assert!(is_within_runs_dir(
            Path::new("/data/runs/backup/r1.out"),
            Path::new("/data/runs")
        ));
    }

    #[test]
    fn runs_dir_itself_counts_as_within() {
        assert!(is_within_runs_dir(
            Path::new("/data/runs"),
            Path::new("/data/runs")
        ));
    }

    #[test]
    fn relative_path_is_never_within_no_matter_its_contents() {
        assert!(!is_within_runs_dir(
            Path::new("runs/backup/r1.out"),
            Path::new("/data/runs")
        ));
    }

    #[test]
    fn path_is_rejected_when_it_climbs_out_via_dotdot() {
        assert!(!is_within_runs_dir(
            Path::new("/data/runs/backup/../../etc/passwd"),
            Path::new("/data/runs")
        ));
    }

    #[test]
    fn directory_is_rejected_when_it_is_a_sibling_that_merely_shares_a_prefix_string() {
        assert!(!is_within_runs_dir(
            Path::new("/data/runs-evil/x"),
            Path::new("/data/runs")
        ));
    }

    #[test]
    fn dotdot_past_the_root_clamps_at_root_rather_than_underflowing() {
        assert_eq!(
            normalize_lexically(Path::new("/../../../etc/passwd")),
            Path::new("/etc/passwd")
        );
    }

    #[test]
    fn current_dir_components_are_dropped() {
        assert_eq!(
            normalize_lexically(Path::new("/data/./runs/./j")),
            Path::new("/data/runs/j")
        );
    }
}
