use jiff::Timestamp;
use nightjar_store::Store;
use nightjar_store::run::{RunStatus, Trigger};
use std::path::Path;

#[test]
fn missed_row_is_written_terminal_with_no_output_paths() {
    let store = Store::open_in_memory().unwrap();
    let t: Timestamp = "2026-06-01T00:15:00Z".parse().unwrap();
    store
        .record_missed_run("m1", "backup", Trigger::Catchup, t)
        .unwrap();

    let run = store.get_run("m1").unwrap().unwrap();
    assert_eq!(run.status, RunStatus::Missed);
    assert_eq!(run.trigger, Trigger::Catchup);
    assert_eq!(run.started_at, t);
    assert_eq!(
        run.finished_at,
        Some(t),
        "the row is terminal the moment it is written"
    );
    assert_eq!(run.duration_ms, Some(0));
    assert_eq!(run.exit_code, None);
    assert_eq!(run.pid, None);
    assert_eq!(run.output_bytes, 0);
    assert_eq!(
        (run.stdout_path, run.stderr_path),
        (None, None),
        "no process ran, so no output file will ever exist to name"
    );
}

#[test]
fn run_replaces_the_missed_row_rather_than_adding_a_second_when_started_under_its_id() {
    let store = Store::open_in_memory().unwrap();
    let occurrence: Timestamp = "2026-06-01T00:15:00Z".parse().unwrap();
    let started: Timestamp = "2026-06-01T02:00:00Z".parse().unwrap();
    store
        .record_missed_run("r1", "backup", Trigger::Catchup, occurrence)
        .unwrap();
    store
        .start_run(
            "r1",
            "backup",
            Trigger::Catchup,
            started,
            Path::new("/tmp/o"),
            Path::new("/tmp/e"),
        )
        .unwrap();

    assert_eq!(
        store.recent_runs(Some("backup"), 100).unwrap().len(),
        1,
        "one occurrence, one row"
    );
    let run = store.get_run("r1").unwrap().unwrap();
    assert_eq!(run.status, RunStatus::Running);
    assert_eq!(run.started_at, started);
    assert_eq!(
        run.finished_at, None,
        "the placeholder's terminal stamp must not survive as the run's"
    );
    assert_eq!((run.duration_ms, run.pid), (None, None));
    assert_eq!(
        run.stdout_path.as_deref(),
        Some(Path::new("/tmp/o")),
        "the run's own capture files, which the placeholder had none of"
    );
}

#[test]
fn start_run_is_still_rejected_when_the_id_is_not_a_placeholder() {
    let store = Store::open_in_memory().unwrap();
    let t: Timestamp = "2026-06-01T00:00:00Z".parse().unwrap();
    for _ in 0..2 {
        let second = store.start_run(
            "r1",
            "j",
            Trigger::Manual,
            t,
            Path::new("/tmp/o"),
            Path::new("/tmp/e"),
        );
        if let Err(e) = second {
            assert!(e.to_string().contains("r1"), "got: {e}");
            return;
        }
    }
    panic!("a second `start_run` on a live row must not silently overwrite it");
}

#[test]
fn pruned_missed_rows_contribute_no_paths_to_unlink() {
    let store = Store::open_in_memory().unwrap();
    for i in 0..5 {
        let t = Timestamp::from_second(1_800_000_000 + i * 60).unwrap();
        store
            .record_missed_run(&format!("m{i}"), "j", Trigger::Catchup, t)
            .unwrap();
    }

    let orphaned = store.prune_runs("j", 50, 2).unwrap();
    assert_eq!(
        store.recent_runs(Some("j"), 100).unwrap().len(),
        2,
        "three of the five must have been pruned"
    );
    assert!(
        orphaned.is_empty(),
        "pruning a missed row has nothing to unlink; got {orphaned:?}"
    );
}

#[test]
fn prune_keeps_the_newest_and_returns_the_orphaned_paths() {
    let store = Store::open_in_memory().unwrap();
    let base: Timestamp = "2026-06-01T00:00:00Z".parse().unwrap();
    for i in 0..10 {
        let id = format!("r{i}");
        let at = base + jiff::Span::new().minutes(i);
        store
            .start_run(
                &id,
                "j",
                Trigger::Schedule,
                at,
                Path::new(&format!("/tmp/{id}.out")),
                Path::new(&format!("/tmp/{id}.err")),
            )
            .unwrap();
        store
            .finish_run(&id, RunStatus::Success, Some(0), at, 0)
            .unwrap();
        store.set_after_fired_at(&id, at).unwrap();
    }

    let orphaned = store.prune_runs("j", 3, 3).unwrap();
    let left = store.recent_runs(Some("j"), 100).unwrap();
    assert_eq!(left.len(), 3);
    assert_eq!(left[0].id, "r9", "newest survives");
    assert_eq!(orphaned.len(), 14, "two files per pruned run");
}

