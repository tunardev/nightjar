use nightjar_core::clock::{Clock, FixedClock};
use nightjar_core::paths::Paths;
use nightjar_daemon::Daemon;
use nightjar_store::Store;
use nightjar_store::run::{RunStatus, Trigger};
use std::sync::Arc;
use std::time::{Duration, Instant};

mod support;
use support::*;

#[test]
fn job_is_not_spawned_when_its_time_has_not_arrived() {
    let (_t, paths) = setup(&[(
        "nightly",
        "command = \"true\"\nschedule = \"daily at 2am\"\n",
    )]);
    let clock = Arc::new(FixedClock::new("2026-06-01T00:00:00Z".parse().unwrap()));
    let mut d = Daemon::new(paths, clock.clone()).unwrap();

    assert!(d.tick().unwrap().is_empty());
    clock.advance(jiff::Span::new().minutes(30));
    assert!(d.tick().unwrap().is_empty());
}

#[test]
fn job_is_spawned_when_its_time_arrives() {
    let (_t, paths) = setup(&[(
        "frequent",
        "command = \"true\"\nschedule = \"every 15 minutes\"\n",
    )]);
    let clock = Arc::new(FixedClock::new("2026-06-01T00:00:00Z".parse().unwrap()));
    let mut d = Daemon::new(paths, clock.clone()).unwrap();

    assert!(d.tick().unwrap().is_empty());
    clock.advance(jiff::Span::new().minutes(16));
    assert_eq!(d.tick().unwrap(), vec!["frequent".to_string()]);
}

#[test]
fn job_never_fires_when_it_is_disabled() {
    let (_t, paths) = setup(&[(
        "off",
        "command = \"true\"\nschedule = \"every 15 minutes\"\nenabled = false\n",
    )]);
    let clock = Arc::new(FixedClock::new("2026-06-01T00:00:00Z".parse().unwrap()));
    let mut d = Daemon::new(paths, clock.clone()).unwrap();
    d.tick().unwrap();
    clock.advance(jiff::Span::new().hours(2));
    assert!(d.tick().unwrap().is_empty());
}

#[test]
fn job_is_skipped_without_stopping_others_when_it_is_invalid() {
    let (_t, paths) = setup(&[
        (
            "good",
            "command = \"true\"\nschedule = \"every 15 minutes\"\n",
        ),
        ("broken", "command = = =\n"),
    ]);
    let clock = Arc::new(FixedClock::new("2026-06-01T00:00:00Z".parse().unwrap()));
    let mut d = Daemon::new(paths, clock.clone()).unwrap();
    d.tick().unwrap();
    clock.advance(jiff::Span::new().minutes(16));
    assert_eq!(d.tick().unwrap(), vec!["good".to_string()]);
}

#[test]
fn job_is_picked_up_when_added_after_startup() {
    let (_t, paths) = setup(&[]);
    let clock = Arc::new(FixedClock::new("2026-06-01T00:00:00Z".parse().unwrap()));
    let mut d = Daemon::new(paths.clone(), clock.clone()).unwrap();
    assert!(d.tick().unwrap().is_empty());

    std::fs::write(
        paths.jobs_dir.join("late.toml"),
        "command = \"true\"\nschedule = \"every 15 minutes\"\n",
    )
    .unwrap();

    d.tick().unwrap();
    clock.advance(jiff::Span::new().minutes(16));
    assert_eq!(d.tick().unwrap(), vec!["late".to_string()]);
}

#[test]
fn overlap_skip_does_not_start_second_run_while_one_is_in_flight() {
    let (_t, paths) = setup(&[(
        "slow",
        "command = \"true\"\nschedule = \"every 15 minutes\"\noverlap = \"skip\"\n",
    )]);
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

    let mut d = Daemon::new(paths, clock.clone()).unwrap();
    d.tick().unwrap();
    clock.advance(jiff::Span::new().minutes(16));
    assert!(
        d.tick().unwrap().is_empty(),
        "skip must not start a concurrent run"
    );
}

#[test]
fn overlap_queue_sets_occurrence_aside_instead_of_recording_it_missed() {
    let (_t, paths) = setup(&[(
        "queued",
        "command = \"true\"\nschedule = \"every 1 minute\"\noverlap = \"queue\"\n",
    )]);
    let clock = Arc::new(FixedClock::new("2026-06-01T00:00:00Z".parse().unwrap()));
    let store = Store::open(&paths.db_path).unwrap();

    store
        .start_run(
            "inflight",
            "queued",
            Trigger::Schedule,
            clock.now(),
            &paths.runs_dir.join("queued/inflight.out"),
            &paths.runs_dir.join("queued/inflight.err"),
        )
        .unwrap();
    store.set_run_pid("inflight", std::process::id()).unwrap();

    let mut d = Daemon::new(paths, clock.clone()).unwrap();
    d.tick().unwrap();
    clock.advance(jiff::Span::new().minutes(1));
    assert!(
        d.tick().unwrap().is_empty(),
        "queue must not start a concurrent run"
    );

    assert_eq!(
        store.queued_count("queued").unwrap(),
        1,
        "the due occurrence must be set aside, not lost"
    );
    let missed = store
        .recent_runs(Some("queued"), 1000)
        .unwrap()
        .into_iter()
        .filter(|r| r.status == RunStatus::Missed)
        .count();
    assert_eq!(
        missed, 0,
        "under queue_depth, overlap=queue must not fall back to missed"
    );
}

