use anyhow::{Context, Result, bail};
use rusqlite::Connection;

pub const CURRENT_VERSION: i64 = 7;

/// One step per version, plus slack for steps another process already
/// applied. Otherwise a no-op step could spin forever.
#[allow(clippy::cast_sign_loss, clippy::cast_possible_truncation)] // CURRENT_VERSION is a small positive literal
const MAX_STEPS: usize = CURRENT_VERSION as usize + 4;

pub fn version(conn: &Connection) -> Result<i64> {
    let exists: i64 = conn.query_row(
        "SELECT count(*) FROM sqlite_master WHERE type='table' AND name='schema_version'",
        [],
        |r| r.get(0),
    )?;
    if exists == 0 {
        return Ok(0);
    }
    Ok(conn.query_row("SELECT v FROM schema_version", [], |r| r.get(0))?)
}

/// Brings `conn` up to `CURRENT_VERSION`, applying every step it hasn't
/// seen.
///
/// A fresh database runs the same steps as an upgraded one, in the same
/// order. There's exactly one definition of what version N looks like.
///
/// A version *ahead* of `CURRENT_VERSION` is left alone, not treated as
/// an error. A newer nightjar wrote this store.
pub fn migrate(conn: &Connection) -> Result<()> {
    conn.pragma_update(None, "journal_mode", "WAL")?;
    conn.pragma_update(None, "foreign_keys", "ON")?;

    for _ in 0..MAX_STEPS {
        let at = version(conn)?;
        if at >= CURRENT_VERSION {
            return Ok(());
        }
        apply(conn, at).with_context(|| format!("migrating store from schema v{at}"))?;
    }
    bail!("schema migration did not reach v{CURRENT_VERSION} after {MAX_STEPS} steps")
}

/// Applies the one step that takes a store from `from` to `from + 1`.
///
/// `BEGIN IMMEDIATE` takes the write lock before re-reading the version.
/// A racing process then waits on the busy handler instead of reaching
/// the same `ALTER TABLE`. Losing that race surfaces as "duplicate
/// column name", not a busy error. `Store::open` won't retry it.
fn apply(conn: &Connection, from: i64) -> Result<()> {
    let sql = match from {
        0 => V1,
        1 => V2,
        2 => V3,
        3 => V4,
        4 => V5,
        5 => V6,
        6 => V7,
        other => bail!("no migration step defined from schema v{other}"),
    };

    conn.execute_batch("BEGIN IMMEDIATE")?;
    let outcome = (|| -> Result<()> {
        // re-read under the write lock. Another process may have applied
        // this step since `migrate` last checked.
        if version(conn)? != from {
            return Ok(());
        }
        conn.execute_batch(sql)?;
        Ok(())
    })();

    match outcome {
        Ok(()) => {
            conn.execute_batch("COMMIT")?;
            Ok(())
        }
        Err(e) => {
            // best-effort. The error already being returned is the one
            // worth reporting.
            let _ = conn.execute_batch("ROLLBACK");
            Err(e)
        }
    }
}

