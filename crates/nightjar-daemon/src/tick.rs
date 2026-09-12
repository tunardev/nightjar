use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};

use anyhow::{Context, Result};
use jiff::Timestamp;
use nightjar_config::job::{JobsDirState, probe_jobs_dir};
use nightjar_config::{Job, Overlap};
use nightjar_core::format::quantity;
use nightjar_core::limits::MAX_SLEEP;
use nightjar_runner::exec::cooldown_expired;
use nightjar_runner::notify::Alert;
use nightjar_store::run::Trigger;
use nightjar_store::{Store, overdue_since};

use crate::daemon::Daemon;
use crate::reconcile::MAX_NORMAL_HEARTBEAT_GAP;
use crate::{secs_i64, sleep_unless_stopping, stop_requested};

const ERROR_BACKOFF: std::time::Duration = std::time::Duration::from_secs(1);

const MAX_BACKOFF_DOUBLINGS: u32 = 5;

impl Daemon {
    /// # Errors
    /// fails if the heartbeat, the watermark, the jobs directory, or a catch-up fails
    pub fn tick(&mut self) -> Result<Vec<String>> {
        self.reap();
        let now = self.clock.now();

        let accounted = self.pin_accounted_through()?;
        let gap_since = self.gap_since(accounted, now);

        self.store
            .write_heartbeat(now, std::process::id(), env!("CARGO_PKG_VERSION"))?;

        if let Err(e) = self.reconcile() {
            nightjar_core::log!("reconcile failed: {e:#}");
        }

        let dir_state = probe_jobs_dir(&self.paths.jobs_dir)?;
        let loaded = match dir_state {
            JobsDirState::Missing => Vec::new(),
            JobsDirState::Present => Job::load_all(&self.paths.jobs_dir),
        };

        let jobs_with_a_file: Vec<String> = loaded.iter().map(|(name, _)| name.clone()).collect();

        let live: Vec<Job> = loaded
            .into_iter()
            .filter_map(|(_, r)| r.ok())
            .filter(|j| j.enabled)
            .collect();

        for warning in newly_seen(
            &mut self.logged_warnings,
            live.iter().flat_map(|j| j.warnings.iter()),
        ) {
            nightjar_core::log!("{warning}");
        }

        let live_names: HashSet<&str> = live.iter().map(|j| j.name.as_str()).collect();
        self.job_memory
            .retain(|job, _| live_names.contains(job.as_str()));

        self.check_overdue(&live, now);

        self.drain_queues(&live);

        self.fire_after_triggers(&live);

        if self.sweep_due(now) {
            self.sweep(dir_state, &jobs_with_a_file);
            self.last_sweep = Some(now);
        }

        let mut fired = Vec::new();
        let mut failure: Option<anyhow::Error> = None;
        let mut systemic_failure = false;

        let gap_is_consumed =
            gap_since.is_none_or(|since| match self.catch_up_gap(&live, since, now) {
                Ok(names) => {
                    fired.extend(names);
                    true
                }
                Err(e) => {
                    nightjar_core::log!("catch-up failed: {e:#}");
                    systemic_failure = true;
                    failure.get_or_insert(e);
                    false
                }
            });

        let recorded_through = if gap_since.is_some() {
            now
        } else {
            accounted.unwrap_or(now)
        };

        let mut refusals = Vec::new();
        for job in &live {
            match self.settle_due_occurrences(job, now, recorded_through) {
                Ok(settled) => {
                    if gap_is_consumed {
                        self.memory_for(&job.name).accounted_through = Some(now);
                    }
                    if settled == Settled::StartedARun {
                        fired.push(job.name.clone());
                    }
                }
                Err(e) => {
                    refusals.push(format!("job {:?}: {e:#}", job.name));
                    failure.get_or_insert(e);
                }
            }
        }
        for refusal in newly_seen(&mut self.logged_refusals, refusals.iter()) {
            nightjar_core::log!("{refusal}");
        }

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

    fn fire_after_triggers(&mut self, live: &[Job]) {
        let parents = match self.store.unfired_successful_runs() {
            Ok(p) => p,
            Err(e) => {
                nightjar_core::log!("after: cannot read unfired successful runs: {e:#}");
                return;
            }
        };

        for parent in parents {
            let Some(finished_at) = parent.finished_at else {
                continue;
            };
            let lost_to_a_restart = finished_at < self.started_at;

            for child in live
                .iter()
                .filter(|c| c.after.as_deref() == Some(&*parent.job))
            {
                if lost_to_a_restart {
                    nightjar_core::log!(
                        "job {:?}: its trigger from {:?} was lost to a daemon \
                         restart; recording it missed rather than running it now",
                        child.name,
                        parent.job
                    );
                    if let Err(e) =
                        self.record_missed(child, finished_at, Trigger::After(parent.job.clone()))
                    {
                        nightjar_core::log!(
                            "job {:?}: cannot record the lost trigger missed: {e:#}",
                            child.name
                        );
                    }
                    continue;
                }

                let in_flight = match self.in_flight_count(&child.name) {
                    Ok(n) => n,
                    Err(e) => {
                        nightjar_core::log!(
                            "job {:?}: cannot check in-flight count: {e:#}",
                            child.name
                        );
                        continue;
                    }
                };
                if !overlap_allows(child.overlap, in_flight) {
                    nightjar_core::log!(
                        "job {:?}: triggered by {:?} but skipped, \
                         {} already in flight",
                        child.name,
                        parent.job,
                        quantity(in_flight, "run", "runs")
                    );
                    if let Err(e) =
                        self.record_missed(child, finished_at, Trigger::After(parent.job.clone()))
                    {
                        nightjar_core::log!(
                            "job {:?}: cannot record the skipped trigger: {e:#}",
                            child.name
                        );
                    }
                    continue;
                }

                let run_id = uuid::Uuid::now_v7().to_string();
                if let Err(e) = self.spawn_as(child, run_id, Trigger::After(parent.job.clone())) {
                    nightjar_core::log!("job {:?}: cannot spawn: {e:#}", child.name);
                }
            }

            if let Err(e) = self.store.set_after_fired_at(&parent.id, self.clock.now()) {
                nightjar_core::log!(
                    "job {:?}: cannot mark run {} handled: {e:#}",
                    parent.job,
                    parent.id
                );
            }
        }
    }

    fn drain_queues(&mut self, live: &[Job]) {
        for job in live {
            let queued = match self.store.queued_count(&job.name) {
                Ok(n) => n,
                Err(e) => {
                    nightjar_core::log!("queue: job {:?}: cannot check queue: {e:#}", job.name);
                    continue;
                }
            };
            if queued == 0 {
                continue;
            }
            let in_flight = match self.in_flight_count(&job.name) {
                Ok(n) => n,
                Err(e) => {
                    nightjar_core::log!(
                        "queue: job {:?}: cannot check in-flight count: {e:#}",
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
                        nightjar_core::log!(
                            "queue: job {:?}: cannot spawn dequeued run: {e:#}",
                            job.name
                        );
                    }
                }
                Ok(None) => {}
                Err(e) => nightjar_core::log!("queue: job {:?}: cannot dequeue: {e:#}", job.name),
            }
        }
    }

    fn settle_due_occurrences(
        &mut self,
        job: &Job,
        now: Timestamp,
        recorded_through: Timestamp,
    ) -> Result<Settled> {
        let Some(due_at) = self.armed_for(&job.name) else {
            self.rearm(job, now)?;
            return Ok(Settled::StartedNothing);
        };

        if due_at > now {
            return Ok(Settled::StartedNothing);
        }

        let in_flight = self.in_flight_count(&job.name)?;
        let allowed = overlap_allows(job.overlap, in_flight);

        if !allowed {
            nightjar_core::log!(
                "job {:?}: skipped, {} already in flight",
                job.name,
                quantity(in_flight, "run", "runs")
            );
            self.rearm(job, now)?;
            if due_at > recorded_through {
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
            return Ok(Settled::StartedNothing);
        }

        self.spawn(job, Trigger::Schedule)?;
        self.rearm(job, now)?;

        self.retire(job, recorded_through.max(due_at), now)?;
        self.prune_job_now(&job.name);
        Ok(Settled::StartedARun)
    }

    fn retire(&self, job: &Job, after: Timestamp, now: Timestamp) -> Result<usize> {
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
            nightjar_core::log!(
                "job {:?}: recorded {} missed",
                job.name,
                quantity(retired, "occurrence", "occurrences")
            );
        }
        Ok(retired)
    }

    pub(crate) fn rearm(&mut self, job: &Job, now: Timestamp) -> Result<()> {
        let next = match &job.schedule {
            Some(s) => s.next_after(now, &self.tz)?,
            None => None,
        };
        self.memory_for(&job.name).armed_for = next;
        self.store.set_next_run(&job.name, next)?;
        Ok(())
    }

    fn sleep_for(&self) -> std::time::Duration {
        let ceiling = self.config.heartbeat_interval.min(MAX_SLEEP);
        let soonest = self.job_memory.values().filter_map(|m| m.armed_for).min();
        sleep_until(soonest, self.clock.now(), ceiling)
    }

    /// # Errors
    /// never fails: a bad tick is logged and retried until a stop is requested
    pub fn run(&mut self) -> Result<()> {
        loop {
            if stop_requested() {
                nightjar_core::log!("daemon stopping");
                return Ok(());
            }
            match self.tick() {
                Ok(fired) => {
                    self.consecutive_failures = 0;
                    for name in fired {
                        nightjar_core::log!("started {name}");
                    }
                    sleep_unless_stopping(self.sleep_for());
                }
                Err(e) => {
                    self.consecutive_failures = self.consecutive_failures.saturating_add(1);
                    nightjar_core::log!("tick failed: {e:#}");
                    let doublings = self.consecutive_failures.min(MAX_BACKOFF_DOUBLINGS);
                    let backoff = (ERROR_BACKOFF * (1u32 << doublings)).min(MAX_SLEEP);
                    sleep_unless_stopping(backoff);
                }
            }
        }
    }
}

const MIN_SLEEP: std::time::Duration = std::time::Duration::from_millis(250);

fn sleep_until(
    soonest: Option<Timestamp>,
    now: Timestamp,
    ceiling: std::time::Duration,
) -> std::time::Duration {
    let Some(t) = soonest else { return ceiling };
    let ms = u64::try_from((t.as_millisecond() - now.as_millisecond()).max(0)).unwrap_or(0);
    std::time::Duration::from_millis(ms).clamp(MIN_SLEEP.min(ceiling), ceiling)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Settled {
    StartedARun,
    StartedNothing,
}

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

#[must_use]
pub const fn overlap_allows(overlap: Overlap, in_flight: usize) -> bool {
    match overlap {
        Overlap::Parallel => true,
        Overlap::Skip | Overlap::Queue => in_flight == 0,
    }
}

const RETENTION_MISSED: usize = 50;

const RETENTION_SWEEP: std::time::Duration = std::time::Duration::from_secs(3600);

const RETENTION_STARTUP_DEFER: std::time::Duration = std::time::Duration::from_secs(300);

impl Daemon {
    pub(crate) fn sweep_due(&self, now: Timestamp) -> bool {
        self.last_sweep.map_or_else(
            || now.as_second() - self.started_at.as_second() >= secs_i64(RETENTION_STARTUP_DEFER),
            |last| now.as_second() - last.as_second() >= secs_i64(RETENTION_SWEEP),
        )
    }

    pub(crate) fn sweep(&self, dir_state: JobsDirState, jobs_with_a_file: &[String]) {
        match self.store.distinct_run_jobs() {
            Ok(jobs) => {
                for job in jobs {
                    self.prune_job_now(&job);
                }
            }
            Err(e) => nightjar_core::log!("retention: cannot list jobs: {e:#}"),
        }

        if matches!(dir_state, JobsDirState::Present) {
            self.prune_orphaned_job_state(jobs_with_a_file);
        }
    }

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
            Err(e) => nightjar_core::log!("retention: job {job:?}: prune failed: {e:#}"),
        }
        if let Some(cutoff) = cutoff {
            match self.store.prune_older_than(job, cutoff) {
                Ok(orphaned) => self.unlink_orphaned(&orphaned),
                Err(e) => {
                    nightjar_core::log!("retention: job {job:?}: age prune failed: {e:#}");
                }
            }
        }
    }