#[test]
fn overlap_queue_drains_oldest_entry_when_in_flight_run_finishes() {
    let (_t, paths) = setup(&[(
        "queued",
        "command = \"true\"\nschedule = \"every 1 minute\"\noverlap = \"queue\"\n",
    )]);
    let clock = Arc::new(FixedClock::new("2026-06-01T00:00:00Z".parse().unwrap()));
    let store = Store::open(&paths.db_path).unwrap();
    store
        .start_run(
            "inflight",
            "queued",
            Trigger::Schedule,
            clock.now(),
            &paths.runs_dir.join("queued/inflight.out"),
            &paths.runs_dir.join("queued/inflight.err"),
        )
        .unwrap();
    store.set_run_pid("inflight", std::process::id()).unwrap();

    let spawner = FakeSpawner::new(false, paths.db_path.clone(), clock.clone());
    let mut d = Daemon::with_spawner(paths.clone(), clock.clone(), spawner.clone()).unwrap();
    d.tick().unwrap();

    clock.advance(jiff::Span::new().minutes(1));
    d.tick().unwrap();
    assert_eq!(store.queued_count("queued").unwrap(), 1);
    assert_eq!(
        spawner.count_for("queued"),
        0,
        "must not spawn while the earlier run is still in flight"
    );

    store
        .finish_run("inflight", RunStatus::Success, Some(0), clock.now(), 0)
        .unwrap();

    d.tick().unwrap();
    assert_eq!(
        store.queued_count("queued").unwrap(),
        0,
        "the queued occurrence must have been dequeued"
    );
    assert_eq!(
        spawner.count_for("queued"),
        1,
        "the dequeued occurrence must have been spawned"
    );
}

#[test]
fn overlap_queue_falls_back_to_missed_when_queue_depth_is_reached() {
    let (_t, paths) = setup(&[(
        "queued",
        "command = \"true\"\nschedule = \"every 1 minute\"\noverlap = \"queue\"\n",
    )]);
    std::fs::write(paths.config_dir.join("config.toml"), "queue_depth = 2\n").unwrap();
    let clock = Arc::new(FixedClock::new("2026-06-01T00:00:00Z".parse().unwrap()));
    let store = Store::open(&paths.db_path).unwrap();
    store
        .start_run(
            "inflight",
            "queued",
            Trigger::Schedule,
            clock.now(),
            &paths.runs_dir.join("queued/inflight.out"),
            &paths.runs_dir.join("queued/inflight.err"),
        )
        .unwrap();
    store.set_run_pid("inflight", std::process::id()).unwrap();

    let mut d = Daemon::new(paths, clock.clone()).unwrap();
    d.tick().unwrap();

    for _ in 0..4 {
        clock.advance(jiff::Span::new().minutes(1));
        d.tick().unwrap();
    }

    assert_eq!(
        store.queued_count("queued").unwrap(),
        2,
        "the queue must not grow past queue_depth"
    );
    let missed = store
        .recent_runs(Some("queued"), 1000)
        .unwrap()
        .into_iter()
        .filter(|r| r.status == RunStatus::Missed)
        .count();
    assert_eq!(
        missed, 2,
        "occurrences beyond queue_depth must fall back to missed"
    );
}

#[test]
fn disabled_jobs_queue_is_not_drained() {
    let (_t, paths) = setup(&[(
        "queued",
        "command = \"true\"\nschedule = \"every 1 minute\"\noverlap = \"queue\"\n",
    )]);
    let clock = Arc::new(FixedClock::new("2026-06-01T00:00:00Z".parse().unwrap()));
    let store = Store::open(&paths.db_path).unwrap();
    store.enqueue_run("queued", clock.now()).unwrap();

    std::fs::write(
        paths.jobs_dir.join("queued.toml"),
        "command = \"true\"\nschedule = \"every 1 minute\"\noverlap = \"queue\"\nenabled = false\n",
    )
    .unwrap();

    let spawner = FakeSpawner::new(false, paths.db_path.clone(), clock.clone());
    let mut d = Daemon::with_spawner(paths, clock.clone(), spawner.clone()).unwrap();
    d.tick().unwrap();

    assert_eq!(
        store.queued_count("queued").unwrap(),
        1,
        "a disabled job's queue entry must be left alone, not silently dropped either"
    );
    assert_eq!(spawner.count_for("queued"), 0);
}