/// v0 -> v1. Frozen. Every later change gets its own step, so a store
/// built today passes through the same states an older one is upgraded
/// through.
const V1: &str = r"
    -- status is authoritative for how a run ended. exit_code is just the
    -- top-level shell's exit status and can be 0 even on a timeout.
    CREATE TABLE IF NOT EXISTS runs (
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
    CREATE INDEX IF NOT EXISTS runs_job_started ON runs(job, started_at DESC);
    CREATE INDEX IF NOT EXISTS runs_status ON runs(status);

    CREATE TABLE IF NOT EXISTS job_state (
        job                  TEXT PRIMARY KEY,
        last_run_at          INTEGER,
        next_run_at          INTEGER,
        consecutive_failures INTEGER NOT NULL DEFAULT 0,
        last_notified_at     INTEGER
    );

    CREATE TABLE IF NOT EXISTS daemon (
        id           INTEGER PRIMARY KEY CHECK (id = 1),
        heartbeat_at INTEGER NOT NULL,
        pid          INTEGER NOT NULL,
        version      TEXT NOT NULL
    );

    CREATE TABLE IF NOT EXISTS schema_version (
        id INTEGER PRIMARY KEY CHECK (id = 1),
        v  INTEGER NOT NULL
    );

    INSERT OR REPLACE INTO schema_version (id, v) VALUES (1, 1);
";

/// v1 -> v2. Splits the daemon's liveness signal from its catch-up
/// watermark. Liveness is written every tick, no matter what. The
/// watermark only advances once a gap's occurrences are recorded. NULL
/// means the store predates the split. Readers fall back to
/// `heartbeat_at`.
const V2: &str = r"
    ALTER TABLE daemon ADD COLUMN caught_up_through INTEGER;

    INSERT OR REPLACE INTO schema_version (id, v) VALUES (1, 2);
";

/// v2 -> v3. `last_run_at` is only ever written on the failure path. A
/// success clears the streak via `clear_failure_count`, which has no
/// timestamp to give it. Renamed to `last_failed_at` to match what it
/// holds.
const V3: &str = r"
    ALTER TABLE job_state RENAME COLUMN last_run_at TO last_failed_at;

    INSERT OR REPLACE INTO schema_version (id, v) VALUES (1, 3);
";

/// v3 -> v4. The OVERDUE cooldown gets its own column, separate from
/// `last_notified_at`. `clear_failure_count` resets that column on every
/// success — right for the other alert, wrong for OVERDUE. NULL means
/// never alerted.
const V4: &str = r"
    ALTER TABLE job_state ADD COLUMN last_overdue_alert_at INTEGER;

    INSERT OR REPLACE INTO schema_version (id, v) VALUES (1, 4);
";

/// v4 -> v5. Where `overlap = "queue"` occurrences land when they lose
/// their spot, instead of `runs` as `missed`. Ordered by the
/// occurrence's own due time, not `now`, so the oldest drains first.
const V5: &str = r"
    CREATE TABLE IF NOT EXISTS queued_runs (
        id     TEXT PRIMARY KEY,
        job    TEXT NOT NULL,
        due_at INTEGER NOT NULL
    );
    CREATE INDEX IF NOT EXISTS queued_runs_job_due ON queued_runs(job, due_at ASC);

    INSERT OR REPLACE INTO schema_version (id, v) VALUES (1, 5);
";

/// v5 -> v6. Lives on the PARENT's run row, not a new table. The
/// invariant is per-run: a job that succeeds twice must fire its child
/// twice. NULL means not yet handled, the only state a crash mid-fire
/// can leave. Existing rows are backfilled as handled, or adding an
/// `after` child would replay the job's whole history as phantom
/// `missed` runs.
const V6: &str = r"
    ALTER TABLE runs ADD COLUMN after_fired_at INTEGER;
    UPDATE runs SET after_fired_at = COALESCE(finished_at, started_at);

    INSERT OR REPLACE INTO schema_version (id, v) VALUES (1, 6);
";

/// v6 -> v7. Lets a run's failure carry a safe-to-show reason — today,
/// only which secret failed to resolve. It doesn't overload `exit_code`,
/// which a resolution failure never sets. NULL otherwise.
const V7: &str = r"
    ALTER TABLE runs ADD COLUMN message TEXT;

    INSERT OR REPLACE INTO schema_version (id, v) VALUES (1, 7);
";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn migrate_creates_schema_and_sets_version() {
        let conn = Connection::open_in_memory().unwrap();
        migrate(&conn).unwrap();
        assert_eq!(version(&conn).unwrap(), CURRENT_VERSION);
    }

    #[test]
    fn migrate_is_idempotent() {
        let conn = Connection::open_in_memory().unwrap();
        migrate(&conn).unwrap();
        migrate(&conn).unwrap();
        migrate(&conn).unwrap();
        assert_eq!(version(&conn).unwrap(), CURRENT_VERSION);
    }

    #[test]
    fn expected_tables_exist_after_migration() {
        let conn = Connection::open_in_memory().unwrap();
        migrate(&conn).unwrap();

        for table in [
            "runs",
            "job_state",
            "daemon",
            "schema_version",
            "queued_runs",
        ] {
            let n: i64 = conn
                .query_row(
                    "SELECT count(*) FROM sqlite_master WHERE type='table' AND name=?1",
                    [table],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(n, 1, "table {table} missing");
        }
    }

    #[test]
    fn expected_indexes_exist_after_migration() {
        let conn = Connection::open_in_memory().unwrap();
        migrate(&conn).unwrap();

        for index in ["runs_job_started", "runs_status", "queued_runs_job_due"] {
            let n: i64 = conn
                .query_row(
                    "SELECT count(*) FROM sqlite_master WHERE type='index' AND name=?1",
                    [index],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(n, 1, "index {index} missing");
        }
    }

    fn columns(conn: &Connection, table: &str) -> Vec<String> {
        let mut stmt = conn
            .prepare(&format!("SELECT name FROM pragma_table_info('{table}')"))
            .unwrap();
        let rows = stmt.query_map([], |r| r.get::<_, String>(0)).unwrap();
        rows.map(|r| r.unwrap()).collect()
    }

    fn v1_store_with_data(conn: &Connection) {
        conn.execute_batch("BEGIN").unwrap();
        conn.execute_batch(V1).unwrap();
        conn.execute_batch("COMMIT").unwrap();
        conn.execute(
            "INSERT INTO daemon (id, heartbeat_at, pid, version) VALUES (1, 1000, 42, '0.1.0')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO runs (id, job, trigger, started_at, status)
             VALUES ('r1', 'backup', 'schedule', 900, 'success')",
            [],
        )
        .unwrap();
        assert_eq!(version(conn).unwrap(), 1);
    }

    #[test]
    fn real_v1_database_is_upgraded_in_place_rather_than_left_behind() {
        let conn = Connection::open_in_memory().unwrap();
        v1_store_with_data(&conn);
        assert!(
            !columns(&conn, "daemon").contains(&"caught_up_through".to_string()),
            "the fixture is not a v1 database"
        );

        migrate(&conn).unwrap();

        assert_eq!(version(&conn).unwrap(), CURRENT_VERSION);
        assert!(
            columns(&conn, "daemon").contains(&"caught_up_through".to_string()),
            "v2's column never reached an existing database"
        );
    }

    #[test]
    fn database_preserves_the_rows_already_in_it_when_upgraded_from_v1() {
        let conn = Connection::open_in_memory().unwrap();
        v1_store_with_data(&conn);

        migrate(&conn).unwrap();

        let (beat, pid): (i64, i64) = conn
            .query_row(
                "SELECT heartbeat_at, pid FROM daemon WHERE id = 1",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!((beat, pid), (1000, 42));

        let watermark: Option<i64> = conn
            .query_row(
                "SELECT caught_up_through FROM daemon WHERE id = 1",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(watermark, None);

        let job: String = conn
            .query_row("SELECT job FROM runs WHERE id = 'r1'", [], |r| r.get(0))
            .unwrap();
        assert_eq!(job, "backup");
    }

    #[test]
    fn freshly_created_store_matches_one_upgraded_from_v1() {
        let fresh = Connection::open_in_memory().unwrap();
        migrate(&fresh).unwrap();

        let upgraded = Connection::open_in_memory().unwrap();
        v1_store_with_data(&upgraded);
        migrate(&upgraded).unwrap();

        for table in [
            "runs",
            "job_state",
            "daemon",
            "schema_version",
            "queued_runs",
        ] {
            assert_eq!(
                columns(&fresh, table),
                columns(&upgraded, table),
                "table {table} differs between a fresh store and an upgraded one"
            );
        }
    }

    fn v2_store_with_data(conn: &Connection) {
        v1_store_with_data(conn);
        conn.execute_batch("BEGIN").unwrap();
        conn.execute_batch(V2).unwrap();
        conn.execute_batch("COMMIT").unwrap();
        conn.execute(
            "INSERT INTO job_state (job, last_run_at, consecutive_failures)
             VALUES ('backup', 950, 3)",
            [],
        )
        .unwrap();
        assert_eq!(version(conn).unwrap(), 2);
    }

    #[test]
    fn real_v2_database_has_last_run_at_renamed_on_upgrade() {
        let conn = Connection::open_in_memory().unwrap();
        v2_store_with_data(&conn);
        assert!(
            columns(&conn, "job_state").contains(&"last_run_at".to_string()),
            "the fixture is not a v2 database"
        );

        migrate(&conn).unwrap();

        assert_eq!(version(&conn).unwrap(), CURRENT_VERSION);
        let cols = columns(&conn, "job_state");
        assert!(
            !cols.contains(&"last_run_at".to_string()),
            "v3's rename never reached an existing database"
        );
        assert!(cols.contains(&"last_failed_at".to_string()));
    }

    #[test]
    fn database_preserves_the_value_under_its_new_name_when_upgraded_from_v2() {
        let conn = Connection::open_in_memory().unwrap();
        v2_store_with_data(&conn);

        migrate(&conn).unwrap();

        let (last_failed, failures): (i64, i64) = conn
            .query_row(
                "SELECT last_failed_at, consecutive_failures FROM job_state WHERE job = 'backup'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!((last_failed, failures), (950, 3));
    }

    fn v3_store_with_data(conn: &Connection) {
        v2_store_with_data(conn);
        conn.execute_batch("BEGIN").unwrap();
        conn.execute_batch(V3).unwrap();
        conn.execute_batch("COMMIT").unwrap();
        conn.execute(
            "UPDATE job_state SET last_notified_at = 1200 WHERE job = 'backup'",
            [],
        )
        .unwrap();
        assert_eq!(version(conn).unwrap(), 3);
    }

    #[test]
    fn real_v3_database_gains_last_overdue_alert_at_on_upgrade() {
        let conn = Connection::open_in_memory().unwrap();
        v3_store_with_data(&conn);
        assert!(
            !columns(&conn, "job_state").contains(&"last_overdue_alert_at".to_string()),
            "the fixture is not a v3 database"
        );

        migrate(&conn).unwrap();

        assert_eq!(version(&conn).unwrap(), CURRENT_VERSION);
        assert!(
            columns(&conn, "job_state").contains(&"last_overdue_alert_at".to_string()),
            "v4's new column never reached an existing database"
        );
    }

    #[test]
    fn v3_upgrade_leaves_last_overdue_alert_at_null_and_last_notified_at_untouched() {
        let conn = Connection::open_in_memory().unwrap();
        v3_store_with_data(&conn);

        migrate(&conn).unwrap();

        let (notified, overdue_alerted): (i64, Option<i64>) = conn
            .query_row(
                "SELECT last_notified_at, last_overdue_alert_at FROM job_state WHERE job = 'backup'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(notified, 1200, "v4 must not touch the existing column");
        assert_eq!(
            overdue_alerted, None,
            "a store that predates the split has no overdue-alert history of its own"
        );
    }

    fn v4_store_with_data(conn: &Connection) {
        v3_store_with_data(conn);
        conn.execute_batch("BEGIN").unwrap();
        conn.execute_batch(V4).unwrap();
        conn.execute_batch("COMMIT").unwrap();
        assert_eq!(version(conn).unwrap(), 4);
    }

    #[test]
    fn real_v4_database_gains_the_queued_runs_table_on_upgrade() {
        let conn = Connection::open_in_memory().unwrap();
        v4_store_with_data(&conn);
        let has_table: i64 = conn
            .query_row(
                "SELECT count(*) FROM sqlite_master WHERE type='table' AND name='queued_runs'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(has_table, 0, "the fixture is not a v4 database");

        migrate(&conn).unwrap();

        assert_eq!(version(&conn).unwrap(), CURRENT_VERSION);
        assert_eq!(
            columns(&conn, "queued_runs"),
            vec!["id", "job", "due_at"],
            "v5's new table never reached an existing database"
        );
    }

    fn v5_store_with_data(conn: &Connection) {
        v4_store_with_data(conn);
        conn.execute_batch("BEGIN").unwrap();
        conn.execute_batch(V5).unwrap();
        conn.execute_batch("COMMIT").unwrap();
        assert_eq!(version(conn).unwrap(), 5);
    }

    #[test]
    fn real_v5_database_gains_after_fired_at_on_upgrade() {
        let conn = Connection::open_in_memory().unwrap();
        v5_store_with_data(&conn);
        assert!(
            !columns(&conn, "runs").contains(&"after_fired_at".to_string()),
            "the fixture is not a v5 database"
        );

        migrate(&conn).unwrap();

        assert_eq!(version(&conn).unwrap(), CURRENT_VERSION);
        assert!(
            columns(&conn, "runs").contains(&"after_fired_at".to_string()),
            "v6's new column never reached an existing database"
        );
    }

    #[test]
    fn database_marks_existing_runs_already_handled_when_upgraded_from_v5() {
        let conn = Connection::open_in_memory().unwrap();
        v5_store_with_data(&conn);

        migrate(&conn).unwrap();

        let (job, fired): (String, Option<i64>) = conn
            .query_row(
                "SELECT job, after_fired_at FROM runs WHERE id = 'r1'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(job, "backup", "v6 must not disturb the rows already there");
        assert_eq!(fired, Some(900));
    }

    fn v6_store_with_data(conn: &Connection) {
        v5_store_with_data(conn);
        conn.execute_batch("BEGIN").unwrap();
        conn.execute_batch(V6).unwrap();
        conn.execute_batch("COMMIT").unwrap();
        assert_eq!(version(conn).unwrap(), 6);
    }

    #[test]
    fn real_v6_database_gains_message_on_upgrade() {
        let conn = Connection::open_in_memory().unwrap();
        v6_store_with_data(&conn);
        assert!(
            !columns(&conn, "runs").contains(&"message".to_string()),
            "the fixture is not a v6 database"
        );

        migrate(&conn).unwrap();

        assert_eq!(version(&conn).unwrap(), CURRENT_VERSION);
        assert!(
            columns(&conn, "runs").contains(&"message".to_string()),
            "v7's new column never reached an existing database"
        );
    }

    #[test]
    fn database_leaves_message_null_for_existing_rows_when_upgraded_from_v6() {
        let conn = Connection::open_in_memory().unwrap();
        v6_store_with_data(&conn);

        migrate(&conn).unwrap();

        let message: Option<String> = conn
            .query_row("SELECT message FROM runs WHERE id = 'r1'", [], |r| r.get(0))
            .unwrap();
        assert_eq!(
            message, None,
            "a run that predates the column has no message to backfill"
        );
    }

    #[test]
    fn migrate_is_a_no_op_not_a_duplicate_column_when_store_is_already_current() {
        let conn = Connection::open_in_memory().unwrap();
        v1_store_with_data(&conn);
        migrate(&conn).unwrap();
        migrate(&conn).unwrap();
        assert_eq!(version(&conn).unwrap(), CURRENT_VERSION);
    }

    #[test]
    fn store_from_a_future_version_is_left_untouched() {
        let conn = Connection::open_in_memory().unwrap();
        migrate(&conn).unwrap();
        conn.execute(
            "INSERT OR REPLACE INTO schema_version (id, v) VALUES (1, ?1)",
            [CURRENT_VERSION + 7],
        )
        .unwrap();

        migrate(&conn).unwrap();

        assert_eq!(version(&conn).unwrap(), CURRENT_VERSION + 7);
    }
}
