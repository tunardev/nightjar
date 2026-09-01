use nightjar_core::clock::{Clock, FixedClock};
use nightjar_daemon::Daemon;
use nightjar_runner::notify::{Alert, Notifier, NotifyOutcome, RecordingNotifier};
use nightjar_store::Store;
use nightjar_store::run::{RunStatus, Trigger};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

mod support;
use support::*;

#[test]
fn overdue_occurrence_after_resolved_one_still_respects_durable_cooldown() {
    let (_t, paths) = setup(&[(
        "flaky",
        "command = \"true\"\nschedule = \"every 5 minutes\"\n[on_failure]\nnotify = true\n",
    )]);
    let clock = Arc::new(FixedClock::new("2026-06-01T00:00:00Z".parse().unwrap()));
    let n = Arc::new(RecordingNotifier::default());
    let mut d = Daemon::with_notifier(paths.clone(), clock.clone(), n.clone()).unwrap();

    d.tick().unwrap();

    clock.advance(jiff::Span::new().minutes(10));
    d.tick().unwrap();
    assert_eq!(
        n.wait_for_calls(1, Duration::from_secs(2)).len(),
        1,
        "the first overdue occurrence must alert"
    );

    clock.advance(jiff::Span::new().minutes(6));
    let job = nightjar_config::Job::load(&paths.jobs_dir.join("flaky.toml")).unwrap();
    let store = Store::open(&paths.db_path).unwrap();
    nightjar_runner::execute(
        &job,
        "flaky-ran-once",
        Trigger::Manual,
        &paths,
        &store,
        clock.as_ref(),
        nightjar_runner::DEFAULT_OUTPUT_CAP,
        n.as_ref(),
        None,
    )
    .unwrap();
    drop(store);

    let _ = d.tick();

    clock.advance(jiff::Span::new().minutes(10));
    let _ = d.tick();
    assert_eq!(n.calls().len(), 1);
}

#[test]
fn down_notification_channel_retries_at_cooldown_cadence_not_tick_cadence() {
    let (_t, paths) = setup(&[(
        "stuck",
        "command = \"true\"\nschedule = \"every 1 minute\"\n[on_failure]\nnotify = true\n",
    )]);
    let clock = Arc::new(FixedClock::new("2026-06-01T00:00:00Z".parse().unwrap()));
    let n = Arc::new(RecordingNotifier::failing_on("desktop"));
    let spawner = FakeSpawner::new(true, paths.db_path.clone(), clock.clone());
    let mut d =
        Daemon::with_spawner_and_notifier(paths, clock.clone(), spawner, n.clone()).unwrap();

    d.tick().unwrap();

    for _ in 0..4 {
        clock.advance(jiff::Span::new().seconds(30));
        let _ = d.tick();
    }
    assert_eq!(
        n.wait_for_calls(1, Duration::from_secs(2)).len(),
        1,
        "the first sustained overdue occurrence must still attempt, even though its only channel is down"
    );

    let ticks = 30;
    for _ in 0..ticks {
        clock.advance(jiff::Span::new().seconds(30));
        let _ = d.tick();
    }
    let alerted_for: Vec<jiff::Timestamp> = n
        .calls()
        .into_iter()
        .filter_map(|c| match c.alert {
            Alert::Overdue { since, .. } => Some(since),
            _ => None,
        })
        .collect();
    let distinct: std::collections::HashSet<jiff::Timestamp> =
        alerted_for.iter().copied().collect();
    assert_eq!(
        distinct.len(),
        alerted_for.len(),
        "one occurrence must never be alerted twice: {alerted_for:?}"
    );
    assert!(
        alerted_for.len() < ticks / 2,
        "{} attempts over {ticks} ticks",
        alerted_for.len()
    );
}

#[test]
fn job_never_dispatches_when_no_alert_channel_is_configured() {
    let (_t, paths) = setup(&[(
        "stuck",
        "command = \"true\"\nschedule = \"every 1 minute\"\n",
    )]);
    let clock = Arc::new(FixedClock::new("2026-06-01T00:00:00Z".parse().unwrap()));
    let n = Arc::new(RecordingNotifier::default());
    let spawner = FakeSpawner::new(true, paths.db_path.clone(), clock.clone());
    let mut d =
        Daemon::with_spawner_and_notifier(paths, clock.clone(), spawner, n.clone()).unwrap();

    d.tick().unwrap();
    for _ in 0..10 {
        clock.advance(jiff::Span::new().seconds(30));
        let _ = d.tick();
    }

    assert_eq!(n.send_count(), 0);
}