#[test]
fn overlap_parallel_starts_regardless_of_in_flight_run() {
    let (_t, paths) = setup(&[(
        "para",
        "command = \"true\"\nschedule = \"every 15 minutes\"\noverlap = \"parallel\"\n",
    )]);
    let clock = Arc::new(FixedClock::new("2026-06-01T00:00:00Z".parse().unwrap()));
    let store = Store::open(&paths.db_path).unwrap();
    store
        .start_run(
            "inflight",
            "para",
            Trigger::Schedule,
            clock.now(),
            &paths.runs_dir.join("para/inflight.out"),
            &paths.runs_dir.join("para/inflight.err"),
        )
        .unwrap();
    store.set_run_pid("inflight", std::process::id()).unwrap();

    let mut d = Daemon::new(paths, clock.clone()).unwrap();
    d.tick().unwrap();
    clock.advance(jiff::Span::new().minutes(16));
    assert_eq!(d.tick().unwrap(), vec!["para".to_string()]);
}

#[test]
fn finished_run_does_not_block_next_one() {
    let (_t, paths) = setup(&[(
        "done",
        "command = \"true\"\nschedule = \"every 15 minutes\"\noverlap = \"skip\"\n",
    )]);
    let clock = Arc::new(FixedClock::new("2026-06-01T00:00:00Z".parse().unwrap()));
    let store = Store::open(&paths.db_path).unwrap();
    store
        .start_run(
            "old",
            "done",
            Trigger::Schedule,
            clock.now(),
            &paths.runs_dir.join("done/old.out"),
            &paths.runs_dir.join("done/old.err"),
        )
        .unwrap();
    store
        .finish_run(
            "old",
            nightjar_store::run::RunStatus::Success,
            Some(0),
            clock.now(),
            0,
        )
        .unwrap();

    let mut d = Daemon::new(paths, clock.clone()).unwrap();
    d.tick().unwrap();
    clock.advance(jiff::Span::new().minutes(16));
    assert_eq!(d.tick().unwrap(), vec!["done".to_string()]);
}

#[test]
fn jobs_directory_surfaces_as_error_not_silently_empty_when_it_is_unreadable() {
    use std::os::unix::fs::PermissionsExt;

    let (_t, paths) = setup(&[(
        "good",
        "command = \"true\"\nschedule = \"every 15 minutes\"\n",
    )]);
    let clock = Arc::new(FixedClock::new("2026-06-01T00:00:00Z".parse().unwrap()));
    let mut d = Daemon::new(paths.clone(), clock.clone()).unwrap();
    assert!(d.tick().unwrap().is_empty());

    let original_mode = std::fs::metadata(&paths.jobs_dir)
        .unwrap()
        .permissions()
        .mode();
    std::fs::set_permissions(&paths.jobs_dir, std::fs::Permissions::from_mode(0o000)).unwrap();

    let result = d.tick();

    std::fs::set_permissions(
        &paths.jobs_dir,
        std::fs::Permissions::from_mode(original_mode),
    )
    .unwrap();

    let err = result.expect_err("an unreadable jobs directory must surface as an error");
    assert!(
        err.to_string().contains("jobs directory"),
        "error should name the jobs directory; got: {err}"
    );
}

#[test]
fn daemon_records_heartbeat_on_every_tick() {
    let (_t, paths) = setup(&[("j", "command = \"true\"\nschedule = \"hourly\"\n")]);
    let clock = Arc::new(FixedClock::new("2026-06-01T00:00:00Z".parse().unwrap()));
    let mut d = Daemon::new(paths.clone(), clock.clone()).unwrap();

    d.tick().unwrap();
    let store = nightjar_store::Store::open(&paths.db_path).unwrap();
    let first = store.daemon_heartbeat().unwrap().unwrap();
    assert_eq!(first.at, clock.now());

    clock.advance(jiff::Span::new().seconds(30));
    d.tick().unwrap();
    let second = store.daemon_heartbeat().unwrap().unwrap();
    assert_eq!(
        second.at,
        clock.now(),
        "heartbeat must advance with the clock"
    );
    assert!(second.at > first.at);
}

#[test]
fn daemon_persists_its_next_run_so_status_can_read_it_without_daemon() {
    let (_t, paths) = setup(&[(
        "nightly",
        "command = \"true\"\nschedule = \"daily at 2am\"\n",
    )]);
    let clock = Arc::new(FixedClock::new("2026-06-01T00:00:00Z".parse().unwrap()));
    let mut d = Daemon::with_spawner_and_tz(
        paths.clone(),
        clock.clone(),
        Arc::new(nightjar_daemon::ExecSpawner),
        jiff::tz::TimeZone::get("UTC").unwrap(),
    )
    .unwrap();
    d.tick().unwrap();

    let store = nightjar_store::Store::open(&paths.db_path).unwrap();
    let s = store.job_state("nightly").unwrap().unwrap();
    let expected: jiff::Timestamp = "2026-06-01T02:00:00Z".parse().unwrap();
    assert_eq!(s.next_run_at, Some(expected));
}

#[test]
fn second_daemon_is_refused_lock_when_paths_are_same() {
    let (_t, paths) = setup(&[]);
    let clock = Arc::new(FixedClock::new("2026-06-01T00:00:00Z".parse().unwrap()));

    let _first = Daemon::new(paths.clone(), clock.clone()).unwrap();
    let Err(err) = Daemon::new(paths, clock) else {
        panic!("a second daemon on the same paths must not start");
    };
    assert!(
        err.to_string().to_lowercase().contains("already running"),
        "message should name the condition; got: {err}"
    );
}

