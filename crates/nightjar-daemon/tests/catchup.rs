use nightjar_core::clock::{Clock, FixedClock};
use nightjar_daemon::{Daemon, Spawner};
use nightjar_store::Store;
use nightjar_store::run::{RunStatus, Trigger};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

mod support;
use support::*;

#[test]
fn catchup_none_records_every_missed_occurrence_and_runs_nothing() {
    let (_t, paths) = setup(&[(
        "j",
        "command = \"true\"\nschedule = \"every 15 minutes\"\ncatchup = \"none\"\n",
    )]);
    let store = nightjar_store::Store::open(&paths.db_path).unwrap();
    let then: jiff::Timestamp = "2026-06-01T00:00:00Z".parse().unwrap();
    store.write_heartbeat(then, 1, "0.1.0").unwrap();
    drop(store);

    let clock = Arc::new(FixedClock::new("2026-06-01T02:00:00Z".parse().unwrap()));
    let (mut d, spawns) = daemon_with_counting_spawner(paths.clone(), clock);
    d.tick().unwrap();

    assert_eq!(spawns.count(), 0, "catchup=none must not run anything");
    assert_eq!(
        missed_rows(&paths, "j"),
        8,
        "every missed occurrence must be recorded"
    );
}

#[test]
fn catchup_once_runs_exactly_one_make_up_and_records_rest_missed() {
    let (_t, paths) = setup(&[(
        "j",
        "command = \"true\"\nschedule = \"every 15 minutes\"\ncatchup = \"once\"\n",
    )]);
    let store = nightjar_store::Store::open(&paths.db_path).unwrap();
    store
        .write_heartbeat("2026-06-01T00:00:00Z".parse().unwrap(), 1, "0.1.0")
        .unwrap();
    drop(store);

    let clock = Arc::new(FixedClock::new("2026-06-01T02:00:00Z".parse().unwrap()));
    let (mut d, spawns) = daemon_with_counting_spawner(paths.clone(), clock);
    d.tick().unwrap();

    assert_eq!(spawns.count(), 1, "nobody wants eight backups");
    assert_eq!(missed_rows(&paths, "j"), 7);
}

#[test]
fn catchup_all_is_capped_and_overflow_is_recorded_missed() {
    let (_t, paths) = setup(&[(
        "j",
        "command = \"true\"\nschedule = \"every 1 minute\"\ncatchup = \"all\"\noverlap = \"parallel\"\n",
    )]);
    let store = nightjar_store::Store::open(&paths.db_path).unwrap();
    store
        .write_heartbeat("2026-06-01T00:00:00Z".parse().unwrap(), 1, "0.1.0")
        .unwrap();
    drop(store);

    let clock = Arc::new(FixedClock::new("2026-06-01T01:00:00Z".parse().unwrap()));
    let (mut d, spawns) = daemon_with_counting_spawner(paths.clone(), clock);
    d.tick().unwrap();

    assert_eq!(spawns.count(), 10, "capped at CATCHUP_MAX");
    assert_eq!(
        missed_rows(&paths, "j"),
        50,
        "the overflow is still recorded"
    );
}

#[test]
fn first_ever_start_catches_nothing_up_when_no_heartbeat_exists() {
    let (_t, paths) = setup(&[(
        "j",
        "command = \"true\"\nschedule = \"every 15 minutes\"\ncatchup = \"all\"\n",
    )]);
    let clock = Arc::new(FixedClock::new("2026-06-01T02:00:00Z".parse().unwrap()));
    let (mut d, spawns) = daemon_with_counting_spawner(paths.clone(), clock);
    d.tick().unwrap();

    assert_eq!(
        spawns.count(),
        0,
        "no heartbeat means no known gap, not an infinite one"
    );
    assert_eq!(missed_rows(&paths, "j"), 0);
}

#[test]
fn missed_rows_carry_occurrence_time_not_daemon_start_time() {
    let (_t, paths) = setup(&[(
        "j",
        "command = \"true\"\nschedule = \"every 15 minutes\"\ncatchup = \"none\"\n",
    )]);
    let store = nightjar_store::Store::open(&paths.db_path).unwrap();
    let heartbeat_at: jiff::Timestamp = "2026-06-01T00:00:00Z".parse().unwrap();
    store.write_heartbeat(heartbeat_at, 1, "0.1.0").unwrap();
    drop(store);

    let now: jiff::Timestamp = "2026-06-01T00:45:00Z".parse().unwrap();
    let clock = Arc::new(FixedClock::new(now));
    let (mut d, _spawns) = daemon_with_counting_spawner(paths.clone(), clock);
    d.tick().unwrap();

    let store = nightjar_store::Store::open(&paths.db_path).unwrap();
    let mut missed: Vec<_> = store
        .recent_runs(Some("j"), 100)
        .unwrap()
        .into_iter()
        .filter(|r| r.status == RunStatus::Missed)
        .collect();
    missed.sort_by_key(|r| r.started_at);

    let expected: Vec<jiff::Timestamp> = [
        "2026-06-01T00:15:00Z",
        "2026-06-01T00:30:00Z",
        "2026-06-01T00:45:00Z",
    ]
    .iter()
    .map(|s| s.parse().unwrap())
    .collect();

    let started: Vec<_> = missed.iter().map(|r| r.started_at).collect();
    assert_eq!(
        started, expected,
        "each missed row must carry its own occurrence time, not `now`"
    );

    let finished: Vec<_> = missed.iter().map(|r| r.finished_at.unwrap()).collect();
    assert_eq!(
        finished, expected,
        "finished_at must match the occurrence too, not the daemon's start time"
    );

    assert!(
        missed.iter().all(|r| r.trigger == Trigger::Catchup),
        "a missed row's trigger must say catchup"
    );
}