#[test]
fn job_never_alerts_overdue_when_healthy_and_running_on_schedule() {
    let (_t, paths) = setup(&[(
        "healthy",
        "command = \"true\"\nschedule = \"every 1 minute\"\n[on_failure]\nnotify = true\n",
    )]);
    let clock = Arc::new(FixedClock::new("2026-06-01T00:00:00Z".parse().unwrap()));
    let n = Arc::new(RecordingNotifier::default());
    let spawner = FakeSpawner::new(false, paths.db_path.clone(), clock.clone());
    let mut d =
        Daemon::with_spawner_and_notifier(paths.clone(), clock.clone(), spawner.clone(), n.clone())
            .unwrap();

    for _ in 0..240 {
        clock.advance(jiff::Span::new().seconds(30));
        d.tick().unwrap();
    }

    let store = Store::open(&paths.db_path).unwrap();
    let next_run_at = store
        .job_state("healthy")
        .unwrap()
        .and_then(|s| s.next_run_at)
        .expect("a job ticked this many times must have an armed next_run_at");
    assert!(spawner.count() >= 20, "fired {} times", spawner.count());
    assert!(
        next_run_at > clock.now() - jiff::Span::new().minutes(2),
        "next_run_at={next_run_at}"
    );
    assert!(n.calls().is_empty(), "got {:?}", n.calls());
}

struct CountingSlowNotifier {
    delay: Duration,
    state: Mutex<(usize, usize, usize)>,
    changed: Condvar,
}

impl CountingSlowNotifier {
    fn new(delay: Duration) -> Arc<Self> {
        Arc::new(Self {
            delay,
            state: Mutex::new((0, 0, 0)),
            changed: Condvar::new(),
        })
    }

    fn wait_for_calls(&self, n: usize, timeout: Duration) -> usize {
        let guard = self.state.lock().unwrap();
        let (guard, _) = self
            .changed
            .wait_timeout_while(guard, timeout, |s| s.0 < n)
            .unwrap();
        guard.0
    }

    fn peak_concurrent(&self) -> usize {
        self.state.lock().unwrap().2
    }
}

impl Notifier for CountingSlowNotifier {
    fn send(
        &self,
        _alert: &Alert,
        _on_failure: &nightjar_config::OnFailure,
        _redact: &[nightjar_config::secrets::SecretValue],
    ) -> Vec<NotifyOutcome> {
        {
            let mut s = self.state.lock().unwrap();
            s.1 += 1;
            s.2 = s.2.max(s.1);
        }
        std::thread::sleep(self.delay);
        {
            let mut s = self.state.lock().unwrap();
            s.1 -= 1;
            s.0 += 1;
        }
        self.changed.notify_all();
        vec![NotifyOutcome {
            channel: "desktop",
            result: Ok(()),
        }]
    }
}

#[test]
fn tick_stays_prompt_regardless_of_how_many_jobs_are_overdue() {
    let names: Vec<String> = (0..10).map(|i| format!("stuck{i}")).collect();
    let body = "command = \"true\"\nschedule = \"every 1 minute\"\n[on_failure]\nnotify = true\n";
    let jobs: Vec<(&str, &str)> = names.iter().map(|n| (n.as_str(), body)).collect();
    let (_t, paths) = setup(&jobs);

    let clock = Arc::new(FixedClock::new("2026-06-01T00:00:00Z".parse().unwrap()));
    let notifier = CountingSlowNotifier::new(Duration::from_secs(3));
    let spawner = FakeSpawner::new(true, paths.db_path.clone(), clock.clone());
    let mut d =
        Daemon::with_spawner_and_notifier(paths, clock.clone(), spawner, notifier.clone()).unwrap();

    d.tick().unwrap();
    clock.advance(jiff::Span::new().minutes(2));

    let started = Instant::now();
    let _ = d.tick();
    let elapsed = started.elapsed();

    assert!(
        elapsed < Duration::from_secs(1),
        "tick must not accumulate per-overdue-job latency; ten jobs took {elapsed:?}"
    );
    assert_eq!(
        notifier.wait_for_calls(10, Duration::from_secs(5)),
        10,
        "every one of the ten overdue jobs must actually have dispatched"
    );
    assert_eq!(
        notifier.peak_concurrent(),
        10,
        "ten independent jobs dispatch concurrently — the in-flight guard is per job, not global"
    );
}

