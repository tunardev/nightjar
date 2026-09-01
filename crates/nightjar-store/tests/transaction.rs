use jiff::Timestamp;
use nightjar_store::Store;
use nightjar_store::run::{RunStatus, Trigger};
use std::path::Path;

#[test]
fn transaction_rolls_back_and_the_store_still_works_when_dropped_without_commit() {
    let tmp = tempfile::tempdir().unwrap();
    let db_path = tmp.path().join("test.db");
    let store = Store::open(&db_path).unwrap();
    let t: Timestamp = "2026-06-01T00:00:00Z".parse().unwrap();

    {
        let txn = store.transaction().unwrap();
        store
            .start_run(
                "dropped",
                "job",
                Trigger::Catchup,
                t,
                Path::new("/tmp/o"),
                Path::new("/tmp/e"),
            )
            .unwrap();
        store
            .finish_run("dropped", RunStatus::Missed, None, t, 0)
            .unwrap();
        drop(txn);
    }

    store
        .start_run(
            "after",
            "job",
            Trigger::Catchup,
            t,
            Path::new("/tmp/o2"),
            Path::new("/tmp/e2"),
        )
        .unwrap();

    let independent = Store::open(&db_path).unwrap();
    assert!(
        independent.get_run("dropped").unwrap().is_none(),
        "a transaction dropped without commit must leave no trace"
    );
    assert!(
        independent.get_run("after").unwrap().is_some(),
        "write after a rolled-back transaction must persist"
    );
}

#[test]
fn transaction_survives_another_connection_committing_between_its_read_and_its_write() {
    let tmp = tempfile::tempdir().unwrap();
    let db = tmp.path().join("nightjar.db");
    let ours = Store::open(&db).unwrap();
    let theirs = Store::open(&db).unwrap();
    let t: Timestamp = "2026-06-01T00:00:00Z".parse().unwrap();

    let txn = ours.transaction().unwrap();
    assert_eq!(
        ours.running_count("j").unwrap(),
        0,
        "the read that opens catch-up's transaction"
    );

    let writer = std::thread::spawn(move || theirs.write_heartbeat(t, 1, "0.1.0"));
    std::thread::sleep(std::time::Duration::from_millis(200));

    ours.record_missed_run("m1", "j", Trigger::Catchup, t)
        .expect("the first write of a transaction that began with a read");
    txn.commit().expect("committing after a concurrent writer");

    writer
        .join()
        .unwrap()
        .expect("the other connection's write must land, not be starved");
    assert!(ours.get_run("m1").unwrap().is_some());
}

#[test]
fn committed_transaction_persists_writes() {
    let tmp = tempfile::tempdir().unwrap();
    let db_path = tmp.path().join("test.db");
    let store = Store::open(&db_path).unwrap();
    let t: Timestamp = "2026-06-01T00:00:00Z".parse().unwrap();

    let txn = store.transaction().unwrap();
    store
        .start_run(
            "committed",
            "job",
            Trigger::Catchup,
            t,
            Path::new("/tmp/o"),
            Path::new("/tmp/e"),
        )
        .unwrap();
    txn.commit().unwrap();

    let independent = Store::open(&db_path).unwrap();
    assert!(independent.get_run("committed").unwrap().is_some());
}