#[test]
fn reconcile_runs_before_catch_up_so_stale_row_does_not_suppress_make_up_run() {
    let (_t, paths) = setup(&[(
        "j",
        "command = \"true\"\nschedule = \"every 15 minutes\"\ncatchup = \"once\"\noverlap = \"skip\"\n",
    )]);
    let store = nightjar_store::Store::open(&paths.db_path).unwrap();
    let heartbeat_at: jiff::Timestamp = "2026-06-01T00:00:00Z".parse().unwrap();
    store.write_heartbeat(heartbeat_at, 1, "0.1.0").unwrap();

    let dead_pid = 0x7FFF_FFFF_u32;
    store
        .start_run(
            "crashed",
            "j",
            Trigger::Schedule,
            heartbeat_at,
            &paths.runs_dir.join("j/crashed.out"),
            &paths.runs_dir.join("j/crashed.err"),
        )
        .unwrap();
    store.set_run_pid("crashed", dead_pid).unwrap();
    drop(store);

    let clock = Arc::new(FixedClock::new("2026-06-01T00:30:00Z".parse().unwrap()));
    let (mut d, spawns) = daemon_with_counting_spawner(paths.clone(), clock);
    d.tick().unwrap();

    assert_eq!(spawns.count(), 1);
    assert_eq!(missed_rows(&paths, "j"), 1);
}

#[test]
fn long_gap_is_bounded_by_catchup_cap_not_by_its_own_length() {
    let (_t, paths) = setup(&[(
        "j",
        "command = \"true\"\nschedule = \"every 1 minute\"\ncatchup = \"all\"\noverlap = \"parallel\"\n",
    )]);
    let store = nightjar_store::Store::open(&paths.db_path).unwrap();
    let heartbeat_at: jiff::Timestamp = "2026-06-01T00:00:00Z".parse().unwrap();
    store.write_heartbeat(heartbeat_at, 1, "0.1.0").unwrap();
    drop(store);

    let now = heartbeat_at + jiff::Span::new().minutes(5000);
    let clock = Arc::new(FixedClock::new(now));

    let started = Instant::now();
    let (mut d, spawns) = daemon_with_counting_spawner(paths.clone(), clock);
    d.tick().unwrap();
    let elapsed = started.elapsed();

    let store = nightjar_store::Store::open(&paths.db_path).unwrap();
    let missed = store
        .recent_runs(Some("j"), 10_000)
        .unwrap()
        .into_iter()
        .filter(|r| r.status == RunStatus::Missed)
        .count();

    assert_eq!(spawns.count(), 10, "still capped at CATCHUP_MAX");
    assert_eq!(missed, 4990);
    assert!(elapsed < Duration::from_secs(30), "took {elapsed:?}");
}

#[test]
fn skip_overlap_caps_catch_up_at_one_run_in_flight_even_under_catchup_all() {
    let (_t, paths) = setup(&[(
        "j",
        "command = \"true\"\nschedule = \"every 1 minute\"\ncatchup = \"all\"\n",
    )]);
    let store = nightjar_store::Store::open(&paths.db_path).unwrap();
    store
        .write_heartbeat("2026-06-01T00:00:00Z".parse().unwrap(), 1, "0.1.0")
        .unwrap();
    drop(store);

    let clock = Arc::new(FixedClock::new("2026-06-01T00:30:00Z".parse().unwrap()));
    let (mut d, spawns) = daemon_with_counting_spawner(paths.clone(), clock);
    d.tick().unwrap();

    let store = nightjar_store::Store::open(&paths.db_path).unwrap();
    let running = store.running_count("j").unwrap();
    assert!(running <= 1, "{running} running rows");
    assert_eq!(
        spawns.count(),
        1,
        "skip caps the whole catch-up batch at one make-up run, not CATCHUP_MAX"
    );
}

#[test]
fn catch_up_runs_most_recent_occurrence_not_oldest() {
    let (_t, paths) = setup(&[(
        "j",
        "command = \"true\"\nschedule = \"every 15 minutes\"\ncatchup = \"once\"\n",
    )]);
    let store = nightjar_store::Store::open(&paths.db_path).unwrap();
    let heartbeat_at: jiff::Timestamp = "2026-06-01T00:00:00Z".parse().unwrap();
    store.write_heartbeat(heartbeat_at, 1, "0.1.0").unwrap();
    drop(store);

    let now: jiff::Timestamp = "2026-06-01T02:00:00Z".parse().unwrap();
    let clock = Arc::new(FixedClock::new(now));
    let (mut d, spawns) = daemon_with_counting_spawner(paths.clone(), clock);
    d.tick().unwrap();

    assert_eq!(
        spawns.count(),
        1,
        "catchup = \"once\" still runs exactly one"
    );

    let store = nightjar_store::Store::open(&paths.db_path).unwrap();
    let mut missed: Vec<jiff::Timestamp> = store
        .recent_runs(Some("j"), 100)
        .unwrap()
        .into_iter()
        .filter(|r| r.status == RunStatus::Missed)
        .map(|r| r.started_at)
        .collect();
    missed.sort();

    let expected_missed: Vec<jiff::Timestamp> = [
        "2026-06-01T00:15:00Z",
        "2026-06-01T00:30:00Z",
        "2026-06-01T00:45:00Z",
        "2026-06-01T01:00:00Z",
        "2026-06-01T01:15:00Z",
        "2026-06-01T01:30:00Z",
        "2026-06-01T01:45:00Z",
    ]
    .iter()
    .map(|s| s.parse().unwrap())
    .collect();
    assert_eq!(missed, expected_missed);

    let most_recent: jiff::Timestamp = "2026-06-01T02:00:00Z".parse().unwrap();
    assert!(
        !missed.contains(&most_recent),
        "the most recent occurrence must be the one that actually ran"
    );
}

