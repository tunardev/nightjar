use jiff::Timestamp;
use nightjar_store::Store;
use nightjar_store::schema;
use rusqlite::Connection;

#[test]
fn store_is_upgraded_and_keeps_working_when_opening_a_v1_database_file() {
    let tmp = tempfile::tempdir().unwrap();
    let db = tmp.path().join("nightjar.db");

    {
        let conn = Connection::open(&db).unwrap();
        conn.pragma_update(None, "journal_mode", "WAL").unwrap();
        conn.execute_batch(
            r"
            BEGIN;
            CREATE TABLE runs (
                id            TEXT PRIMARY KEY,
                job           TEXT NOT NULL,
                trigger       TEXT NOT NULL,
                started_at    INTEGER NOT NULL,
                finished_at   INTEGER,
                exit_code     INTEGER,
                duration_ms   INTEGER,
                status        TEXT NOT NULL,
                pid           INTEGER,
                stdout_path   TEXT,
                stderr_path   TEXT,
                output_bytes  INTEGER NOT NULL DEFAULT 0
            );
            CREATE INDEX runs_job_started ON runs(job, started_at DESC);
            CREATE INDEX runs_status ON runs(status);
            CREATE TABLE job_state (
                job                  TEXT PRIMARY KEY,
                last_run_at          INTEGER,
                next_run_at          INTEGER,
                consecutive_failures INTEGER NOT NULL DEFAULT 0,
                last_notified_at     INTEGER
            );
            CREATE TABLE daemon (
                id           INTEGER PRIMARY KEY CHECK (id = 1),
                heartbeat_at INTEGER NOT NULL,
                pid          INTEGER NOT NULL,
                version      TEXT NOT NULL
            );
            CREATE TABLE schema_version (
                id INTEGER PRIMARY KEY CHECK (id = 1),
                v  INTEGER NOT NULL
            );
            INSERT INTO schema_version (id, v) VALUES (1, 1);
            INSERT INTO daemon (id, heartbeat_at, pid, version)
                 VALUES (1, 1780000000000, 77, '0.0.9');
            INSERT INTO runs (id, job, trigger, started_at, finished_at, status)
                 VALUES ('old', 'backup', 'schedule', 1779999000000, 1779999001000, 'success');
            COMMIT;
            ",
        )
        .unwrap();
    }

    let store = Store::open(&db).unwrap();
    assert_eq!(store.schema_version().unwrap(), schema::CURRENT_VERSION);

    let beat = store.daemon_heartbeat().unwrap().unwrap();
    assert_eq!(beat.pid, 77);
    assert_eq!(beat.version, "0.0.9");
    assert_eq!(
        beat.caught_up_through, None,
        "the store predates the split; the daemon falls back to heartbeat_at"
    );
    assert_eq!(store.last_run("backup").unwrap().unwrap().id, "old");

    let t: Timestamp = "2026-06-01T00:00:00Z".parse().unwrap();
    store.set_caught_up_through(t).unwrap();
    assert_eq!(
        store.daemon_heartbeat().unwrap().unwrap().caught_up_through,
        Some(t)
    );
}