#[test]
fn prune_does_not_delete_a_running_row() {
    let store = Store::open_in_memory().unwrap();
    let base: Timestamp = "2026-06-01T00:00:00Z".parse().unwrap();
    for i in 0..5 {
        let id = format!("r{i}");
        let at = base + jiff::Span::new().minutes(i);
        store
            .start_run(
                &id,
                "j",
                Trigger::Schedule,
                at,
                Path::new("/tmp/o"),
                Path::new("/tmp/e"),
            )
            .unwrap();
        store
            .finish_run(&id, RunStatus::Success, Some(0), at, 0)
            .unwrap();
        store.set_after_fired_at(&id, at).unwrap();
    }
    store
        .start_run(
            "live",
            "j",
            Trigger::Schedule,
            base,
            Path::new("/tmp/o"),
            Path::new("/tmp/e"),
        )
        .unwrap();

    store.prune_runs("j", 2, 2).unwrap();
    assert!(
        store.get_run("live").unwrap().is_some(),
        "an in-flight run must survive pruning"
    );
}

#[test]
fn prune_does_not_count_a_running_row_toward_the_keep_window() {
    let store = Store::open_in_memory().unwrap();
    let base: Timestamp = "2026-06-01T00:00:00Z".parse().unwrap();
    for i in 0..5 {
        let id = format!("r{i}");
        let at = base + jiff::Span::new().minutes(i);
        store
            .start_run(
                &id,
                "j",
                Trigger::Schedule,
                at,
                Path::new("/tmp/o"),
                Path::new("/tmp/e"),
            )
            .unwrap();
        store
            .finish_run(&id, RunStatus::Success, Some(0), at, 0)
            .unwrap();
        store.set_after_fired_at(&id, at).unwrap();
    }
    store
        .start_run(
            "live",
            "j",
            Trigger::Schedule,
            base + jiff::Span::new().minutes(5),
            Path::new("/tmp/o"),
            Path::new("/tmp/e"),
        )
        .unwrap();

    let orphaned = store.prune_runs("j", 5, 5).unwrap();
    assert!(orphaned.is_empty(), "nothing should be pruned yet");
    assert_eq!(store.recent_runs(Some("j"), 100).unwrap().len(), 6);
}

#[test]
fn prune_runs_is_scoped_to_the_named_job() {
    let store = Store::open_in_memory().unwrap();
    let base: Timestamp = "2026-06-01T00:00:00Z".parse().unwrap();
    for job in ["a", "b"] {
        for i in 0..5 {
            let id = format!("{job}-{i}");
            let at = base + jiff::Span::new().minutes(i);
            store
                .start_run(
                    &id,
                    job,
                    Trigger::Schedule,
                    at,
                    Path::new("/tmp/o"),
                    Path::new("/tmp/e"),
                )
                .unwrap();
            store
                .finish_run(&id, RunStatus::Success, Some(0), at, 0)
                .unwrap();
            store.set_after_fired_at(&id, at).unwrap();
        }
    }

    store.prune_runs("a", 1, 1).unwrap();
    assert_eq!(store.recent_runs(Some("a"), 100).unwrap().len(), 1);
    assert_eq!(
        store.recent_runs(Some("b"), 100).unwrap().len(),
        5,
        "pruning one job must not touch another"
    );
}

fn finished_success(store: &Store, id: &str, at: Timestamp) {
    store
        .start_run(
            id,
            "j",
            Trigger::Schedule,
            at,
            Path::new(&format!("/tmp/{id}.out")),
            Path::new(&format!("/tmp/{id}.err")),
        )
        .unwrap();
    store
        .finish_run(id, RunStatus::Success, Some(0), at, 0)
        .unwrap();
}

#[test]
fn keep_count_prune_spares_a_success_whose_after_children_are_still_owed() {
    let store = Store::open_in_memory().unwrap();
    let base: Timestamp = "2026-06-01T00:00:00Z".parse().unwrap();
    for i in 0..5 {
        let id = format!("r{i}");
        let at = base + jiff::Span::new().minutes(i);
        finished_success(&store, &id, at);
        if i != 0 {
            store.set_after_fired_at(&id, at).unwrap();
        }
    }

    let orphaned = store.prune_runs("j", 1, 1).unwrap();

    let left: Vec<String> = store
        .recent_runs(Some("j"), 100)
        .unwrap()
        .into_iter()
        .map(|r| r.id)
        .collect();
    assert_eq!(
        left,
        vec!["r4", "r0"],
        "the oldest row is unstamped and must survive; the stamped middle rows must not"
    );
    assert_eq!(
        orphaned.len(),
        6,
        "two files per pruned row, three rows pruned"
    );

    store.set_after_fired_at("r0", base).unwrap();
    store.prune_runs("j", 1, 1).unwrap();
    assert!(
        store.get_run("r0").unwrap().is_none(),
        "once stamped, the same row prunes like any other"
    );
}