#[test]
fn spawn_does_not_consume_occurrence_when_it_fails() {
    let (_t, paths) = setup(&[(
        "retry",
        "command = \"true\"\nschedule = \"every 1 minute\"\n",
    )]);
    let clock = Arc::new(FixedClock::new("2026-06-01T00:00:00Z".parse().unwrap()));
    let spawner = FakeSpawner::new(true, paths.db_path.clone(), clock.clone());
    let mut d = Daemon::with_spawner(paths, clock.clone(), spawner.clone()).unwrap();

    assert!(d.tick().unwrap().is_empty());
    clock.advance(jiff::Span::new().seconds(60));

    assert!(
        d.tick().unwrap().is_empty(),
        "one job's spawn failure is logged, not surfaced as the tick's own error"
    );
    assert_eq!(spawner.attempts(), 1);

    assert!(d.tick().unwrap().is_empty());
    assert_eq!(
        spawner.attempts(),
        2,
        "the job must be retried on the following tick"
    );

    spawner.start_succeeding();
    assert_eq!(d.tick().unwrap(), vec!["retry".to_string()]);
    assert_eq!(spawner.attempts(), 3);
}

#[test]
fn job_does_not_force_daemon_into_backoff_when_it_is_permanently_broken() {
    let (_t, paths) = setup(&[
        (
            "broken",
            "command = \"true\"\nschedule = \"every 1 minute\"\n",
        ),
        (
            "healthy",
            "command = \"true\"\nschedule = \"every 1 minute\"\n",
        ),
    ]);
    let clock = Arc::new(FixedClock::new("2026-06-01T00:00:00Z".parse().unwrap()));
    let spawner = FakeSpawner::failing_only("broken", paths.db_path.clone(), clock.clone());
    let mut d = Daemon::with_spawner(paths, clock.clone(), spawner.clone()).unwrap();

    assert!(d.tick().unwrap().is_empty());
    clock.advance(jiff::Span::new().seconds(60));

    assert_eq!(d.tick().unwrap(), vec!["healthy".to_string()]);
    assert_eq!(spawner.count_for("broken"), 1);

    for attempt in 2..=6 {
        assert!(
            d.tick().unwrap().is_empty(),
            "tick {attempt} must stay Ok despite the permanently failing job"
        );
        assert_eq!(spawner.count_for("broken"), attempt);
    }
    assert_eq!(spawner.count_for("healthy"), 1);
}

#[test]
fn job_does_not_force_daemon_into_backoff_across_moving_gap_when_it_is_permanently_broken() {
    let (_t, paths) = setup(&[
        (
            "broken",
            "command = \"true\"\nschedule = \"every 1 minute\"\noverlap = \"parallel\"\n",
        ),
        (
            "healthy",
            "command = \"true\"\nschedule = \"every 1 minute\"\noverlap = \"parallel\"\n",
        ),
    ]);
    let clock = Arc::new(FixedClock::new("2026-06-01T00:00:00Z".parse().unwrap()));
    let spawner = FakeSpawner::failing_only("broken", paths.db_path.clone(), clock.clone());
    let mut d = Daemon::with_spawner(paths, clock.clone(), spawner.clone()).unwrap();

    d.tick().unwrap();

    for tick in 1..=10u32 {
        clock.advance(jiff::Span::new().seconds(60));
        assert!(d.tick().is_ok(), "tick {tick}");
    }
    assert_eq!(spawner.count_for("healthy"), 10);
}

#[test]
fn finished_child_is_reaped_rather_than_left_zombie() {
    let (_t, paths) = setup(&[(
        "reaped",
        "command = \"true\"\nschedule = \"every 15 minutes\"\n",
    )]);
    let clock = Arc::new(FixedClock::new("2026-06-01T00:00:00Z".parse().unwrap()));
    let spawner = FakeSpawner::new(false, paths.db_path.clone(), clock.clone());
    let mut d = Daemon::with_spawner(paths, clock.clone(), spawner.clone()).unwrap();

    assert!(d.tick().unwrap().is_empty());
    clock.advance(jiff::Span::new().minutes(16));
    assert_eq!(d.tick().unwrap(), vec!["reaped".to_string()]);

    let pid = spawner.pids()[0];

    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline && pid_exists(pid) {
        std::thread::sleep(Duration::from_millis(20));
        d.tick().unwrap();
    }

    assert!(!pid_exists(pid), "pid {pid} still in process table");
    assert_eq!(spawner.attempts(), 1, "the job must not have fired again");
}

