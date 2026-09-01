use nightjar_core::clock::{Clock, FixedClock};
use nightjar_daemon::Daemon;
use nightjar_store::Store;
use nightjar_store::run::{RunStatus, Trigger};
use std::sync::Arc;
use std::time::{Duration, Instant};

mod support;
use support::*;

#[test]
fn sweep_does_nothing_before_retention_interval_elapses() {
    let (_t, paths) = setup(&[]);
    let clock = Arc::new(FixedClock::new("2026-06-01T00:00:00Z".parse().unwrap()));
    let store = nightjar_store::Store::open(&paths.db_path).unwrap();
    let t = clock.now();
    for i in 0..60 {
        write_finished_run_with_files(&store, &paths, "j", &format!("r{i}"), t);
    }
    drop(store);

    let mut d = Daemon::new(paths.clone(), clock.clone()).unwrap();
    d.tick().unwrap();

    let store = nightjar_store::Store::open(&paths.db_path).unwrap();
    assert_eq!(
        store.recent_runs(Some("j"), 1000).unwrap().len(),
        60,
        "the very first tick must not sweep before RETENTION_SWEEP has elapsed"
    );
}

#[test]
fn sweep_prunes_to_retention_runs_and_unlinks_orphaned_files_when_due() {
    let (_t, paths) = setup(&[]);
    let clock = Arc::new(FixedClock::new("2026-06-01T00:00:00Z".parse().unwrap()));
    let store = nightjar_store::Store::open(&paths.db_path).unwrap();
    let base = clock.now();
    for i in 0..60 {
        let at = base + jiff::Span::new().minutes(i);
        write_finished_run_with_files(&store, &paths, "j", &format!("r{i}"), at);
    }
    drop(store);

    let mut d = Daemon::new(paths.clone(), clock.clone()).unwrap();
    d.tick().unwrap();

    clock.advance(jiff::Span::new().hours(1));
    d.tick().unwrap();

    let store = nightjar_store::Store::open(&paths.db_path).unwrap();
    let left = store.recent_runs(Some("j"), 1000).unwrap();
    assert_eq!(
        left.len(),
        50,
        "retention must prune down to RETENTION_RUNS"
    );
    assert_eq!(left[0].id, "r59", "the newest runs must survive");

    let pruned_out = paths.runs_dir.join("j/r0.out");
    let kept_out = paths.runs_dir.join("j/r59.out");
    assert!(
        !pruned_out.exists(),
        "a pruned run's output file must be unlinked"
    );
    assert!(kept_out.exists(), "a kept run's output file must survive");
}

#[test]
fn finished_run_is_pruned_immediately_not_only_by_hourly_sweep() {
    let (_t, paths) = setup(&[(
        "busy",
        "command = \"true\"\nschedule = \"every 1 minute\"\n",
    )]);
    std::fs::write(paths.config_dir.join("config.toml"), "retention_runs = 3\n").unwrap();

    let clock = Arc::new(FixedClock::new("2026-06-01T00:00:00Z".parse().unwrap()));
    let spawner = FakeSpawner::new(false, paths.db_path.clone(), clock.clone());
    let mut d = Daemon::with_spawner(paths.clone(), clock.clone(), spawner).unwrap();
    let store = Store::open(&paths.db_path).unwrap();

    d.tick().unwrap();

    for firing in 1..=4u32 {
        clock.advance(jiff::Span::new().seconds(60));
        assert_eq!(d.tick().unwrap(), vec!["busy".to_string()]);

        let deadline = Instant::now() + Duration::from_secs(5);
        while store.running_count("busy").unwrap() > 0 {
            assert!(
                Instant::now() < deadline,
                "firing {firing} never reached a terminal row"
            );
            std::thread::sleep(Duration::from_millis(10));
            d.tick().unwrap();
        }

        let total = store.recent_runs(Some("busy"), 1000).unwrap().len();
        assert!(
            total <= 3,
            "firing {firing}: found {total} rows, {}s elapsed",
            firing * 60
        );
    }
}

