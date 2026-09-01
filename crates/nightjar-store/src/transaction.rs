use crate::Store;
use anyhow::Result;

/// Holds a shared `&Store`, not the connection. See `Store::transaction`.
pub struct Transaction<'a> {
    store: &'a Store,
    committed: bool,
}

impl Store {
    /// Not `Connection::transaction`. That needs `&mut self`, and would
    /// lock out the `&self` methods callers run inside the transaction.
    ///
    /// `BEGIN IMMEDIATE`, since catch-up reads before it writes. A
    /// deferred `BEGIN` pins a read snapshot, and the first write then
    /// fails `SQLITE_BUSY_SNAPSHOT` — a code `SQLite` never routes through
    /// the busy handler.
    pub fn transaction(&self) -> Result<Transaction<'_>> {
        self.conn.execute_batch("BEGIN IMMEDIATE")?;
        Ok(Transaction {
            store: self,
            committed: false,
        })
    }
}

impl Transaction<'_> {
    pub fn commit(mut self) -> Result<()> {
        self.store.conn.execute_batch("COMMIT")?;
        self.committed = true;
        Ok(())
    }
}

impl Drop for Transaction<'_> {
    fn drop(&mut self) {
        if !self.committed {
            // best-effort. Panicking in a destructor mid-unwind is worse
            // than whatever state a failed rollback leaves behind.
            let _ = self.store.conn.execute_batch("ROLLBACK");
        }
    }
}