#[test]
fn running_row_becomes_unknown_at_startup_when_its_process_is_gone() {
    let (_t, paths) = setup(&[("j", "command = \"true\"\nschedule = \"hourly\"\n")]);
    let store = nightjar_store::Store::open(&paths.db_path).unwrap();
    let t: jiff::Timestamp = "2026-06-01T00:00:00Z".parse().unwrap();
    let dead_pid = 0x7FFF_FFFF_u32;
    store
        .start_run(
            "stale",
            "j",
            Trigger::Schedule,
            t,
            &paths.runs_dir.join("j/stale.out"),
            &paths.runs_dir.join("j/stale.err"),
        )
        .unwrap();
    store.set_run_pid("stale", dead_pid).unwrap();
    drop(store);

    let clock = Arc::new(FixedClock::new("2026-06-01T01:00:00Z".parse().unwrap()));
    let _d = Daemon::new(paths.clone(), clock).unwrap();

    let store = nightjar_store::Store::open(&paths.db_path).unwrap();
    let r = store.get_run("stale").unwrap().unwrap();
    assert_eq!(r.status, RunStatus::Unknown);
    assert!(
        r.finished_at.is_some(),
        "an unknown run is finished, just not provably"
    );
}

#[test]
fn running_row_is_left_alone_when_its_process_is_live() {
    let (_t, paths) = setup(&[("j", "command = \"true\"\nschedule = \"hourly\"\n")]);
    let store = nightjar_store::Store::open(&paths.db_path).unwrap();
    let t: jiff::Timestamp = "2026-06-01T00:00:00Z".parse().unwrap();
    store
        .start_run(
            "live",
            "j",
            Trigger::Schedule,
            t,
            &paths.runs_dir.join("j/live.out"),
            &paths.runs_dir.join("j/live.err"),
        )
        .unwrap();
    store.set_run_pid("live", std::process::id()).unwrap();
    drop(store);

    let clock = Arc::new(FixedClock::new("2026-06-01T01:00:00Z".parse().unwrap()));
    let _d = Daemon::new(paths.clone(), clock).unwrap();

    let store = nightjar_store::Store::open(&paths.db_path).unwrap();
    assert_eq!(
        store.get_run("live").unwrap().unwrap().status,
        RunStatus::Running
    );
}

#[test]
fn running_row_becomes_unknown_when_it_has_no_pid() {
    let (_t, paths) = setup(&[("j", "command = \"true\"\nschedule = \"hourly\"\n")]);
    let store = nightjar_store::Store::open(&paths.db_path).unwrap();
    let t: jiff::Timestamp = "2026-06-01T00:00:00Z".parse().unwrap();
    store
        .start_run(
            "nopid",
            "j",
            Trigger::Schedule,
            t,
            &paths.runs_dir.join("j/nopid.out"),
            &paths.runs_dir.join("j/nopid.err"),
        )
        .unwrap();
    drop(store);

    let clock = Arc::new(FixedClock::new("2026-06-01T01:00:00Z".parse().unwrap()));
    let _d = Daemon::new(paths.clone(), clock).unwrap();

    let store = nightjar_store::Store::open(&paths.db_path).unwrap();
    assert_eq!(
        store.get_run("nopid").unwrap().unwrap().status,
        RunStatus::Unknown
    );
}

fn seed_finished_run(
    store: &Store,
    id: &str,
    job: &str,
    at: jiff::Timestamp,
    status: RunStatus,
    paths: &Paths,
) {
    store
        .start_run(
            id,
            job,
            Trigger::Schedule,
            at,
            &paths.runs_dir.join(format!("{job}/{id}.out")),
            &paths.runs_dir.join(format!("{job}/{id}.err")),
        )
        .unwrap();
    store.finish_run(id, status, Some(0), at, 0).unwrap();
}

fn missed_runs(store: &Store, job: &str) -> usize {
    store
        .recent_runs(Some(job), 1000)
        .unwrap()
        .into_iter()
        .filter(|r| r.status == RunStatus::Missed)
        .count()
}

#[test]
fn parent_run_fires_its_child_when_successful() {
    let (_t, paths) = setup(&[
        ("a", "command = \"true\"\nschedule = \"every 1 hour\"\n"),
        ("b", "command = \"true\"\nafter = [\"a\"]\n"),
    ]);
    let clock = Arc::new(FixedClock::new("2026-06-01T00:00:00Z".parse().unwrap()));
    let spawner = FakeSpawner::new(false, paths.db_path.clone(), clock.clone());
    let mut d = Daemon::with_spawner(paths.clone(), clock.clone(), spawner.clone()).unwrap();
    let store = Store::open(&paths.db_path).unwrap();

    clock.advance(jiff::Span::new().seconds(1));
    seed_finished_run(
        &store,
        "a-run",
        "a",
        clock.now(),
        RunStatus::Success,
        &paths,
    );

    d.tick().unwrap();

    assert_eq!(
        spawner.count_for("b"),
        1,
        "a succeeding parent must fire its child"
    );
}