    fn prune_orphaned_job_state(&self, jobs_with_a_file: &[String]) {
        let states = match self.store.all_job_states() {
            Ok(s) => s,
            Err(e) => {
                nightjar_core::log!("retention: cannot list job_state: {e:#}");
                return;
            }
        };
        for state in states {
            if jobs_with_a_file.iter().any(|name| name == &state.job) {
                continue;
            }
            if let Err(e) = self.store.delete_job_state(&state.job) {
                nightjar_core::log!(
                    "retention: job {:?}: cannot delete job_state: {e:#}",
                    state.job
                );
            }
        }
    }

    fn unlink_orphaned(&self, paths: &[PathBuf]) {
        for path in paths {
            if !is_within_runs_dir(path, &self.paths.runs_dir) {
                nightjar_core::log!(
                    "retention: refusing to remove {}; outside the runs directory",
                    path.display()
                );
                continue;
            }
            match std::fs::remove_file(path) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => {
                    nightjar_core::log!("retention: cannot remove {}: {e}", path.display());
                }
            }
        }
    }
}

fn is_within_runs_dir(path: &Path, runs_dir: &Path) -> bool {
    path.is_absolute() && normalize_lexically(path).starts_with(normalize_lexically(runs_dir))
}

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

struct InFlightGuard {
    in_flight: Arc<Mutex<HashSet<String>>>,
    job: String,
}

