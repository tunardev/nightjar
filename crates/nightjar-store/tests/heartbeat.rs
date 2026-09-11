use jiff::Timestamp;
use nightjar_store::Store;

#[test]
fn heartbeat_is_absent_until_written_then_reflects_the_latest_write() {
    let store = Store::open_in_memory().unwrap();
    assert!(store.daemon_heartbeat().unwrap().is_none());

    let t1: Timestamp = "2026-06-01T00:00:00Z".parse().unwrap();
    store.write_heartbeat(t1, 4242, "0.1.0").unwrap();
    let b = store.daemon_heartbeat().unwrap().unwrap();
    assert_eq!(b.at, t1);
    assert_eq!(b.pid, 4242);
    assert_eq!(b.version, "0.1.0");

    let t2: Timestamp = "2026-06-01T00:00:30Z".parse().unwrap();
    store.write_heartbeat(t2, 4243, "0.1.0").unwrap();
    let b = store.daemon_heartbeat().unwrap().unwrap();
    assert_eq!(b.at, t2, "second write must replace, not append");
    assert_eq!(b.pid, 4243);
}

#[test]
fn watermark_is_absent_until_written_and_never_moved_by_a_heartbeat() {
    let store = Store::open_in_memory().unwrap();
    let t1: Timestamp = "2026-06-01T00:00:00Z".parse().unwrap();
    store.write_heartbeat(t1, 1, "0.1.0").unwrap();
    assert_eq!(
        store.daemon_heartbeat().unwrap().unwrap().caught_up_through,
        None,
        "a heartbeat says the daemon is alive, not that anything is caught up"
    );

    store.set_caught_up_through(t1).unwrap();
    let t2: Timestamp = "2026-06-01T00:00:30Z".parse().unwrap();
    store.write_heartbeat(t2, 1, "0.1.0").unwrap();

    let b = store.daemon_heartbeat().unwrap().unwrap();
    assert_eq!(b.at, t2, "liveness advanced");
    assert_eq!(
        b.caught_up_through,
        Some(t1),
        "a later heartbeat must not drag the watermark along with it"
    );
}

#[test]
fn watermark_can_be_written_when_no_heartbeat_exists_yet() {
    let store = Store::open_in_memory().unwrap();
    let t: Timestamp = "2026-06-01T00:00:00Z".parse().unwrap();
    store.set_caught_up_through(t).unwrap();
    assert_eq!(
        store.daemon_heartbeat().unwrap().unwrap().caught_up_through,
        Some(t)
    );
}

#[test]
fn heartbeat_table_does_not_hold_more_than_one_row() {
    let store = Store::open_in_memory().unwrap();
    for i in 0..5 {
        let t = Timestamp::from_second(1_800_000_000 + i).unwrap();
        store.write_heartbeat(t, 1, "0.1.0").unwrap();
    }
    let n = store.count_rows_for_testing("daemon").unwrap();
    assert_eq!(n, 1);
}