#[test]
fn gap_between_two_ticks_on_one_daemon_accounts_for_every_elapsed_occurrence() {
    let (_t, paths) = setup(&[
        (
            "quarterly",
            "command = \"true\"\nschedule = \"every 15 minutes\"\n",
        ),
        (
            "minutely",
            "command = \"true\"\nschedule = \"every 1 minute\"\ncatchup = \"all\"\noverlap = \"parallel\"\n",
        ),
    ]);
    let clock = Arc::new(FixedClock::new("2026-06-01T00:00:00Z".parse().unwrap()));
    let (mut d, spawns) = daemon_with_counting_spawner(paths.clone(), clock.clone());

    d.tick().unwrap();
    clock.advance(jiff::Span::new().hours(2));
    d.tick().unwrap();

    let (ran, missed) = (
        spawns.count_for("quarterly"),
        missed_rows(&paths, "quarterly"),
    );
    assert_eq!(ran + missed, 8, "{ran} ran, {missed} missed");
    assert_eq!(
        (ran, missed),
        (1, 7),
        "catchup = \"once\" is the default: exactly one make-up run, the rest missed"
    );

    let (ran, missed) = (
        spawns.count_for("minutely"),
        missed_rows(&paths, "minutely"),
    );
    assert_eq!(
        ran + missed,
        120,
        "{ran} ran and {missed} were recorded missed"
    );
    assert_eq!(
        (ran, missed),
        (10, 110),
        "catchup = \"all\" is still bounded by CATCHUP_MAX; the overflow is recorded, not run"
    );

    let store = Store::open(&paths.db_path).unwrap();
    let newest: jiff::Timestamp = store
        .recent_runs(Some("quarterly"), 100)
        .unwrap()
        .into_iter()
        .filter(|r| r.status == RunStatus::Missed)
        .map(|r| r.started_at)
        .max()
        .unwrap();
    assert_eq!(
        newest,
        "2026-06-01T01:45:00Z".parse::<jiff::Timestamp>().unwrap()
    );
}

#[test]
fn gap_stays_ordinary_tick_when_it_is_within_normal_tick_cadence() {
    let (_t, paths) = setup(&[(
        "j",
        "command = \"true\"\nschedule = \"every 1 minute\"\ncatchup = \"none\"\n",
    )]);
    let clock = Arc::new(FixedClock::new("2026-06-01T00:00:00Z".parse().unwrap()));
    let (mut d, spawns) = daemon_with_counting_spawner(paths.clone(), clock.clone());
    d.tick().unwrap();

    clock.advance(jiff::Span::new().seconds(60));
    assert_eq!(
        d.tick().unwrap(),
        vec!["j".to_string()],
        "an ordinary tick must fire the job itself, not hand it to catch-up"
    );
    assert_eq!(spawns.count(), 1);
    assert_eq!(missed_rows(&paths, "j"), 0);
}

#[test]
fn gap_goes_through_catch_up_policy_when_it_is_past_normal_tick_cadence() {
    let (_t, paths) = setup(&[(
        "j",
        "command = \"true\"\nschedule = \"every 1 minute\"\ncatchup = \"none\"\n",
    )]);
    let clock = Arc::new(FixedClock::new("2026-06-01T00:00:00Z".parse().unwrap()));
    let (mut d, spawns) = daemon_with_counting_spawner(paths.clone(), clock.clone());
    d.tick().unwrap();

    clock.advance(jiff::Span::new().seconds(61));
    assert!(d.tick().unwrap().is_empty());
    assert_eq!(spawns.count(), 0, "catchup = \"none\" runs nothing");
    assert_eq!(
        missed_rows(&paths, "j"),
        1,
        "the occurrence must still be recorded rather than dropped"
    );
}

#[test]
fn consumed_gap_is_not_replayed_by_daemon_that_starts_next() {
    let (_t, paths) = setup(&[(
        "j",
        "command = \"true\"\nschedule = \"every 15 minutes\"\ncatchup = \"once\"\n",
    )]);
    let store = Store::open(&paths.db_path).unwrap();
    store
        .write_heartbeat("2026-06-01T00:00:00Z".parse().unwrap(), 1, "0.1.0")
        .unwrap();
    drop(store);

    let clock = Arc::new(FixedClock::new("2026-06-01T02:00:00Z".parse().unwrap()));
    let (mut first, first_spawns) = daemon_with_counting_spawner(paths.clone(), clock.clone());
    first.tick().unwrap();
    let consumed = missed_rows(&paths, "j");
    assert_eq!((first_spawns.count(), consumed), (1, 7));

    drop(first);

    let (mut second, second_spawns) = daemon_with_counting_spawner(paths.clone(), clock.clone());
    second.tick().unwrap();

    assert_eq!(
        second_spawns.count(),
        0,
        "a gap already consumed must not spawn a second round of make-up runs"
    );
    assert_eq!(
        missed_rows(&paths, "j"),
        consumed,
        "a gap already consumed must not write its `missed` rows a second time"
    );
}