#[test]
fn age_prune_spares_a_success_whose_after_children_are_still_owed() {
    let store = Store::open_in_memory().unwrap();
    let base: Timestamp = "2026-06-01T00:00:00Z".parse().unwrap();
    finished_success(&store, "owed", base);
    finished_success(&store, "handled", base);
    store.set_after_fired_at("handled", base).unwrap();

    let cutoff = base + jiff::Span::new().hours(24 * 30);
    store.prune_older_than("j", cutoff).unwrap();

    assert!(store.get_run("owed").unwrap().is_some());
    assert!(store.get_run("handled").unwrap().is_none());
}

#[test]
fn prune_guard_applies_only_to_successes_not_to_other_unstamped_terminal_rows() {
    let store = Store::open_in_memory().unwrap();
    let base: Timestamp = "2026-06-01T00:00:00Z".parse().unwrap();
    for (i, status) in [RunStatus::Failure, RunStatus::Timeout, RunStatus::Unknown]
        .into_iter()
        .enumerate()
    {
        let id = format!("r{i}");
        let at = base + jiff::Span::new().minutes(i64::try_from(i).unwrap());
        store
            .start_run(
                &id,
                "j",
                Trigger::Schedule,
                at,
                Path::new("/tmp/o"),
                Path::new("/tmp/e"),
            )
            .unwrap();
        store.finish_run(&id, status, Some(1), at, 0).unwrap();
    }

    store.prune_runs("j", 1, 1).unwrap();
    assert_eq!(
        store.recent_runs(Some("j"), 100).unwrap().len(),
        1,
        "a failure fires nothing, so it owes nothing and prunes normally"
    );
}

#[test]
fn distinct_run_jobs_lists_every_job_with_history_sorted() {
    let store = Store::open_in_memory().unwrap();
    let t: Timestamp = "2026-06-01T00:00:00Z".parse().unwrap();
    for (id, job) in [("1", "zeta"), ("2", "alpha"), ("3", "zeta")] {
        store
            .start_run(
                id,
                job,
                Trigger::Schedule,
                t,
                Path::new("/tmp/o"),
                Path::new("/tmp/e"),
            )
            .unwrap();
    }
    assert_eq!(
        store.distinct_run_jobs().unwrap(),
        vec!["alpha".to_string(), "zeta".to_string()]
    );
}

#[test]
fn enqueue_run_is_reflected_in_queued_count() {
    let store = Store::open_in_memory().unwrap();
    let t: Timestamp = "2026-06-01T00:00:00Z".parse().unwrap();
    assert_eq!(store.queued_count("j").unwrap(), 0);

    store.enqueue_run("j", t).unwrap();
    assert_eq!(store.queued_count("j").unwrap(), 1);

    store
        .enqueue_run("j", t + jiff::Span::new().minutes(1))
        .unwrap();
    assert_eq!(store.queued_count("j").unwrap(), 2);

    assert_eq!(
        store.queued_count("other").unwrap(),
        0,
        "one job's queue must not count toward another's"
    );
}

#[test]
fn dequeue_oldest_returns_the_earliest_due_at_first_and_removes_it() {
    let store = Store::open_in_memory().unwrap();
    let t: Timestamp = "2026-06-01T00:00:00Z".parse().unwrap();
    let mid = store
        .enqueue_run("j", t + jiff::Span::new().minutes(1))
        .unwrap();
    let first = store.enqueue_run("j", t).unwrap();
    let last = store
        .enqueue_run("j", t + jiff::Span::new().minutes(2))
        .unwrap();

    let (id, due_at) = store.dequeue_oldest("j").unwrap().unwrap();
    assert_eq!(id, first);
    assert_eq!(due_at, t);
    assert_eq!(store.queued_count("j").unwrap(), 2);

    let (id, _) = store.dequeue_oldest("j").unwrap().unwrap();
    assert_eq!(id, mid);

    let (id, _) = store.dequeue_oldest("j").unwrap().unwrap();
    assert_eq!(id, last);

    assert_eq!(
        store.dequeue_oldest("j").unwrap(),
        None,
        "an empty queue must not fabricate a row"
    );
}

#[test]
fn dequeue_oldest_does_not_return_another_jobs_entry() {
    let store = Store::open_in_memory().unwrap();
    let t: Timestamp = "2026-06-01T00:00:00Z".parse().unwrap();
    store.enqueue_run("other", t).unwrap();

    assert_eq!(store.dequeue_oldest("j").unwrap(), None);
    assert_eq!(
        store.queued_count("other").unwrap(),
        1,
        "dequeuing an empty job's queue must not touch another job's entries"
    );
}