/// The parent's exec is reaped, and its row pruned by age, in the same
/// tick that should fire the child. Retention must not win that race.
#[test]
fn child_still_fires_when_parent_row_ages_past_retention_before_the_next_tick() {
    // `catchup = "none"`: a make-up run of `a` after the gap would be a
    // second success for `b` to fire from, hiding the loss of the first.
    let (_t, paths) = setup(&[
        (
            "a",
            "command = \"true\"\nschedule = \"every 1 minute\"\ncatchup = \"none\"\n",
        ),
        ("b", "command = \"true\"\nafter = [\"a\"]\n"),
    ]);
    std::fs::write(
        paths.config_dir.join("config.toml"),
        "retention_age = \"5m\"\n",
    )
    .unwrap();
    let clock = Arc::new(FixedClock::new("2026-06-01T00:00:00Z".parse().unwrap()));
    let spawner = SucceedingSpawner::new(paths.db_path.clone(), clock.clone());
    let mut d = Daemon::with_spawner(paths.clone(), clock.clone(), spawner.clone()).unwrap();

    d.tick().unwrap();
    clock.advance(jiff::Span::new().seconds(60));
    assert_eq!(d.tick().unwrap(), vec!["a".to_string()]);
    assert_eq!(spawner.count_for("b"), 0, "b fires only once a is reaped");

    // Let `a`'s exec exit, so the next tick reaps it and prunes `a` on
    // the spot. Then the lid closes for two hours before that tick.
    std::thread::sleep(Duration::from_millis(200));
    clock.advance(jiff::Span::new().hours(2));
    for _ in 0..3 {
        d.tick().unwrap();
    }
    assert_eq!(
        spawner.count_for("b"),
        1,
        "retention pruned a's success row before the trigger could read it"
    );
}

#[test]
fn parent_run_does_not_fire_its_child_when_failed() {
    let (_t, paths) = setup(&[
        ("a", "command = \"false\"\nschedule = \"every 1 hour\"\n"),
        ("b", "command = \"true\"\nafter = [\"a\"]\n"),
    ]);
    let clock = Arc::new(FixedClock::new("2026-06-01T00:00:00Z".parse().unwrap()));
    let spawner = FakeSpawner::new(false, paths.db_path.clone(), clock.clone());
    let mut d = Daemon::with_spawner(paths.clone(), clock.clone(), spawner.clone()).unwrap();
    let store = Store::open(&paths.db_path).unwrap();

    clock.advance(jiff::Span::new().seconds(1));
    seed_finished_run(
        &store,
        "a-run",
        "a",
        clock.now(),
        RunStatus::Failure,
        &paths,
    );

    d.tick().unwrap();

    assert_eq!(
        spawner.count_for("b"),
        0,
        "\"after a\" means after a *succeeds*; a non-zero exit is not success"
    );
}

#[test]
fn parent_does_not_fire_its_child_when_timed_out() {
    let (_t, paths) = setup(&[
        ("a", "command = \"sleep 99\"\nschedule = \"every 1 hour\"\n"),
        ("b", "command = \"true\"\nafter = [\"a\"]\n"),
    ]);
    let clock = Arc::new(FixedClock::new("2026-06-01T00:00:00Z".parse().unwrap()));
    let spawner = FakeSpawner::new(false, paths.db_path.clone(), clock.clone());
    let mut d = Daemon::with_spawner(paths.clone(), clock.clone(), spawner.clone()).unwrap();
    let store = Store::open(&paths.db_path).unwrap();

    clock.advance(jiff::Span::new().seconds(1));
    seed_finished_run(
        &store,
        "a-run",
        "a",
        clock.now(),
        RunStatus::Timeout,
        &paths,
    );

    d.tick().unwrap();

    assert_eq!(spawner.count_for("b"), 0, "a timeout is not a success");
}

#[test]
fn parent_run_still_fires_its_child_when_started_by_hand() {
    let (_t, paths) = setup(&[
        ("a", "command = \"true\"\nschedule = \"every 1 hour\"\n"),
        ("b", "command = \"true\"\nafter = [\"a\"]\n"),
    ]);
    let clock = Arc::new(FixedClock::new("2026-06-01T00:00:00Z".parse().unwrap()));
    let spawner = FakeSpawner::new(false, paths.db_path.clone(), clock.clone());
    let mut d = Daemon::with_spawner(paths.clone(), clock.clone(), spawner.clone()).unwrap();
    let store = Store::open(&paths.db_path).unwrap();

    clock.advance(jiff::Span::new().seconds(1));
    store
        .start_run(
            "a-run",
            "a",
            Trigger::Manual,
            clock.now(),
            &paths.runs_dir.join("a/a-run.out"),
            &paths.runs_dir.join("a/a-run.err"),
        )
        .unwrap();
    store
        .finish_run("a-run", RunStatus::Success, Some(0), clock.now(), 0)
        .unwrap();

    d.tick().unwrap();

    assert_eq!(spawner.count_for("b"), 1);
}

#[test]
fn child_run_records_its_trigger_as_parent() {
    let (_t, paths) = setup(&[
        ("a", "command = \"true\"\nschedule = \"every 1 hour\"\n"),
        ("b", "command = \"true\"\nafter = [\"a\"]\n"),
    ]);
    let clock = Arc::new(FixedClock::new("2026-06-01T00:00:00Z".parse().unwrap()));
    let spawner = FakeSpawner::new(false, paths.db_path.clone(), clock.clone());
    let mut d = Daemon::with_spawner(paths.clone(), clock.clone(), spawner).unwrap();
    let store = Store::open(&paths.db_path).unwrap();

    clock.advance(jiff::Span::new().seconds(1));
    seed_finished_run(
        &store,
        "a-run",
        "a",
        clock.now(),
        RunStatus::Success,
        &paths,
    );

    d.tick().unwrap();

    let b_run = store.last_run("b").unwrap().expect("b should have run");
    assert_eq!(
        b_run.trigger,
        Trigger::After("a".to_string()),
        "\"why did this run\" is the question this product exists to answer"
    );
}