#[test]
fn catch_up_spawn_failure_records_occurrences_it_abandons() {
    let (_t, paths) = setup(&[(
        "j",
        "command = \"true\"\nschedule = \"every 1 minute\"\ncatchup = \"all\"\noverlap = \"parallel\"\n",
    )]);
    let store = Store::open(&paths.db_path).unwrap();
    store
        .write_heartbeat("2026-06-01T00:00:00Z".parse().unwrap(), 1, "0.1.0")
        .unwrap();
    drop(store);

    let clock = Arc::new(FixedClock::new("2026-06-01T01:00:00Z".parse().unwrap()));
    let spawner = FakeSpawner::new(true, paths.db_path.clone(), clock.clone());
    let mut d = Daemon::with_spawner(paths.clone(), clock, spawner.clone()).unwrap();

    assert!(
        d.tick().unwrap().is_empty(),
        "no job actually started this tick"
    );
    assert_eq!(spawner.attempts(), 1, "it gives up after the first failure");
    assert_eq!(missed_rows(&paths, "j"), 60);
}

fn accounted_for_after_a_restart_gap(gap_seconds: i64) -> (usize, usize) {
    let (_t, paths) = setup(&[(
        "j",
        "command = \"true\"\nschedule = \"every 1 minute\"\ncatchup = \"all\"\noverlap = \"parallel\"\n",
    )]);
    let beat: jiff::Timestamp = "2026-06-01T00:00:30Z".parse().unwrap();
    let store = Store::open(&paths.db_path).unwrap();
    store.write_heartbeat(beat, 1, "0.1.0").unwrap();
    drop(store);

    let now = beat + jiff::Span::new().seconds(gap_seconds);
    let clock = Arc::new(FixedClock::new(now));
    let (mut d, spawns) = daemon_with_counting_spawner(paths.clone(), clock);
    d.tick().unwrap();
    (spawns.count(), missed_rows(&paths, "j"))
}

#[test]
fn restart_gap_shorter_than_a_running_daemon_s_own_cadence_is_still_caught_up() {
    for gap in [40, 50, 70] {
        let (ran, missed) = accounted_for_after_a_restart_gap(gap);
        assert_eq!(ran + missed, 1, "gap={gap}s: {ran} ran, {missed} missed");
        assert_eq!(
            (ran, missed),
            (1, 0),
            "catchup = \"all\" must actually run it, not merely record it missed"
        );
    }
}

#[test]
fn daemon_that_has_watched_clock_still_treats_its_own_cadence_as_ordinary() {
    let (_t, paths) = setup(&[(
        "j",
        "command = \"true\"\nschedule = \"every 1 minute\"\ncatchup = \"none\"\n",
    )]);
    let store = Store::open(&paths.db_path).unwrap();
    store
        .write_heartbeat("2026-06-01T00:00:30Z".parse().unwrap(), 1, "0.1.0")
        .unwrap();
    drop(store);

    let clock = Arc::new(FixedClock::new("2026-06-01T00:00:40Z".parse().unwrap()));
    let (mut d, spawns) = daemon_with_counting_spawner(paths.clone(), clock.clone());
    d.tick().unwrap();
    assert_eq!(
        (spawns.count(), missed_rows(&paths, "j")),
        (0, 0),
        "no occurrence falls inside (00:00:30, 00:00:40]"
    );

    clock.advance(jiff::Span::new().seconds(40));
    assert_eq!(
        d.tick().unwrap(),
        vec!["j".to_string()],
        "an ordinary wake must fire the job itself, not hand it to catch-up"
    );
    assert_eq!(missed_rows(&paths, "j"), 0);
}

#[test]
fn failing_catch_up_does_not_freeze_daemon_s_liveness_signal() {
    let (_t, paths) = setup(&[(
        "j",
        "command = \"true\"\nschedule = \"every 1 minute\"\ncatchup = \"all\"\noverlap = \"parallel\"\n",
    )]);
    let store = Store::open(&paths.db_path).unwrap();
    store
        .write_heartbeat("2026-06-01T00:00:00Z".parse().unwrap(), 1, "0.1.0")
        .unwrap();
    drop(store);

    let now: jiff::Timestamp = "2026-06-01T01:00:00Z".parse().unwrap();
    let clock = Arc::new(FixedClock::new(now));
    let spawner = FakeSpawner::new(true, paths.db_path.clone(), clock.clone());
    let mut d = Daemon::with_spawner(paths.clone(), clock, spawner).unwrap();
    d.tick().unwrap();

    let store = Store::open(&paths.db_path).unwrap();
    let beat = store.daemon_heartbeat().unwrap().unwrap();
    assert_eq!(
        beat.at, now,
        "the daemon is alive and must say so even though its catch-up failed"
    );
}

#[test]
fn committed_catch_up_retires_gap_it_consumed() {
    let (_t, paths) = setup(&[(
        "j",
        "command = \"true\"\nschedule = \"every 15 minutes\"\ncatchup = \"none\"\n",
    )]);
    let store = Store::open(&paths.db_path).unwrap();
    let beat: jiff::Timestamp = "2026-06-01T00:00:00Z".parse().unwrap();
    store.write_heartbeat(beat, 1, "0.1.0").unwrap();
    drop(store);

    let clock = Arc::new(FixedClock::new("2026-06-01T02:00:00Z".parse().unwrap()));
    let (mut d, _spawns) = daemon_with_counting_spawner(paths.clone(), clock.clone());
    d.tick().unwrap();

    let store = Store::open(&paths.db_path).unwrap();
    let beat = store.daemon_heartbeat().unwrap().unwrap();
    assert_eq!(
        beat.caught_up_through,
        Some("2026-06-01T02:00:00Z".parse().unwrap()),
        "a committed catch-up must retire the gap it consumed"
    );
    assert_eq!(
        missed_rows(&paths, "j"),
        8,
        "and it must have consumed the whole gap"
    );
}