#[test]
fn job_sends_at_most_once_per_cooldown_across_forty_ticks_when_persistently_overdue() {
    let (_t, paths) = setup(&[(
        "stuck",
        "command = \"true\"\nschedule = \"every 1 minute\"\n[on_failure]\nnotify = true\n",
    )]);
    let clock = Arc::new(FixedClock::new("2026-06-01T00:00:00Z".parse().unwrap()));
    let notifier = CountingSlowNotifier::new(Duration::from_secs(3));
    let spawner = FakeSpawner::new(true, paths.db_path.clone(), clock.clone());
    let mut d =
        Daemon::with_spawner_and_notifier(paths, clock.clone(), spawner, notifier.clone()).unwrap();

    d.tick().unwrap();

    for _ in 0..40 {
        clock.advance(jiff::Span::new().seconds(30));
        let _ = d.tick();
    }

    assert_eq!(
        notifier.wait_for_calls(1, Duration::from_secs(5)),
        1,
        "one persistently overdue job, ticked forty times inside its cooldown, must send at most once"
    );
}

struct GatedNotifier {
    state: Mutex<(usize, usize, usize)>,
    state_changed: Condvar,
    released: Mutex<bool>,
    released_changed: Condvar,
}

impl GatedNotifier {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            state: Mutex::new((0, 0, 0)),
            state_changed: Condvar::new(),
            released: Mutex::new(false),
            released_changed: Condvar::new(),
        })
    }

    fn wait_for_calls(&self, n: usize, timeout: Duration) -> usize {
        let guard = self.state.lock().unwrap();
        let (guard, _) = self
            .state_changed
            .wait_timeout_while(guard, timeout, |s| s.0 < n)
            .unwrap();
        guard.0
    }

    fn peak_concurrent(&self) -> usize {
        self.state.lock().unwrap().2
    }

    fn release(&self) {
        *self.released.lock().unwrap() = true;
        self.released_changed.notify_all();
    }
}

impl Notifier for GatedNotifier {
    fn send(
        &self,
        _alert: &Alert,
        _on_failure: &nightjar_config::OnFailure,
        _redact: &[nightjar_config::secrets::SecretValue],
    ) -> Vec<NotifyOutcome> {
        {
            let mut s = self.state.lock().unwrap();
            s.0 += 1;
            s.1 += 1;
            s.2 = s.2.max(s.1);
        }
        self.state_changed.notify_all();

        let guard = self.released.lock().unwrap();
        let _guard = self.released_changed.wait_while(guard, |r| !*r).unwrap();

        self.state.lock().unwrap().1 -= 1;
        vec![NotifyOutcome {
            channel: "desktop",
            result: Ok(()),
        }]
    }
}

#[test]
fn overdue_dispatch_stays_bounded_to_one_thread_per_job_even_once_attempt_cooldown_expires() {
    let (_t, paths) = setup(&[(
        "stuck",
        "command = \"true\"\nschedule = \"every 1 minute\"\n[on_failure]\nnotify = true\n",
    )]);
    let clock = Arc::new(FixedClock::new("2026-06-01T00:00:00Z".parse().unwrap()));
    let notifier = GatedNotifier::new();
    let spawner = FakeSpawner::new(true, paths.db_path.clone(), clock.clone());
    let mut d =
        Daemon::with_spawner_and_notifier(paths, clock.clone(), spawner, notifier.clone()).unwrap();

    d.tick().unwrap();
    clock.advance(jiff::Span::new().minutes(2));
    let _ = d.tick();

    assert_eq!(
        notifier.wait_for_calls(1, Duration::from_secs(2)),
        1,
        "the first overdue occurrence must dispatch"
    );

    clock.advance(jiff::Span::new().hours(2));
    let _ = d.tick();
    let _ = d.tick();

    assert_eq!(notifier.wait_for_calls(2, Duration::from_millis(500)), 1);
    assert_eq!(notifier.peak_concurrent(), 1);

    notifier.release();
    std::thread::sleep(Duration::from_millis(100));
}

struct PanickingNotifier {
    calls: Mutex<usize>,
    calls_changed: Condvar,
}

impl PanickingNotifier {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            calls: Mutex::new(0),
            calls_changed: Condvar::new(),
        })
    }

    fn wait_for_calls(&self, n: usize, timeout: Duration) -> usize {
        let guard = self.calls.lock().unwrap();
        let (guard, _) = self
            .calls_changed
            .wait_timeout_while(guard, timeout, |c| *c < n)
            .unwrap();
        *guard
    }
}

impl Notifier for PanickingNotifier {
    fn send(
        &self,
        _alert: &Alert,
        _on_failure: &nightjar_config::OnFailure,
        _redact: &[nightjar_config::secrets::SecretValue],
    ) -> Vec<NotifyOutcome> {
        {
            let mut calls = self.calls.lock().unwrap();
            *calls += 1;
        }
        self.calls_changed.notify_all();
        panic!("PanickingNotifier deliberately panics");
    }
}

fn wait_for_guard_to_clear() {
    std::thread::sleep(Duration::from_millis(300));
}