#[test]
fn report_exec_exit_prunes_immediately_not_only_reconcile_or_hourly_sweep() {
    let (_t, paths) = setup(&[(
        "busy",
        "command = \"true\"\nschedule = \"every 1 minute\"\n",
    )]);
    std::fs::write(paths.config_dir.join("config.toml"), "retention_runs = 3\n").unwrap();

    let clock = Arc::new(FixedClock::new("2026-06-01T00:00:00Z".parse().unwrap()));
    let spawner = AbandoningSpawner::new(paths.db_path.clone(), clock.clone());
    let mut d = Daemon::with_spawner(paths.clone(), clock.clone(), spawner).unwrap();
    let store = Store::open(&paths.db_path).unwrap();

    d.tick().unwrap();

    for firing in 1..=4u32 {
        clock.advance(jiff::Span::new().seconds(60));
        d.tick().unwrap();

        let deadline = Instant::now() + Duration::from_secs(5);
        while store.running_count("busy").unwrap() > 0 {
            assert!(
                Instant::now() < deadline,
                "firing {firing} never reached a terminal row"
            );
            std::thread::sleep(Duration::from_millis(10));
            d.tick().unwrap();
        }

        let run = store.last_run("busy").unwrap().unwrap();
        assert_eq!(run.status, RunStatus::Unknown);

        let total = store.recent_runs(Some("busy"), 1000).unwrap().len();
        assert!(total <= 3, "firing {firing}: found {total} rows");
    }
}

#[test]
fn evaluate_prunes_immediately_when_it_fires_a_job_not_only_after_that_run_finishes() {
    let (_t, paths) = setup(&[(
        "busy",
        "command = \"true\"\nschedule = \"every 1 minute\"\n",
    )]);
    std::fs::write(paths.config_dir.join("config.toml"), "retention_runs = 3\n").unwrap();

    let clock = Arc::new(FixedClock::new("2026-06-01T00:00:00Z".parse().unwrap()));
    let store = Store::open(&paths.db_path).unwrap();
    for i in 0..5 {
        let id = format!("old{i}");
        let at = clock.now() - jiff::Span::new().minutes(5 - i);
        store
            .start_run(
                &id,
                "busy",
                Trigger::Schedule,
                at,
                &paths.runs_dir.join(format!("busy/{id}.out")),
                &paths.runs_dir.join(format!("busy/{id}.err")),
            )
            .unwrap();
        store
            .finish_run(&id, RunStatus::Success, Some(0), at, 0)
            .unwrap();
    }
    assert_eq!(store.recent_runs(Some("busy"), 1000).unwrap().len(), 5);

    let spawner = FakeSpawner::new(false, paths.db_path.clone(), clock.clone());
    let mut d = Daemon::with_spawner(paths.clone(), clock.clone(), spawner).unwrap();
    d.tick().unwrap();
    clock.advance(jiff::Span::new().seconds(60));
    assert_eq!(d.tick().unwrap(), vec!["busy".to_string()]);

    let total = store.recent_runs(Some("busy"), 1000).unwrap().len();
    assert!(total <= 4, "found {total} rows");
}

#[test]
fn missed_occurrence_is_pruned_immediately_when_it_comes_from_overlap_refusal() {
    let (_t, paths) = setup(&[(
        "blocked",
        "command = \"true\"\nschedule = \"every 1 minute\"\noverlap = \"skip\"\n",
    )]);
    let clock = Arc::new(FixedClock::new("2026-06-01T00:00:00Z".parse().unwrap()));
    let store = Store::open(&paths.db_path).unwrap();

    store
        .start_run(
            "inflight",
            "blocked",
            Trigger::Schedule,
            clock.now(),
            &paths.runs_dir.join("blocked/inflight.out"),
            &paths.runs_dir.join("blocked/inflight.err"),
        )
        .unwrap();
    store.set_run_pid("inflight", std::process::id()).unwrap();

    let mut d = Daemon::new(paths.clone(), clock.clone()).unwrap();
    d.tick().unwrap();

    for _ in 0..60 {
        clock.advance(jiff::Span::new().seconds(60));
        d.tick().unwrap();
    }

    let total = store.recent_runs(Some("blocked"), 1000).unwrap().len();
    assert!(total <= 51, "found {total} rows");
}