#[test]
fn gap_left_unconsumed_is_retried_even_though_heartbeat_moved_on() {
    let (_t, paths) = setup(&[(
        "j",
        "command = \"true\"\nschedule = \"every 15 minutes\"\ncatchup = \"none\"\n",
    )]);
    let now: jiff::Timestamp = "2026-06-01T02:00:00Z".parse().unwrap();
    let store = Store::open(&paths.db_path).unwrap();
    store.write_heartbeat(now, 1, "0.1.0").unwrap();
    store
        .set_caught_up_through("2026-06-01T00:00:00Z".parse().unwrap())
        .unwrap();
    drop(store);

    let clock = Arc::new(FixedClock::new(now));
    let (mut d, _spawns) = daemon_with_counting_spawner(paths.clone(), clock);
    d.tick().unwrap();

    assert_eq!(missed_rows(&paths, "j"), 8);
}

#[test]
fn catch_up_spawn_failure_on_one_job_does_not_lose_others_their_occurrences() {
    let body = "command = \"true\"\nschedule = \"every 1 minute\"\ncatchup = \"all\"\noverlap = \"parallel\"\n";
    let (_t, paths) = setup(&[("a", body), ("b", body), ("c", body)]);
    let store = Store::open(&paths.db_path).unwrap();
    store
        .write_heartbeat("2026-06-01T00:00:00Z".parse().unwrap(), 1, "0.1.0")
        .unwrap();
    drop(store);

    let clock = Arc::new(FixedClock::new("2026-06-01T01:00:00Z".parse().unwrap()));
    let spawner = FakeSpawner::failing_only("a", paths.db_path.clone(), clock.clone());
    let mut d = Daemon::with_spawner(paths.clone(), clock, spawner.clone()).unwrap();

    d.tick().unwrap();

    assert_eq!((spawner.count_for("a"), missed_rows(&paths, "a")), (1, 60));
    for job in ["b", "c"] {
        assert_eq!(
            (spawner.count_for(job), missed_rows(&paths, job)),
            (10, 50),
            "job {job:?} comes after the failing one and must still be caught up"
        );
    }
}

#[test]
fn pre_commit_catch_up_failure_leaves_gap_to_be_found_again() {
    let (_t, paths) = setup(&[(
        "j",
        "command = \"true\"\nschedule = \"every 1 minute\"\ncatchup = \"none\"\n",
    )]);
    let store = Store::open(&paths.db_path).unwrap();
    store
        .write_heartbeat("2026-06-01T00:00:20Z".parse().unwrap(), 1, "0.1.0")
        .unwrap();
    assert_eq!(
        store.daemon_heartbeat().unwrap().unwrap().caught_up_through,
        None,
        "the fixture must be a store that predates the watermark column"
    );
    drop(store);

    let conn = rusqlite::Connection::open(&paths.db_path).unwrap();
    conn.execute_batch(
        "CREATE TRIGGER refuse_missed BEFORE INSERT ON runs
         WHEN NEW.status = 'missed'
         BEGIN SELECT RAISE(ABORT, 'runs is refusing this write'); END;",
    )
    .unwrap();
    drop(conn);

    let clock = Arc::new(FixedClock::new("2026-06-01T00:01:05Z".parse().unwrap()));
    let (mut d, spawns) = daemon_with_counting_spawner(paths.clone(), clock.clone());
    assert!(
        d.tick().is_err(),
        "the refused write must surface — without it this test proves nothing"
    );
    assert_eq!(
        (spawns.count(), missed_rows(&paths, "j")),
        (0, 0),
        "the transaction rolled back, so nothing was recorded"
    );

    let conn = rusqlite::Connection::open(&paths.db_path).unwrap();
    conn.execute_batch("DROP TRIGGER refuse_missed").unwrap();
    drop(conn);

    clock.advance(jiff::Span::new().seconds(10));
    d.tick().unwrap();

    assert_eq!(missed_rows(&paths, "j"), 1);
}