impl Drop for InFlightGuard {
    fn drop(&mut self) {
        self.in_flight
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(&self.job);
    }
}

const OVERDUE_GRACE: std::time::Duration = MAX_NORMAL_HEARTBEAT_GAP;

impl Daemon {
    fn check_overdue(&mut self, live: &[Job], now: Timestamp) {
        for job in live {
            let state = match self.store.job_state(&job.name) {
                Ok(s) => s,
                Err(e) => {
                    nightjar_core::log!(
                        "job {:?}: cannot read state for overdue check: {e:#}",
                        job.name
                    );
                    continue;
                }
            };
            let last = match self.store.last_run(&job.name) {
                Ok(l) => l,
                Err(e) => {
                    nightjar_core::log!(
                        "job {:?}: cannot read last run for overdue check: {e:#}",
                        job.name
                    );
                    continue;
                }
            };
            let Some(since) = overdue_since(state.as_ref(), last.as_ref(), now) else {
                self.memory_for(&job.name).overdue_alert_attempted_at = None;
                continue;
            };

            let overdue_for = now.as_second() - since.as_second();
            if overdue_for < secs_i64(OVERDUE_GRACE) {
                continue;
            }

            self.dispatch_overdue_alert(job, since, now);
        }
    }

    fn dispatch_overdue_alert(&mut self, job: &Job, since: Timestamp, now: Timestamp) {
        if !job.on_failure.has_channel() {
            return;
        }

        let notified_recently = match self.store.last_overdue_alert_at(&job.name) {
            Ok(last) => !cooldown_expired(last, now),
            Err(e) => {
                nightjar_core::log!(
                    "job {:?}: cannot read overdue alert cooldown: {e:#}",
                    job.name
                );
                false
            }
        };
        if notified_recently {
            return;
        }

        let last_attempt = self
            .job_memory
            .get(&job.name)
            .and_then(|m| m.overdue_alert_attempted_at);
        if !cooldown_expired(last_attempt, now) {
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
        self.memory_for(&job.name).overdue_alert_attempted_at = Some(now);

        let alert = Alert::Overdue {
            job: job.name.clone(),
            since,
        };
        let on_failure = job.on_failure.clone();
        let notifier = Arc::clone(&self.notifier);
        let db_path = self.paths.db_path.clone();
        let job_name = job.name.clone();
        let in_flight = Arc::clone(&self.overdue_dispatch_in_flight);

        std::thread::spawn(move || {
            let _guard = InFlightGuard {
                in_flight,
                job: job_name.clone(),
            };
            let outcomes = notifier.send(&alert, &on_failure, &[]);
            for outcome in &outcomes {
                if let Err(e) = &outcome.result {
                    nightjar_core::log!(
                        "{} overdue alert failed for job {:?}: {e:#}",
                        outcome.channel,
                        job_name
                    );
                }
            }
            if outcomes.iter().any(|o| o.result.is_ok()) {
                match Store::open(&db_path) {
                    Ok(store) => {
                        let _ = store.set_last_overdue_alert_at(&job_name, now);
                    }
                    Err(e) => nightjar_core::log!(
                        "job {job_name:?}: cannot record overdue alert cooldown: {e:#}"
                    ),
                }
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;
    use std::path::Path;

    use jiff::Timestamp;

    use super::{MIN_SLEEP, is_within_runs_dir, newly_seen, normalize_lexically, sleep_until};

    fn ts(s: &str) -> Timestamp {
        s.parse().unwrap()
    }

    #[test]
    fn the_loop_always_yields_even_when_a_firing_is_already_overdue() {
        let now = ts("2026-06-01T12:00:00Z");
        let ceiling = std::time::Duration::from_secs(30);
        let overdue = ts("2026-06-01T11:00:00Z");

        assert_eq!(
            sleep_until(Some(overdue), now, ceiling),
            MIN_SLEEP,
            "a timestamp left in the past must never spin the daemon at full speed"
        );
        assert_eq!(sleep_until(Some(now), now, ceiling), MIN_SLEEP);
    }

    #[test]
    fn a_firing_less_than_a_second_away_is_waited_for_rather_than_truncated_to_zero() {
        let now = ts("2026-06-01T12:00:00.100Z");
        let ceiling = std::time::Duration::from_secs(30);

        assert_eq!(
            sleep_until(Some(ts("2026-06-01T12:00:00.900Z")), now, ceiling),
            std::time::Duration::from_millis(800)
        );
    }

    #[test]
    fn the_heartbeat_ceiling_caps_both_a_distant_firing_and_no_firing_at_all() {
        let now = ts("2026-06-01T12:00:00Z");
        let ceiling = std::time::Duration::from_secs(30);

        assert_eq!(sleep_until(None, now, ceiling), ceiling);
        assert_eq!(
            sleep_until(Some(ts("2026-06-02T12:00:00Z")), now, ceiling),
            ceiling
        );
    }

    #[test]
    fn a_ceiling_below_the_floor_still_wins_so_the_heartbeat_is_never_overshot() {
        let now = ts("2026-06-01T12:00:00Z");
        let ceiling = std::time::Duration::from_millis(50);

        assert_eq!(sleep_until(Some(now), now, ceiling), ceiling);
        assert_eq!(sleep_until(None, now, ceiling), ceiling);
    }

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
    fn a_failure_that_never_clears_is_reported_once_not_once_per_tick() {
        let mut seen = HashSet::new();
        let refusal = [r#"job "broken": spawning exec for job "broken": no such file"#.to_string()];

        assert_eq!(newly_seen(&mut seen, refusal.iter()), refusal);
        for tick in 2..=1000 {
            assert!(
                newly_seen(&mut seen, refusal.iter()).is_empty(),
                "tick {tick} repeated a line the log already carries"
            );
        }
    }

    #[test]
    fn a_failure_that_changes_is_reported_again_rather_than_swallowed() {
        let mut seen = HashSet::new();
        let first = [r#"job "broken": permission denied"#.to_string()];
        let second = [r#"job "broken": no such file"#.to_string()];

        assert_eq!(newly_seen(&mut seen, first.iter()), first);
        assert_eq!(newly_seen(&mut seen, second.iter()), second);
        assert_eq!(newly_seen(&mut seen, first.iter()), first, "and back again");
    }

    #[test]
    fn only_the_new_entries_are_reported_when_the_set_grows() {
        let mut seen = HashSet::new();
        let first = ["one".to_string()];
        let both = ["one".to_string(), "two".to_string()];
        newly_seen(&mut seen, first.iter());
        assert_eq!(newly_seen(&mut seen, both.iter()), ["two".to_string()]);
    }

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
