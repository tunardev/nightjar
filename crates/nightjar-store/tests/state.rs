use jiff::Timestamp;
use nightjar_store::Store;

#[test]
fn next_run_round_trips_and_can_be_cleared() {
    let store = Store::open_in_memory().unwrap();
    assert!(store.job_state("backup").unwrap().is_none());

    let t: Timestamp = "2026-06-01T02:00:00Z".parse().unwrap();
    store.set_next_run("backup", Some(t)).unwrap();
    assert_eq!(
        store.job_state("backup").unwrap().unwrap().next_run_at,
        Some(t)
    );

    store.set_next_run("backup", None).unwrap();
    let s = store.job_state("backup").unwrap().unwrap();
    assert_eq!(
        s.next_run_at, None,
        "a schedule with no further occurrence clears it"
    );
}

#[test]
fn next_run_updates_rather_than_duplicating_when_set_twice() {
    let store = Store::open_in_memory().unwrap();
    let a: Timestamp = "2026-06-01T02:00:00Z".parse().unwrap();
    let b: Timestamp = "2026-06-02T02:00:00Z".parse().unwrap();
    store.set_next_run("backup", Some(a)).unwrap();
    store.set_next_run("backup", Some(b)).unwrap();

    assert_eq!(
        store.job_state("backup").unwrap().unwrap().next_run_at,
        Some(b)
    );
    assert_eq!(store.all_job_states().unwrap().len(), 1);
}

#[test]
fn all_job_states_returns_every_job_that_has_one() {
    let store = Store::open_in_memory().unwrap();
    let t: Timestamp = "2026-06-01T02:00:00Z".parse().unwrap();
    store.set_next_run("a", Some(t)).unwrap();
    store.set_next_run("b", Some(t)).unwrap();

    let mut names: Vec<String> = store
        .all_job_states()
        .unwrap()
        .into_iter()
        .map(|s| s.job)
        .collect();
    names.sort();
    assert_eq!(names, vec!["a".to_string(), "b".to_string()]);
}

#[test]
fn delete_job_state_removes_only_the_named_job() {
    let store = Store::open_in_memory().unwrap();
    let t: Timestamp = "2026-06-01T00:00:00Z".parse().unwrap();
    store.set_next_run("keep", Some(t)).unwrap();
    store.set_next_run("gone", Some(t)).unwrap();

    store.delete_job_state("gone").unwrap();

    assert!(store.job_state("gone").unwrap().is_none());
    assert!(store.job_state("keep").unwrap().is_some());
}

#[test]
fn delete_job_state_is_not_an_error_when_job_is_unknown() {
    let store = Store::open_in_memory().unwrap();
    store.delete_job_state("never-existed").unwrap();
}

#[test]
fn record_failure_and_count_starts_at_one_and_increments() {
    let store = Store::open_in_memory().unwrap();
    let t: Timestamp = "2026-06-01T00:00:00Z".parse().unwrap();

    assert_eq!(store.record_failure_and_count("j", t).unwrap(), 1);
    assert_eq!(store.record_failure_and_count("j", t).unwrap(), 2);
    assert_eq!(
        store.job_state("j").unwrap().unwrap().consecutive_failures,
        2
    );
}

#[test]
fn record_failure_and_count_is_scoped_per_job() {
    let store = Store::open_in_memory().unwrap();
    let t: Timestamp = "2026-06-01T00:00:00Z".parse().unwrap();

    store.record_failure_and_count("a", t).unwrap();
    store.record_failure_and_count("a", t).unwrap();
    store.record_failure_and_count("b", t).unwrap();

    assert_eq!(
        store.job_state("a").unwrap().unwrap().consecutive_failures,
        2
    );
    assert_eq!(
        store.job_state("b").unwrap().unwrap().consecutive_failures,
        1
    );
}

#[test]
fn clear_failure_count_resets_the_streak_and_the_cooldown() {
    let store = Store::open_in_memory().unwrap();
    let t: Timestamp = "2026-06-01T00:00:00Z".parse().unwrap();

    store.record_failure_and_count("j", t).unwrap();
    store.set_last_notified_at("j", t).unwrap();

    store.clear_failure_count("j").unwrap();

    let state = store.job_state("j").unwrap().unwrap();
    assert_eq!(state.consecutive_failures, 0);
    assert_eq!(
        state.last_notified_at, None,
        "a success must clear the cooldown too, or the next failure would stay suppressed"
    );
}

#[test]
fn clear_failure_count_is_not_an_error_when_the_job_has_no_state() {
    let store = Store::open_in_memory().unwrap();
    store.clear_failure_count("never-failed").unwrap();
}

#[test]
fn last_notified_at_is_none_until_set_then_round_trips() {
    let store = Store::open_in_memory().unwrap();
    assert_eq!(store.last_notified_at("j").unwrap(), None);

    let t: Timestamp = "2026-06-01T00:00:00Z".parse().unwrap();
    store.set_last_notified_at("j", t).unwrap();
    assert_eq!(store.last_notified_at("j").unwrap(), Some(t));

    let t2: Timestamp = "2026-06-01T01:00:00Z".parse().unwrap();
    store.set_last_notified_at("j", t2).unwrap();
    assert_eq!(
        store.last_notified_at("j").unwrap(),
        Some(t2),
        "a second write must replace, not be ignored"
    );
}

#[test]
fn last_overdue_alert_at_is_none_until_set_then_round_trips() {
    let store = Store::open_in_memory().unwrap();
    assert_eq!(store.last_overdue_alert_at("j").unwrap(), None);

    let t: Timestamp = "2026-06-01T00:00:00Z".parse().unwrap();
    store.set_last_overdue_alert_at("j", t).unwrap();
    assert_eq!(store.last_overdue_alert_at("j").unwrap(), Some(t));

    let t2: Timestamp = "2026-06-01T01:00:00Z".parse().unwrap();
    store.set_last_overdue_alert_at("j", t2).unwrap();
    assert_eq!(
        store.last_overdue_alert_at("j").unwrap(),
        Some(t2),
        "a second write must replace, not be ignored"
    );
}

#[test]
fn last_overdue_alert_at_is_independent_of_last_notified_at() {
    let store = Store::open_in_memory().unwrap();
    let t: Timestamp = "2026-06-01T00:00:00Z".parse().unwrap();
    store.set_last_overdue_alert_at("j", t).unwrap();

    store.clear_failure_count("j").unwrap();

    assert_eq!(store.last_overdue_alert_at("j").unwrap(), Some(t));
}