#[test]
fn post_commit_catch_up_failure_does_not_make_next_ordinary_wake_outage() {
    let body = "command = \"true\"\nschedule = \"every 1 minute\"\ncatchup = \"none\"\n";
    let (_t, paths) = setup(&[("a", body), ("b", body)]);
    let store = Store::open(&paths.db_path).unwrap();
    store
        .write_heartbeat("2026-06-01T00:00:20Z".parse().unwrap(), 1, "0.1.0")
        .unwrap();
    drop(store);

    let conn = rusqlite::Connection::open(&paths.db_path).unwrap();
    conn.execute_batch(
        "CREATE TRIGGER refuse_a BEFORE INSERT ON job_state
         WHEN NEW.job = 'a'
         BEGIN SELECT RAISE(ABORT, 'job_state is refusing this write'); END;",
    )
    .unwrap();
    drop(conn);

    let clock = Arc::new(FixedClock::new("2026-06-01T00:00:30Z".parse().unwrap()));
    let (mut d, spawns) = daemon_with_counting_spawner(paths.clone(), clock.clone());
    d.tick().unwrap();
    let store = Store::open(&paths.db_path).unwrap();
    assert!(
        store.job_state("a").unwrap().is_none(),
        "the refused write must have actually failed — without it this test proves nothing"
    );
    assert!(
        store.job_state("b").unwrap().is_some(),
        "an unrelated job's own job_state write must not be swept up in another job's refusal"
    );
    drop(store);
    assert_eq!(
        (
            spawns.count(),
            missed_rows(&paths, "a"),
            missed_rows(&paths, "b")
        ),
        (0, 0, 0)
    );

    let conn = rusqlite::Connection::open(&paths.db_path).unwrap();
    conn.execute_batch("DROP TRIGGER refuse_a").unwrap();
    drop(conn);

    clock.advance(jiff::Span::new().seconds(40));
    assert_eq!(
        d.tick().unwrap(),
        vec!["a".to_string(), "b".to_string()],
        "a daemon that watched this window must fire both jobs itself"
    );
    assert_eq!((missed_rows(&paths, "a"), missed_rows(&paths, "b")), (0, 0));
}

#[test]
fn refused_row_leaves_whole_gap_to_be_caught_up_again() {
    let (_t, paths) = setup(&[(
        "j",
        "command = \"true\"\nschedule = \"every 1 minute\"\ncatchup = \"all\"\noverlap = \"parallel\"\n",
    )]);
    let since: jiff::Timestamp = "2026-06-01T00:00:00Z".parse().unwrap();
    let store = Store::open(&paths.db_path).unwrap();
    store.write_heartbeat(since, 1, "0.1.0").unwrap();
    drop(store);

    let refused: jiff::Timestamp = "2026-06-01T00:51:00Z".parse().unwrap();
    let conn = rusqlite::Connection::open(&paths.db_path).unwrap();
    conn.execute(
        &format!(
            "CREATE TRIGGER refuse_one BEFORE INSERT ON runs
             WHEN NEW.status = 'missed' AND NEW.started_at = {}
             BEGIN SELECT RAISE(ABORT, 'runs is refusing this write'); END;",
            refused.as_millisecond()
        ),
        [],
    )
    .unwrap();
    drop(conn);

    let clock = Arc::new(FixedClock::new("2026-06-01T01:00:00Z".parse().unwrap()));
    let spawner = FakeSpawner::new(false, paths.db_path.clone(), clock.clone());
    let mut d = Daemon::with_spawner(paths.clone(), clock, spawner.clone()).unwrap();

    assert!(
        d.tick().is_err(),
        "the refused write must surface as an error"
    );
    assert_eq!(
        (spawner.attempts(), missed_rows(&paths, "j")),
        (0, 0),
        "a gap whose rows could not all be written must not be half-consumed"
    );

    let store = Store::open(&paths.db_path).unwrap();
    assert_eq!(
        store.daemon_heartbeat().unwrap().unwrap().caught_up_through,
        Some(since)
    );
}

#[test]
fn rearm_failure_on_one_job_does_not_lose_others_their_occurrences() {
    let body = "command = \"true\"\nschedule = \"every 1 minute\"\ncatchup = \"all\"\noverlap = \"parallel\"\n";
    let (_t, paths) = setup(&[("a", body), ("b", body), ("c", body)]);
    let store = Store::open(&paths.db_path).unwrap();
    store
        .write_heartbeat("2026-06-01T00:00:00Z".parse().unwrap(), 1, "0.1.0")
        .unwrap();
    drop(store);

    let conn = rusqlite::Connection::open(&paths.db_path).unwrap();
    conn.execute_batch(
        "CREATE TRIGGER refuse_a BEFORE INSERT ON job_state
         WHEN NEW.job = 'a'
         BEGIN SELECT RAISE(ABORT, 'job_state is refusing this write'); END;",
    )
    .unwrap();
    drop(conn);

    let clock = Arc::new(FixedClock::new("2026-06-01T01:00:00Z".parse().unwrap()));
    let (mut d, spawns) = daemon_with_counting_spawner(paths.clone(), clock);

    d.tick().unwrap();
    let store = Store::open(&paths.db_path).unwrap();
    assert!(
        store.job_state("a").unwrap().is_none(),
        "the refused write must have actually failed — without it this test proves nothing"
    );
    for job in ["b", "c"] {
        assert!(
            store.job_state(job).unwrap().is_some(),
            "job {job:?}'s own job_state write must not be swept up in \"a\"'s refusal"
        );
    }
    drop(store);

    for job in ["a", "b", "c"] {
        assert_eq!(
            (spawns.count_for(job), missed_rows(&paths, job)),
            (10, 50),
            "job {job:?}"
        );
    }
}

