use anyhow::Result;

use crate::Store;

pub struct Transaction<'a> {
    store: &'a Store,
    committed: bool,
}

impl Store {
    /// # Errors
    /// fails if `BEGIN IMMEDIATE` cannot take the write lock before `busy_timeout`
    pub fn transaction(&self) -> Result<Transaction<'_>> {
        self.conn.execute_batch("BEGIN IMMEDIATE")?;
        Ok(Transaction {
            store: self,
            committed: false,
        })
    }
}

impl Transaction<'_> {
    /// # Errors
    /// fails if `COMMIT` is refused; `Drop` then still attempts the rollback
    pub fn commit(mut self) -> Result<()> {
        self.store.conn.execute_batch("COMMIT")?;
        self.committed = true;
        Ok(())
    }
}

impl Drop for Transaction<'_> {
    fn drop(&mut self) {
        if !self.committed {
            let _ = self.store.conn.execute_batch("ROLLBACK");
        }
    }
}