#[test]
fn panicking_notifier_does_not_permanently_disable_jobs_overdue_alerts() {
    let (_t, paths) = setup(&[(
        "stuck",
        "command = \"true\"\nschedule = \"every 1 minute\"\n[on_failure]\nnotify = true\n",
    )]);
    let clock = Arc::new(FixedClock::new("2026-06-01T00:00:00Z".parse().unwrap()));
    let notifier = PanickingNotifier::new();
    let spawner = FakeSpawner::new(true, paths.db_path.clone(), clock.clone());
    let mut d =
        Daemon::with_spawner_and_notifier(paths, clock.clone(), spawner, notifier.clone()).unwrap();

    d.tick().unwrap();
    for _ in 0..4 {
        clock.advance(jiff::Span::new().seconds(30));
        let _ = d.tick();
    }
    assert_eq!(
        notifier.wait_for_calls(1, Duration::from_secs(2)),
        1,
        "the first sustained overdue occurrence must attempt, even against a notifier that panics"
    );

    for cooldowns_elapsed in 1..=6 {
        wait_for_guard_to_clear();
        clock.advance(jiff::Span::new().hours(1));
        let _ = d.tick();
        assert_eq!(
            notifier.wait_for_calls(1 + cooldowns_elapsed, Duration::from_secs(10)),
            1 + cooldowns_elapsed,
            "cooldown #{cooldowns_elapsed} after the panic must still dispatch"
        );
    }
}

#[test]
fn flapping_job_does_not_alert_once_per_flap() {
    let (_t, paths) = setup(&[(
        "flapper",
        "command = \"true\"\nschedule = \"every 1 minute\"\n[on_failure]\nnotify = true\n",
    )]);
    let clock = Arc::new(FixedClock::new("2026-06-01T00:00:00Z".parse().unwrap()));
    let n = Arc::new(RecordingNotifier::default());
    let mut d = Daemon::with_notifier(paths.clone(), clock.clone(), n.clone()).unwrap();

    d.tick().unwrap();

    let job = nightjar_config::Job::load(&paths.jobs_dir.join("flapper.toml")).unwrap();
    for cycle in 0..30 {
        clock.advance(jiff::Span::new().minutes(2));
        let _ = d.tick();

        let store = Store::open(&paths.db_path).unwrap();
        nightjar_runner::execute(
            &job,
            &format!("flapper-run-{cycle}"),
            Trigger::Manual,
            &paths,
            &store,
            clock.as_ref(),
            nightjar_runner::DEFAULT_OUTPUT_CAP,
            n.as_ref(),
            None,
        )
        .unwrap();
        drop(store);
    }

    let overdue_alerts = n
        .calls()
        .iter()
        .filter(|c| matches!(c.alert, Alert::Overdue { .. }))
        .count();
    assert!(
        overdue_alerts <= 2,
        "{overdue_alerts} overdue alerts, calls: {:?}",
        n.calls()
    );
}

#[test]
fn skipped_window_is_not_recorded_twice_when_another_job_holds_watermark() {
    let (_t, paths) = setup(&[
        ("broken", "command = \"true\"\nschedule = \"* * * * * *\"\n"),
        (
            "slow",
            "command = \"true\"\nschedule = \"* * * * * *\"\noverlap = \"skip\"\n",
        ),
    ]);
    let clock = Arc::new(FixedClock::new("2026-06-01T00:00:00Z".parse().unwrap()));
    let store = Store::open(&paths.db_path).unwrap();

    store
        .start_run(
            "inflight",
            "slow",
            Trigger::Schedule,
            clock.now(),
            &paths.runs_dir.join("slow/inflight.out"),
            &paths.runs_dir.join("slow/inflight.err"),
        )
        .unwrap();
    store.set_run_pid("inflight", std::process::id()).unwrap();
    drop(store);

    let spawner = FakeSpawner::failing_only("broken", paths.db_path.clone(), clock.clone());
    let mut d = Daemon::with_spawner(paths.clone(), clock.clone(), spawner).unwrap();

    d.tick().unwrap();
    for _ in 0..3 {
        clock.advance(jiff::Span::new().seconds(2));
        let _ = d.tick();
    }

    let store = Store::open(&paths.db_path).unwrap();
    let mut occurrences: Vec<i64> = store
        .recent_runs(Some("slow"), 1000)
        .unwrap()
        .into_iter()
        .filter(|r| r.status == RunStatus::Missed)
        .map(|r| r.started_at.as_second())
        .collect();
    occurrences.sort_unstable();

    assert!(
        occurrences.len() >= 4,
        "too few skipped occurrences to prove anything: {occurrences:?}"
    );
    let mut distinct = occurrences.clone();
    distinct.dedup();
    assert_eq!(distinct, occurrences);
}