#[test]
fn sweep_prunes_runs_older_than_retention_age_even_inside_keep_count() {
    let (_t, paths) = setup(&[]);
    std::fs::write(
        paths.config_dir.join("config.toml"),
        "retention_age = \"7d\"\n",
    )
    .unwrap();
    let clock = Arc::new(FixedClock::new("2026-06-01T00:00:00Z".parse().unwrap()));
    let store = nightjar_store::Store::open(&paths.db_path).unwrap();
    let base = clock.now();
    for i in 0..10 {
        let at = base - jiff::Span::new().hours(i * 24);
        write_finished_run_with_files(&store, &paths, "j", &format!("r{i}"), at);
    }
    drop(store);

    let mut d = Daemon::new(paths.clone(), clock.clone()).unwrap();
    d.tick().unwrap();
    clock.advance(jiff::Span::new().hours(1));
    d.tick().unwrap();

    let store = nightjar_store::Store::open(&paths.db_path).unwrap();
    let left: Vec<String> = store
        .recent_runs(Some("j"), 1000)
        .unwrap()
        .into_iter()
        .map(|r| r.id)
        .collect();
    assert_eq!(
        left,
        vec!["r0", "r1", "r2", "r3", "r4", "r5", "r6"],
        "everything older than retention_age must go, everything inside it must stay"
    );
    assert!(
        !paths.runs_dir.join("j/r9.out").exists(),
        "an age-pruned run's output file must be unlinked"
    );
    assert!(
        paths.runs_dir.join("j/r0.out").exists(),
        "a run inside retention_age must keep its output"
    );
}

#[test]
fn sweep_never_deletes_running_runs_output_file() {
    let (_t, paths) = setup(&[]);
    let clock = Arc::new(FixedClock::new("2026-06-01T00:00:00Z".parse().unwrap()));
    let store = nightjar_store::Store::open(&paths.db_path).unwrap();
    let base = clock.now();
    for i in 0..55 {
        let at = base + jiff::Span::new().minutes(i);
        write_finished_run_with_files(&store, &paths, "j", &format!("r{i}"), at);
    }
    let live_dir = paths.runs_dir.join("j");
    let live_out = live_dir.join("live.out");
    let live_err = live_dir.join("live.err");
    std::fs::write(&live_out, "still writing").unwrap();
    std::fs::write(&live_err, "").unwrap();
    store
        .start_run(
            "live",
            "j",
            Trigger::Schedule,
            base - jiff::Span::new().hours(1),
            &live_out,
            &live_err,
        )
        .unwrap();
    store.set_run_pid("live", std::process::id()).unwrap();
    drop(store);

    let mut d = Daemon::new(paths.clone(), clock.clone()).unwrap();
    d.tick().unwrap();
    clock.advance(jiff::Span::new().hours(1));
    d.tick().unwrap();

    let store = nightjar_store::Store::open(&paths.db_path).unwrap();
    assert_eq!(
        store.get_run("live").unwrap().unwrap().status,
        RunStatus::Running,
        "a running row must never be pruned"
    );
    assert!(
        live_out.exists(),
        "a running run's output file must never be unlinked"
    );
    assert!(live_err.exists());
}

#[test]
fn sweep_refuses_to_unlink_path_outside_runs_directory() {
    let (_t, paths) = setup(&[]);
    let clock = Arc::new(FixedClock::new("2026-06-01T00:00:00Z".parse().unwrap()));
    let store = nightjar_store::Store::open(&paths.db_path).unwrap();
    let base = clock.now();

    for i in 0..55 {
        let at = base + jiff::Span::new().minutes(i);
        write_finished_run_with_files(&store, &paths, "j", &format!("r{i}"), at);
    }

    let decoy = paths.data_dir.join("decoy.txt");
    std::fs::write(&decoy, "must not be touched").unwrap();
    let at = base - jiff::Span::new().minutes(1);
    store
        .start_run("evil", "j", Trigger::Schedule, at, &decoy, &decoy)
        .unwrap();
    store
        .finish_run("evil", RunStatus::Success, Some(0), at, 0)
        .unwrap();
    drop(store);

    let mut d = Daemon::new(paths.clone(), clock.clone()).unwrap();
    d.tick().unwrap();
    clock.advance(jiff::Span::new().hours(1));
    d.tick().unwrap();

    assert!(
        decoy.exists(),
        "a stdout_path outside the runs directory must never be unlinked"
    );
    assert_eq!(
        std::fs::read_to_string(&decoy).unwrap(),
        "must not be touched"
    );
}