#[test]
fn chain_of_three_runs_all_three_in_order() {
    let (_t, paths) = setup(&[
        ("a", "command = \"true\"\nschedule = \"every 1 hour\"\n"),
        ("b", "command = \"true\"\nafter = [\"a\"]\n"),
        ("c", "command = \"true\"\nafter = [\"b\"]\n"),
    ]);
    let clock = Arc::new(FixedClock::new("2026-06-01T00:00:00Z".parse().unwrap()));
    let spawner = FakeSpawner::new(false, paths.db_path.clone(), clock.clone());
    let mut d = Daemon::with_spawner(paths.clone(), clock.clone(), spawner.clone()).unwrap();
    let store = Store::open(&paths.db_path).unwrap();

    clock.advance(jiff::Span::new().seconds(1));
    seed_finished_run(
        &store,
        "a-run",
        "a",
        clock.now(),
        RunStatus::Success,
        &paths,
    );

    d.tick().unwrap();
    assert_eq!(spawner.count_for("b"), 1, "a's success must fire b");

    let b_run = store.last_run("b").unwrap().unwrap();
    clock.advance(jiff::Span::new().seconds(1));
    store
        .finish_run(&b_run.id, RunStatus::Success, Some(0), clock.now(), 0)
        .unwrap();

    d.tick().unwrap();
    assert_eq!(spawner.count_for("c"), 1, "b's success must fire c");
}

#[test]
fn child_still_honours_its_overlap_policy() {
    let (_t, paths) = setup(&[
        ("a", "command = \"true\"\nschedule = \"every 1 hour\"\n"),
        (
            "b",
            "command = \"true\"\nafter = [\"a\"]\noverlap = \"skip\"\n",
        ),
    ]);
    let clock = Arc::new(FixedClock::new("2026-06-01T00:00:00Z".parse().unwrap()));
    let store = Store::open(&paths.db_path).unwrap();

    store
        .start_run(
            "b-inflight",
            "b",
            Trigger::Manual,
            clock.now(),
            &paths.runs_dir.join("b/b-inflight.out"),
            &paths.runs_dir.join("b/b-inflight.err"),
        )
        .unwrap();
    store.set_run_pid("b-inflight", std::process::id()).unwrap();

    let spawner = FakeSpawner::new(false, paths.db_path.clone(), clock.clone());
    let mut d = Daemon::with_spawner(paths.clone(), clock.clone(), spawner.clone()).unwrap();

    clock.advance(jiff::Span::new().seconds(1));
    seed_finished_run(
        &store,
        "a-run",
        "a",
        clock.now(),
        RunStatus::Success,
        &paths,
    );

    d.tick().unwrap();

    assert_eq!(
        spawner.count_for("b"),
        0,
        "being triggered is not a licence to run concurrently with itself"
    );
    assert_eq!(
        missed_runs(&store, "b"),
        1,
        "the refused trigger must still be recorded, not silently dropped"
    );
}

#[test]
fn child_does_not_fire_when_disabled() {
    let (_t, paths) = setup(&[
        ("a", "command = \"true\"\nschedule = \"every 1 hour\"\n"),
        (
            "b",
            "command = \"true\"\nafter = [\"a\"]\nenabled = false\n",
        ),
    ]);
    let clock = Arc::new(FixedClock::new("2026-06-01T00:00:00Z".parse().unwrap()));
    let spawner = FakeSpawner::new(false, paths.db_path.clone(), clock.clone());
    let mut d = Daemon::with_spawner(paths.clone(), clock.clone(), spawner.clone()).unwrap();
    let store = Store::open(&paths.db_path).unwrap();

    clock.advance(jiff::Span::new().seconds(1));
    seed_finished_run(
        &store,
        "a-run",
        "a",
        clock.now(),
        RunStatus::Success,
        &paths,
    );

    d.tick().unwrap();

    assert_eq!(
        spawner.count_for("b"),
        0,
        "`enabled = false` is an explicit stop, not a pause"
    );
}

#[test]
fn trigger_lost_to_daemon_crash_is_recorded_missed_not_run() {
    let (_t, paths) = setup(&[
        ("a", "command = \"true\"\nschedule = \"every 1 hour\"\n"),
        ("b", "command = \"true\"\nafter = [\"a\"]\n"),
    ]);
    let clock = Arc::new(FixedClock::new("2026-06-01T00:00:00Z".parse().unwrap()));
    let store = Store::open(&paths.db_path).unwrap();

    seed_finished_run(
        &store,
        "a-run",
        "a",
        clock.now(),
        RunStatus::Success,
        &paths,
    );

    clock.advance(jiff::Span::new().seconds(60));
    let spawner = FakeSpawner::new(false, paths.db_path.clone(), clock.clone());
    let mut d = Daemon::with_spawner(paths.clone(), clock.clone(), spawner.clone()).unwrap();

    d.tick().unwrap();

    assert_eq!(
        spawner.count_for("b"),
        0,
        "a lost trigger must never be retro-fired at a time nobody chose"
    );
    assert_eq!(
        missed_runs(&store, "b"),
        1,
        "losing it silently would violate the product's one promise"
    );
}

