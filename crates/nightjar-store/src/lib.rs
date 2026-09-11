pub mod heartbeat;
pub mod run;
pub mod schema;
pub mod state;
pub mod transaction;

use std::path::Path;

use anyhow::{Context, Result};
pub use heartbeat::DaemonBeat;
use rusqlite::Connection;
pub use state::{JobState, overdue_since};
pub use transaction::Transaction;

pub struct Store {
    conn: Connection,
}

const OPEN_RETRIES: u32 = 10;

impl Store {
    /// # Errors
    /// fails if the file cannot be opened, secured to 0600, or migrated
    pub fn open(db_path: &Path) -> Result<Self> {
        if let Some(parent) = db_path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating {}", parent.display()))?;
        }
        let created_here = !db_path.exists();

        let mut last_err = None;
        for attempt in 0..OPEN_RETRIES {
            if attempt > 0 {
                std::thread::sleep(std::time::Duration::from_millis(20 * u64::from(attempt)));
            }
            let conn = Connection::open(db_path)
                .with_context(|| format!("opening {}", db_path.display()))?;
            conn.busy_timeout(std::time::Duration::from_secs(5))?;
            match schema::migrate(&conn) {
                Ok(()) => {
                    if created_here {
                        make_private(db_path)?;
                    }
                    return Ok(Self { conn });
                }
                Err(e) if is_database_busy(&e) => last_err = Some(e),
                Err(e) => return Err(e),
            }
        }
        Err(last_err.unwrap_or_else(|| {
            anyhow::anyhow!("the store would not open and sqlite gave no reason")
        }))
        .with_context(|| format!("initializing schema for {}", db_path.display()))
    }

    /// # Errors
    /// fails if sqlite cannot open in memory, or the migration does not apply
    pub fn open_in_memory() -> Result<Self> {
        let conn = Connection::open_in_memory()?;
        conn.busy_timeout(std::time::Duration::from_secs(5))?;
        schema::migrate(&conn)?;
        Ok(Self { conn })
    }

    /// # Errors
    /// fails if `sqlite_master` cannot be read, or `schema_version` is not an integer
    pub fn schema_version(&self) -> Result<i64> {
        schema::version(&self.conn)
    }

    /// # Errors
    /// never fails on an open connection
    pub fn data_version(&self) -> Result<i64> {
        Ok(self
            .conn
            .pragma_query_value(None, "data_version", |r| r.get(0))?)
    }

    /// # Errors
    /// fails if no such table exists, or if `table` is not a bare identifier
    pub fn drop_table_for_testing(&self, table: &str) -> Result<()> {
        self.conn
            .execute_batch(&format!("DROP TABLE {table}"))
            .map_err(Into::into)
    }

    /// # Errors
    /// fails if no such table exists, or if `table` is not a bare identifier
    pub fn count_rows_for_testing(&self, table: &str) -> Result<i64> {
        Ok(self
            .conn
            .query_row(&format!("SELECT count(*) FROM {table}"), [], |r| r.get(0))?)
    }
}

fn make_private(db_path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(db_path, std::fs::Permissions::from_mode(0o600))
        .with_context(|| format!("restricting permissions on {}", db_path.display()))
}

pub fn is_database_busy(e: &anyhow::Error) -> bool {
    e.downcast_ref::<rusqlite::Error>()
        .and_then(rusqlite::Error::sqlite_error_code)
        .is_some_and(|c| c == rusqlite::ErrorCode::DatabaseBusy)
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt;

    use super::Store;

    fn mode_of(path: &std::path::Path) -> u32 {
        std::fs::metadata(path).unwrap().permissions().mode() & 0o777
    }

    #[test]
    fn a_database_created_by_open_is_readable_by_the_owner_alone() {
        let tmp = tempfile::tempdir().unwrap();
        let db = tmp.path().join("nightjar.db");
        Store::open(&db).unwrap();
        assert_eq!(mode_of(&db), 0o600);
    }

    #[test]
    fn an_existing_database_keeps_its_permissions() {
        let tmp = tempfile::tempdir().unwrap();
        let db = tmp.path().join("nightjar.db");
        Store::open(&db).unwrap();
        std::fs::set_permissions(&db, std::fs::Permissions::from_mode(0o644)).unwrap();

        Store::open(&db).unwrap();

        assert_eq!(mode_of(&db), 0o644, "not ours to change once it exists");
    }
}