#[test]
fn sweep_deletes_job_state_for_job_removed_from_disk() {
    let (_t, paths) = setup(&[("keep", "command = \"true\"\nschedule = \"hourly\"\n")]);
    let clock = Arc::new(FixedClock::new("2026-06-01T00:00:00Z".parse().unwrap()));
    let store = nightjar_store::Store::open(&paths.db_path).unwrap();
    let stale: jiff::Timestamp = "2026-05-01T00:00:00Z".parse().unwrap();
    store.set_next_run("vanished", Some(stale)).unwrap();
    drop(store);

    let mut d = Daemon::new(paths.clone(), clock.clone()).unwrap();
    d.tick().unwrap();
    clock.advance(jiff::Span::new().hours(1));
    d.tick().unwrap();

    let store = nightjar_store::Store::open(&paths.db_path).unwrap();
    assert!(
        store.job_state("vanished").unwrap().is_none(),
        "job_state for a job no longer on disk must be deleted"
    );
}

#[test]
fn sweep_preserves_job_state_for_job_that_still_exists_but_fails_to_parse() {
    let (_t, paths) = setup(&[("broken", "command = = =\n")]);
    let clock = Arc::new(FixedClock::new("2026-06-01T00:00:00Z".parse().unwrap()));
    let store = nightjar_store::Store::open(&paths.db_path).unwrap();
    let stale: jiff::Timestamp = "2026-05-01T00:00:00Z".parse().unwrap();
    store.set_next_run("broken", Some(stale)).unwrap();
    drop(store);

    let mut d = Daemon::new(paths.clone(), clock.clone()).unwrap();
    d.tick().unwrap();
    clock.advance(jiff::Span::new().hours(1));
    d.tick().unwrap();

    let store = nightjar_store::Store::open(&paths.db_path).unwrap();
    assert!(
        store.job_state("broken").unwrap().is_some(),
        "job_state swept away as if deleted"
    );
}

#[test]
fn sweep_never_touches_job_state_when_jobs_directory_is_entirely_missing() {
    let (_t, paths) = setup(&[]);
    let clock = Arc::new(FixedClock::new("2026-06-01T00:00:00Z".parse().unwrap()));
    let store = nightjar_store::Store::open(&paths.db_path).unwrap();
    let stale: jiff::Timestamp = "2026-05-01T00:00:00Z".parse().unwrap();
    store.set_next_run("orphan", Some(stale)).unwrap();
    drop(store);

    let mut d = Daemon::new(paths.clone(), clock.clone()).unwrap();
    d.tick().unwrap();

    std::fs::remove_dir_all(&paths.jobs_dir).unwrap();

    clock.advance(jiff::Span::new().hours(1));
    d.tick().unwrap();

    let store = nightjar_store::Store::open(&paths.db_path).unwrap();
    assert!(
        store.job_state("orphan").unwrap().is_some(),
        "a missing jobs directory must never be mistaken for \"every job deleted\""
    );
}

#[test]
fn sweep_fires_shortly_after_restart_even_though_full_interval_has_not_elapsed() {
    let (_t, paths) = setup(&[]);
    let clock = Arc::new(FixedClock::new("2026-06-01T00:00:00Z".parse().unwrap()));
    let store = nightjar_store::Store::open(&paths.db_path).unwrap();
    let base = clock.now();
    for i in 0..60 {
        let at = base + jiff::Span::new().minutes(i);
        write_finished_run_with_files(&store, &paths, "j", &format!("r{i}"), at);
    }
    drop(store);

    let mut d = Daemon::new(paths.clone(), clock.clone()).unwrap();

    clock.advance(jiff::Span::new().minutes(6));
    d.tick().unwrap();

    let store = nightjar_store::Store::open(&paths.db_path).unwrap();
    assert_eq!(store.recent_runs(Some("j"), 1000).unwrap().len(), 50);
}