#[test]
fn reconcile_never_declares_live_run_dead_while_descendant_holds_it_open() {
    let (_t, paths) = setup(&[(
        "held",
        "command = \"sleep 1 &\"\nschedule = \"hourly\"\noverlap = \"skip\"\n",
    )]);
    let job = nightjar_config::Job::load(&paths.jobs_dir.join("held.toml")).unwrap();

    let clock = Arc::new(FixedClock::new("2026-06-01T00:00:00Z".parse().unwrap()));
    let d = Daemon::new(paths.clone(), clock).unwrap();

    let done = Arc::new(AtomicBool::new(false));
    let runner = {
        let (paths, done) = (paths.clone(), done.clone());
        std::thread::spawn(move || {
            let store = Store::open(&paths.db_path).unwrap();
            let out = nightjar_runner::execute(
                &job,
                "held-run",
                Trigger::Manual,
                &paths,
                &store,
                &nightjar_core::clock::SystemClock,
                nightjar_runner::DEFAULT_OUTPUT_CAP,
                &nightjar_runner::notify::RecordingNotifier::default(),
                None,
            );
            done.store(true, Ordering::SeqCst);
            out.unwrap()
        })
    };

    let store = Store::open(&paths.db_path).unwrap();
    let mut ever_unknown = false;
    let deadline = Instant::now() + Duration::from_secs(30);
    while !done.load(Ordering::SeqCst) && Instant::now() < deadline {
        d.reconcile().unwrap();
        if let Some(run) = store.get_run("held-run").unwrap() {
            ever_unknown |= run.status == RunStatus::Unknown;
        }
    }
    runner.join().unwrap();

    assert!(!ever_unknown);
}

#[test]
fn missed_row_names_no_output_files() {
    let (_t, paths) = setup(&[(
        "j",
        "command = \"true\"\nschedule = \"every 15 minutes\"\ncatchup = \"none\"\n",
    )]);
    let store = Store::open(&paths.db_path).unwrap();
    store
        .write_heartbeat("2026-06-01T00:00:00Z".parse().unwrap(), 1, "0.1.0")
        .unwrap();
    drop(store);

    let clock = Arc::new(FixedClock::new("2026-06-01T02:00:00Z".parse().unwrap()));
    let (mut d, _spawns) = daemon_with_counting_spawner(paths.clone(), clock);
    d.tick().unwrap();

    let store = Store::open(&paths.db_path).unwrap();
    let missed: Vec<_> = store
        .recent_runs(Some("j"), 100)
        .unwrap()
        .into_iter()
        .filter(|r| r.status == RunStatus::Missed)
        .collect();
    assert_eq!(missed.len(), 8);
    assert!(
        missed
            .iter()
            .all(|r| r.stdout_path.is_none() && r.stderr_path.is_none()),
        "a missed row must not name files that will never exist"
    );
    assert!(
        missed.iter().all(|r| r.finished_at == Some(r.started_at)),
        "missed row not written terminal"
    );
}

#[test]
fn pid_less_row_younger_than_grace_is_left_alone() {
    let (_t, paths) = setup(&[("j", "command = \"true\"\nschedule = \"hourly\"\n")]);
    let store = Store::open(&paths.db_path).unwrap();
    let t: jiff::Timestamp = "2026-06-01T00:00:00Z".parse().unwrap();
    store
        .start_run(
            "midwrite",
            "j",
            Trigger::Manual,
            t,
            &paths.runs_dir.join("j/midwrite.out"),
            &paths.runs_dir.join("j/midwrite.err"),
        )
        .unwrap();
    drop(store);

    let clock = Arc::new(FixedClock::new("2026-06-01T00:00:10Z".parse().unwrap()));
    let _d = Daemon::new(paths.clone(), clock).unwrap();

    let store = Store::open(&paths.db_path).unwrap();
    assert_eq!(
        store.get_run("midwrite").unwrap().unwrap().status,
        RunStatus::Running
    );
}

#[test]
fn stale_row_appearing_after_startup_is_reconciled_without_restart() {
    let (_t, paths) = setup(&[(
        "j",
        "command = \"true\"\nschedule = \"every 15 minutes\"\noverlap = \"skip\"\n",
    )]);
    let clock = Arc::new(FixedClock::new("2026-06-01T00:00:00Z".parse().unwrap()));
    let (mut d, spawns) = daemon_with_counting_spawner(paths.clone(), clock.clone());
    d.tick().unwrap();

    let store = Store::open(&paths.db_path).unwrap();
    store
        .start_run(
            "orphaned",
            "j",
            Trigger::Manual,
            clock.now(),
            &paths.runs_dir.join("j/orphaned.out"),
            &paths.runs_dir.join("j/orphaned.err"),
        )
        .unwrap();
    store.set_run_pid("orphaned", 0x7FFF_FFFF).unwrap();
    drop(store);

    clock.advance(jiff::Span::new().minutes(16));
    d.tick().unwrap();

    let store = Store::open(&paths.db_path).unwrap();
    assert_eq!(
        store.get_run("orphaned").unwrap().unwrap().status,
        RunStatus::Unknown
    );
    assert_eq!(
        spawns.count(),
        1,
        "overlap = \"skip\" must not be silenced forever by a dead wrapper's row"
    );
}

#[derive(Default)]
struct MuteSpawner {
    pids: Mutex<Vec<u32>>,
}

impl MuteSpawner {
    fn pids(&self) -> Vec<u32> {
        self.pids.lock().unwrap().clone()
    }
}

