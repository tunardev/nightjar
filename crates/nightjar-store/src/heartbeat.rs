use crate::Store;
use crate::run::{from_ms, ms};
use anyhow::Result;
use jiff::Timestamp;
use rusqlite::OptionalExtension;

#[derive(Debug, Clone)]
pub struct DaemonBeat {
    pub at: Timestamp,
    pub pid: u32,
    pub version: String,
    /// `None` on a store predating this split. Readers fall back to `at`.
    pub caught_up_through: Option<Timestamp>,
}

impl Store {
    pub fn write_heartbeat(&self, at: Timestamp, pid: u32, version: &str) -> Result<()> {
        self.conn.execute(
            "INSERT INTO daemon (id, heartbeat_at, pid, version) VALUES (1, ?1, ?2, ?3)
             ON CONFLICT(id) DO UPDATE SET heartbeat_at = ?1, pid = ?2, version = ?3",
            rusqlite::params![ms(at), pid, version],
        )?;
        Ok(())
    }

    /// Callers must have already made `at` true, not merely watched time
    /// pass.
    pub fn set_caught_up_through(&self, at: Timestamp) -> Result<()> {
        self.conn.execute(
            "INSERT INTO daemon (id, heartbeat_at, pid, version, caught_up_through)
             VALUES (1, ?1, ?2, ?3, ?1)
             ON CONFLICT(id) DO UPDATE SET caught_up_through = ?1",
            rusqlite::params![ms(at), std::process::id(), env!("CARGO_PKG_VERSION")],
        )?;
        Ok(())
    }

    pub fn daemon_heartbeat(&self) -> Result<Option<DaemonBeat>> {
        let row = self
            .conn
            .query_row(
                "SELECT heartbeat_at, pid, version, caught_up_through FROM daemon WHERE id = 1",
                [],
                |r| {
                    Ok((
                        r.get::<_, i64>(0)?,
                        r.get::<_, u32>(1)?,
                        r.get::<_, String>(2)?,
                        r.get::<_, Option<i64>>(3)?,
                    ))
                },
            )
            .optional()?;

        match row {
            None => Ok(None),
            Some((at, pid, version, caught_up)) => Ok(Some(DaemonBeat {
                at: from_ms(at)?,
                pid,
                version,
                caught_up_through: caught_up.map(from_ms).transpose()?,
            })),
        }
    }
}