#[test]
fn trigger_already_honoured_is_not_recorded_missed_on_restart() {
    let (_t, paths) = setup(&[
        ("a", "command = \"true\"\nschedule = \"every 1 hour\"\n"),
        ("b", "command = \"true\"\nafter = [\"a\"]\n"),
    ]);
    let clock = Arc::new(FixedClock::new("2026-06-01T00:00:00Z".parse().unwrap()));
    let store = Store::open(&paths.db_path).unwrap();

    {
        let spawner = FakeSpawner::new(false, paths.db_path.clone(), clock.clone());
        let mut d = Daemon::with_spawner(paths.clone(), clock.clone(), spawner.clone()).unwrap();
        clock.advance(jiff::Span::new().seconds(1));
        seed_finished_run(
            &store,
            "a-run",
            "a",
            clock.now(),
            RunStatus::Success,
            &paths,
        );
        d.tick().unwrap();
        assert_eq!(spawner.count_for("b"), 1, "the first daemon must fire b");
    }

    clock.advance(jiff::Span::new().seconds(300));
    let spawner2 = FakeSpawner::new(false, paths.db_path.clone(), clock.clone());
    let mut d2 = Daemon::with_spawner(paths.clone(), clock.clone(), spawner2.clone()).unwrap();
    d2.tick().unwrap();

    assert_eq!(missed_runs(&store, "b"), 0);
    assert_eq!(
        spawner2.count_for("b"),
        0,
        "and it must not fire a second time either"
    );
}

#[test]
fn parent_that_never_succeeded_produces_no_missed_child_row() {
    let (_t, paths) = setup(&[
        ("a", "command = \"false\"\nschedule = \"every 1 hour\"\n"),
        ("b", "command = \"true\"\nafter = [\"a\"]\n"),
    ]);
    let clock = Arc::new(FixedClock::new("2026-06-01T00:00:00Z".parse().unwrap()));
    let store = Store::open(&paths.db_path).unwrap();

    seed_finished_run(
        &store,
        "a-run",
        "a",
        clock.now(),
        RunStatus::Failure,
        &paths,
    );

    clock.advance(jiff::Span::new().seconds(60));
    let spawner = FakeSpawner::new(false, paths.db_path.clone(), clock.clone());
    let mut d = Daemon::with_spawner(paths.clone(), clock.clone(), spawner.clone()).unwrap();

    d.tick().unwrap();

    assert_eq!(spawner.count_for("b"), 0);
    assert_eq!(
        missed_runs(&store, "b"),
        0,
        "a child is only owed a run once its parent actually succeeds"
    );
}

#[test]
fn one_parent_success_fires_its_child_exactly_once_across_many_ticks() {
    let (_t, paths) = setup(&[
        ("a", "command = \"true\"\nschedule = \"every 1 hour\"\n"),
        (
            "b",
            "command = \"true\"\nafter = [\"a\"]\noverlap = \"parallel\"\n",
        ),
    ]);
    let clock = Arc::new(FixedClock::new("2026-06-01T00:00:00Z".parse().unwrap()));
    let spawner = FakeSpawner::new(false, paths.db_path.clone(), clock.clone());
    let mut d = Daemon::with_spawner(paths.clone(), clock.clone(), spawner.clone()).unwrap();
    let store = Store::open(&paths.db_path).unwrap();

    clock.advance(jiff::Span::new().seconds(1));
    seed_finished_run(
        &store,
        "a-run",
        "a",
        clock.now(),
        RunStatus::Success,
        &paths,
    );

    for _ in 0..5 {
        clock.advance(jiff::Span::new().seconds(1));
        d.tick().unwrap();
    }

    assert_eq!(
        spawner.count_for("b"),
        1,
        "one success owes exactly one run, no matter how many ticks follow"
    );
}

#[test]
fn parent_that_succeeds_twice_fires_its_child_twice() {
    let (_t, paths) = setup(&[
        ("a", "command = \"true\"\nschedule = \"every 1 hour\"\n"),
        (
            "b",
            "command = \"true\"\nafter = [\"a\"]\noverlap = \"parallel\"\n",
        ),
    ]);
    let clock = Arc::new(FixedClock::new("2026-06-01T00:00:00Z".parse().unwrap()));
    let spawner = FakeSpawner::new(false, paths.db_path.clone(), clock.clone());
    let mut d = Daemon::with_spawner(paths.clone(), clock.clone(), spawner.clone()).unwrap();
    let store = Store::open(&paths.db_path).unwrap();

    for (i, id) in ["a-run-1", "a-run-2"].iter().enumerate() {
        clock.advance(jiff::Span::new().seconds(1));
        seed_finished_run(&store, id, "a", clock.now(), RunStatus::Success, &paths);
        d.tick().unwrap();
        assert_eq!(spawner.count_for("b"), i + 1);
    }
}