impl Spawner for MuteSpawner {
    fn spawn(&self, _job: &str, _run_id: &str, _trigger: Trigger) -> anyhow::Result<Child> {
        let child = Command::new("/bin/sh")
            .args(["-c", "exit 3"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()?;
        self.pids.lock().unwrap().push(child.id());
        Ok(child)
    }
}

#[test]
fn wrapper_that_dies_before_recording_still_leaves_row() {
    let (_t, paths) = setup(&[("j", "command = \"true\"\nschedule = \"* * * * * *\"\n")]);
    let clock = Arc::new(FixedClock::new("2026-06-01T00:00:00Z".parse().unwrap()));
    let mut d = Daemon::with_spawner(
        paths.clone(),
        clock.clone(),
        Arc::new(MuteSpawner::default()),
    )
    .unwrap();
    d.tick().unwrap();
    clock.advance(jiff::Span::new().seconds(1));
    d.tick().unwrap();

    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        d.tick().unwrap();
        let store = Store::open(&paths.db_path).unwrap();
        let runs = store.recent_runs(Some("j"), 10).unwrap();
        if runs.iter().any(|r| r.status == RunStatus::Unknown) {
            assert_eq!(runs.len(), 1, "exactly one row for the one occurrence");
            break;
        }
        assert!(Instant::now() < deadline, "wrapper died before start_run");
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn make_up_run_that_records_nothing_leaves_its_occurrence_missed() {
    let (_t, paths) = setup(&[(
        "j",
        "command = \"true\"\nschedule = \"every 15 minutes\"\ncatchup = \"once\"\n",
    )]);
    let store = Store::open(&paths.db_path).unwrap();
    store
        .write_heartbeat("2026-06-01T00:00:00Z".parse().unwrap(), 1, "0.1.0")
        .unwrap();
    drop(store);

    let clock = Arc::new(FixedClock::new("2026-06-01T02:00:00Z".parse().unwrap()));
    let spawner = Arc::new(MuteSpawner::default());
    let mut d = Daemon::with_spawner(paths.clone(), clock, spawner.clone()).unwrap();
    d.tick().unwrap();

    let pid = spawner.pids()[0];
    let deadline = Instant::now() + Duration::from_secs(10);
    while pid_exists(pid) {
        assert!(Instant::now() < deadline, "the wrapper was never reaped");
        std::thread::sleep(Duration::from_millis(20));
        d.tick().unwrap();
    }

    let store = Store::open(&paths.db_path).unwrap();
    let rows = store.recent_runs(Some("j"), 100).unwrap();
    assert_eq!(rows.len(), 8, "one row per occurrence in the gap, no more");
    assert!(
        rows.iter().all(|r| r.status == RunStatus::Missed),
        "found {:?}",
        rows.iter().map(|r| r.status).collect::<Vec<_>>()
    );
}

#[test]
fn flood_of_missed_rows_does_not_evict_job_s_real_history() {
    let (_t, paths) = setup(&[]);
    let clock = Arc::new(FixedClock::new("2026-06-01T00:00:00Z".parse().unwrap()));
    let store = Store::open(&paths.db_path).unwrap();
    let base = clock.now();

    for i in 0..60 {
        let at = base + jiff::Span::new().minutes(i);
        write_finished_run_with_files(&store, &paths, "j", &format!("real{i}"), at);
    }

    let outage = base + jiff::Span::new().hours(2);
    for i in 0..191 {
        let at = outage + jiff::Span::new().minutes(15 * i);
        let id = format!("missed{i}");
        let stub = paths.runs_dir.join(format!("j/{id}.out"));
        store
            .start_run(&id, "j", Trigger::Catchup, at, &stub, &stub)
            .unwrap();
        store
            .finish_run(&id, RunStatus::Missed, None, at, 0)
            .unwrap();
    }
    drop(store);

    let mut d = Daemon::new(paths.clone(), clock.clone()).unwrap();
    d.tick().unwrap();
    clock.advance(jiff::Span::new().hours(1));
    d.tick().unwrap();

    let store = Store::open(&paths.db_path).unwrap();
    let left = store.recent_runs(Some("j"), 1000).unwrap();
    let real = left
        .iter()
        .filter(|r| r.status != RunStatus::Missed)
        .count();
    assert_eq!(
        real, 50,
        "placeholders for runs that never happened must not evict real history"
    );
    assert!(
        paths.runs_dir.join("j/real59.out").exists(),
        "newest real run's output must survive the flood"
    );
    assert_eq!(
        left.iter()
            .filter(|r| r.status == RunStatus::Missed)
            .count(),
        50,
        "`missed` rows are still bounded, just on their own budget"
    );
}

#[test]
fn wrapper_that_exits_without_finishing_its_row_does_not_silence_job() {
    let (_t, paths) = setup(&[(
        "j",
        "command = \"true\"\nschedule = \"every 15 minutes\"\noverlap = \"skip\"\n",
    )]);
    let clock = Arc::new(FixedClock::new("2026-06-01T00:00:00Z".parse().unwrap()));
    let spawner = AbandoningSpawner::new(paths.db_path.clone(), clock.clone());
    let mut d = Daemon::with_spawner(paths.clone(), clock.clone(), spawner.clone()).unwrap();
    d.tick().unwrap();
    clock.advance(jiff::Span::new().minutes(16));
    d.tick().unwrap();
    assert_eq!(spawner.count(), 1);

    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        d.tick().unwrap();
        let store = Store::open(&paths.db_path).unwrap();
        if store.running_count("j").unwrap() == 0 {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "the row nobody will ever finish is still `running`"
        );
        std::thread::sleep(Duration::from_millis(20));
    }

    let store = Store::open(&paths.db_path).unwrap();
    let run = store.last_run("j").unwrap().unwrap();
    assert_eq!(
        run.status,
        RunStatus::Unknown,
        "a run that started and cannot be proven to have ended is `unknown`"
    );
    drop(store);

    clock.advance(jiff::Span::new().minutes(16));
    d.tick().unwrap();
    assert_eq!(
        spawner.count(),
        2,
        "overlap = \"skip\" must not be silenced forever by the abandoned row"
    );
}
