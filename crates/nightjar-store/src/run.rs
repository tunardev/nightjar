use std::path::{Path, PathBuf};

use anyhow::{Result, bail};
use jiff::Timestamp;
use rusqlite::OptionalExtension;

use crate::Store;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunStatus {
    Running,
    Success,
    Failure,
    Timeout,
    Unknown,
    Missed,
    Limit,
}

impl RunStatus {
    #[must_use]
    pub const fn as_str(&self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::Success => "success",
            Self::Failure => "failure",
            Self::Timeout => "timeout",
            Self::Unknown => "unknown",
            Self::Missed => "missed",
            Self::Limit => "limit",
        }
    }

    /// # Errors
    /// fails unless `s` is one of the seven run statuses
    pub fn parse(s: &str) -> Result<Self> {
        Ok(match s {
            "running" => Self::Running,
            "success" => Self::Success,
            "failure" => Self::Failure,
            "timeout" => Self::Timeout,
            "unknown" => Self::Unknown,
            "missed" => Self::Missed,
            "limit" => Self::Limit,
            other => bail!("unknown run status {other:?}"),
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Trigger {
    Schedule,
    Manual,
    Catchup,
    After(String),
}

impl Trigger {
    #[must_use]
    pub fn to_db_string(&self) -> String {
        match self {
            Self::Schedule => "schedule".to_string(),
            Self::Manual => "manual".to_string(),
            Self::Catchup => "catchup".to_string(),
            Self::After(parent) => format!("after:{parent}"),
        }
    }

    /// # Errors
    /// fails unless `s` is schedule, manual, catchup, or `after:<parent>`
    pub fn parse(s: &str) -> Result<Self> {
        Ok(match s {
            "schedule" => Self::Schedule,
            "manual" => Self::Manual,
            "catchup" => Self::Catchup,
            other => match other.strip_prefix("after:") {
                Some(parent) if !parent.is_empty() => Self::After(parent.to_string()),
                _ => bail!("unknown trigger {other:?}"),
            },
        })
    }
}

#[derive(Debug, Clone)]
pub struct Run {
    pub id: String,
    pub job: String,
    pub trigger: Trigger,
    pub started_at: Timestamp,
    pub finished_at: Option<Timestamp>,
    pub exit_code: Option<i32>,
    pub duration_ms: Option<i64>,
    pub status: RunStatus,
    pub pid: Option<u32>,
    pub stdout_path: Option<PathBuf>,
    pub stderr_path: Option<PathBuf>,
    pub output_bytes: u64,
    pub message: Option<String>,
}

pub(crate) fn ms(t: Timestamp) -> i64 {
    t.as_millisecond()
}
pub(crate) fn from_ms(v: i64) -> Result<Timestamp> {
    Ok(Timestamp::from_millisecond(v)?)
}

fn bytes_to_sql(n: u64) -> i64 {
    i64::try_from(n).unwrap_or(i64::MAX)
}

fn bytes_from_sql(n: i64) -> u64 {
    u64::try_from(n).unwrap_or(0)
}

const AFTER_HANDLED: &str = "NOT (status = 'success' AND after_fired_at IS NULL)";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RetentionClass {
    Finished,
    NeverRan,
}

impl RetentionClass {
    const fn sql_predicate(self) -> &'static str {
        match self {
            Self::Finished => "status NOT IN ('running', 'missed')",
            Self::NeverRan => "status = 'missed'",
        }
    }
}

const RUN_COLUMNS: &str = "id, job, trigger, started_at, finished_at, exit_code,
                           duration_ms, status, pid, stdout_path, stderr_path, output_bytes,
                           message";

impl Store {
    /// # Errors
    /// fails if the insert is blocked past `busy_timeout`, or `id` is already a real run
    #[allow(clippy::needless_pass_by_value)]
    pub fn start_run(
        &self,
        id: &str,
        job: &str,
        trigger: Trigger,
        started: Timestamp,
        stdout: &Path,
        stderr: &Path,
    ) -> Result<()> {
        let affected = self.conn.execute(
            "INSERT INTO runs (id, job, trigger, started_at, status, stdout_path, stderr_path)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
             ON CONFLICT(id) DO UPDATE SET
                 job          = excluded.job,
                 trigger      = excluded.trigger,
                 started_at   = excluded.started_at,
                 status       = excluded.status,
                 stdout_path  = excluded.stdout_path,
                 stderr_path  = excluded.stderr_path,
                 finished_at  = NULL,
                 exit_code    = NULL,
                 duration_ms  = NULL,
                 pid          = NULL,
                 output_bytes = 0,
                 message      = NULL
               WHERE runs.status = 'missed'",
            rusqlite::params![
                id,
                job,
                trigger.to_db_string(),
                ms(started),
                RunStatus::Running.as_str(),
                stdout.to_string_lossy(),
                stderr.to_string_lossy()
            ],
        )?;
        if affected == 0 {
            bail!("run {id:?} already exists and is not a catch-up placeholder to take over");
        }
        Ok(())
    }

    /// # Errors
    /// fails if the insert is blocked past `busy_timeout`, or the existing id cannot be read back
    #[allow(clippy::needless_pass_by_value)]
    pub fn record_missed_run(
        &self,
        id: &str,
        job: &str,
        trigger: Trigger,
        occurrence: Timestamp,
    ) -> Result<String> {
        let affected = self.conn.execute(
            "INSERT INTO runs
                 (id, job, trigger, started_at, finished_at, duration_ms, status, output_bytes)
             SELECT ?1, ?2, ?3, ?4, ?4, 0, ?5, 0
              WHERE NOT EXISTS (SELECT 1 FROM runs WHERE job = ?2 AND started_at = ?4)",
            rusqlite::params![
                id,
                job,
                trigger.to_db_string(),
                ms(occurrence),
                RunStatus::Missed.as_str()
            ],
        )?;
        if affected == 1 {
            return Ok(id.to_string());
        }
        let existing = self.conn.query_row(
            "SELECT id FROM runs WHERE job = ?1 AND started_at = ?2 LIMIT 1",
            rusqlite::params![job, ms(occurrence)],
            |r| r.get::<_, String>(0),
        )?;
        Ok(existing)
    }

    /// # Errors
    /// fails if the update is blocked past `busy_timeout`, or no run has id `id`
    pub fn set_run_pid(&self, id: &str, pid: u32) -> Result<()> {
        let affected = self.conn.execute(
            "UPDATE runs SET pid = ?2 WHERE id = ?1",
            rusqlite::params![id, pid],
        )?;
        if affected == 0 {
            bail!("no run has id {id:?} to record a pid against");
        }
        Ok(())
    }

    /// # Errors
    /// fails if the update is blocked past `busy_timeout`, or no run has id `id`
    pub fn finish_run(
        &self,
        id: &str,
        status: RunStatus,
        exit_code: Option<i32>,
        finished: Timestamp,
        output_bytes: u64,
    ) -> Result<()> {
        let affected = self.conn.execute(
            "UPDATE runs
                SET finished_at = ?2, exit_code = ?3,
                    duration_ms = MAX(?2 - started_at, 0),
                    status = ?4, output_bytes = ?5
              WHERE id = ?1",
            rusqlite::params![
                id,
                ms(finished),
                exit_code,
                status.as_str(),
                bytes_to_sql(output_bytes)
            ],
        )?;
        if affected == 0 {
            bail!("no run has id {id:?} to finish");
        }
        Ok(())
    }

    /// # Errors
    /// fails if the update is blocked past `busy_timeout`, or no run has id `id`
    pub fn set_run_message(&self, id: &str, message: &str) -> Result<()> {
        let affected = self.conn.execute(
            "UPDATE runs SET message = ?2 WHERE id = ?1",
            rusqlite::params![id, message],
        )?;
        if affected == 0 {
            bail!("no run has id {id:?} to attach a message to");
        }
        Ok(())
    }

    /// # Errors
    /// fails if the update is blocked past `busy_timeout`
    pub fn finish_unfinished_run(
        &self,
        id: &str,
        status: RunStatus,
        exit_code: Option<i32>,
        finished: Timestamp,
        output_bytes: u64,
    ) -> Result<bool> {
        let affected = self.conn.execute(
            "UPDATE runs
                SET finished_at = ?2, exit_code = ?3,
                    duration_ms = MAX(?2 - started_at, 0),
                    status = ?4, output_bytes = ?5
              WHERE id = ?1 AND finished_at IS NULL",
            rusqlite::params![
                id,
                ms(finished),
                exit_code,
                status.as_str(),
                bytes_to_sql(output_bytes)
            ],
        )?;
        Ok(affected > 0)
    }

    /// # Errors
    /// fails if the query cannot run, or if a row does not decode
    pub fn recent_runs(&self, job: Option<&str>, limit: usize) -> Result<Vec<Run>> {
        let sql = format!(
            "SELECT {RUN_COLUMNS} FROM runs
              WHERE (?1 IS NULL OR job = ?1)
           ORDER BY started_at DESC, rowid DESC
              LIMIT ?2"
        );
        let mut stmt = self.conn.prepare(&sql)?;
        let rows = stmt.query_map(rusqlite::params![job, clamp_limit(limit)], row_to_run)?;

        let mut out = Vec::new();
        for r in rows {
            out.push(r??);
        }
        Ok(out)
    }

    /// # Errors
    /// fails if the query cannot run, or if the row does not decode
    pub fn get_run(&self, id: &str) -> Result<Option<Run>> {
        let sql = format!("SELECT {RUN_COLUMNS} FROM runs WHERE id = ?1");
        let mut stmt = self.conn.prepare(&sql)?;
        let mut rows = stmt.query_map([id], row_to_run)?;
        match rows.next() {
            Some(row) => Ok(Some(row??)),
            None => Ok(None),
        }
    }

    /// # Errors
    /// fails for the same reasons as `recent_runs`
    pub fn last_run(&self, job: &str) -> Result<Option<Run>> {
        Ok(self.recent_runs(Some(job), 1)?.into_iter().next())
    }

    /// # Errors
    /// fails if the `runs` table cannot be read
    pub fn running_count(&self, job: &str) -> Result<usize> {
        let in_flight: i64 = self.conn.query_row(
            "SELECT count(*) FROM runs WHERE job = ?1 AND status = 'running'",
            [job],
            |r| r.get(0),
        )?;
        Ok(usize::try_from(in_flight).unwrap_or(0))
    }

    /// # Errors
    /// fails if the `queued_runs` table cannot be read
    pub fn queued_count(&self, job: &str) -> Result<usize> {
        let set_aside: i64 = self.conn.query_row(
            "SELECT count(*) FROM queued_runs WHERE job = ?1",
            [job],
            |r| r.get(0),
        )?;
        Ok(usize::try_from(set_aside).unwrap_or(0))
    }

    /// # Errors
    /// fails if the insert is blocked past `busy_timeout`
    pub fn enqueue_run(&self, job: &str, due_at: Timestamp) -> Result<String> {
        let id = uuid::Uuid::now_v7().to_string();
        self.conn.execute(
            "INSERT INTO queued_runs (id, job, due_at) VALUES (?1, ?2, ?3)",
            rusqlite::params![id, job, ms(due_at)],
        )?;
        Ok(id)
    }

    /// # Errors
    /// fails if the delete is blocked past `busy_timeout`, or `due_at` does not decode
    pub fn dequeue_oldest(&self, job: &str) -> Result<Option<(String, Timestamp)>> {
        self.conn
            .query_row(
                "DELETE FROM queued_runs WHERE id = (
                     SELECT id FROM queued_runs WHERE job = ?1 ORDER BY due_at ASC LIMIT 1
                 )
                 RETURNING id, due_at",
                [job],
                |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)),
            )
            .optional()?
            .map(|(id, at)| Ok((id, from_ms(at)?)))
            .transpose()
    }

    /// # Errors
    /// fails if the update is blocked past `busy_timeout`
    pub fn set_after_fired_at(&self, run_id: &str, at: Timestamp) -> Result<()> {
        self.conn.execute(
            "UPDATE runs SET after_fired_at = ?2 WHERE id = ?1",
            rusqlite::params![run_id, ms(at)],
        )?;
        Ok(())
    }

    /// # Errors
    /// fails if the query cannot run, or if a row does not decode
    pub fn unfired_successful_runs(&self) -> Result<Vec<Run>> {
        let sql = format!(
            "SELECT {RUN_COLUMNS} FROM runs
              WHERE status = 'success' AND finished_at IS NOT NULL
                AND after_fired_at IS NULL
              ORDER BY started_at ASC"
        );
        let mut stmt = self.conn.prepare(&sql)?;
        let rows = stmt.query_map([], row_to_run)?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r??);
        }
        Ok(out)
    }

    /// # Errors
    /// fails if the query cannot run, or if a row does not decode
    pub fn running_runs(&self) -> Result<Vec<Run>> {
        let sql = format!("SELECT {RUN_COLUMNS} FROM runs WHERE status = 'running'");
        let mut stmt = self.conn.prepare(&sql)?;
        let rows = stmt.query_map([], row_to_run)?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r??);
        }
        Ok(out)
    }

    /// # Errors
    /// fails if the `runs` table cannot be read, or a `job` column is not text
    pub fn distinct_run_jobs(&self) -> Result<Vec<String>> {
        let mut stmt = self
            .conn
            .prepare("SELECT DISTINCT job FROM runs ORDER BY job")?;
        let rows = stmt.query_map([], |r| r.get::<_, String>(0))?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r?);
        }
        Ok(out)
    }

    /// # Errors
    /// fails if either delete is blocked past `busy_timeout`
    pub fn prune_runs(
        &self,
        job: &str,
        keep_runs: usize,
        keep_missed: usize,
    ) -> Result<Vec<PathBuf>> {
        let mut orphaned = self.prune_class(job, RetentionClass::Finished, keep_runs)?;
        orphaned.extend(self.prune_class(job, RetentionClass::NeverRan, keep_missed)?);
        Ok(orphaned)
    }

    /// # Errors
    /// fails if the delete is blocked past `busy_timeout`, or a capture path is not text
    pub fn prune_older_than(&self, job: &str, cutoff: Timestamp) -> Result<Vec<PathBuf>> {
        let mut stmt = self.conn.prepare(&format!(
            "DELETE FROM runs
              WHERE job = ?1 AND status != 'running' AND started_at < ?2
                AND {AFTER_HANDLED}
          RETURNING stdout_path, stderr_path"
        ))?;
        let rows = stmt.query_map(
            rusqlite::params![job, ms(cutoff)],
            |r| -> rusqlite::Result<(Option<String>, Option<String>)> {
                Ok((r.get(0)?, r.get(1)?))
            },
        )?;

        let mut orphaned = Vec::new();
        for row in rows {
            let (stdout, stderr) = row?;
            orphaned.extend(stdout.map(PathBuf::from));
            orphaned.extend(stderr.map(PathBuf::from));
        }
        Ok(orphaned)
    }

    fn prune_class(&self, job: &str, class: RetentionClass, keep: usize) -> Result<Vec<PathBuf>> {
        let in_class = class.sql_predicate();
        let sql = format!(
            "DELETE FROM runs
              WHERE job = ?1 AND {in_class} AND {AFTER_HANDLED}
                AND id NOT IN (
                    SELECT id FROM runs
                     WHERE job = ?1 AND {in_class}
                  ORDER BY started_at DESC, rowid DESC
                     LIMIT ?2
                )
          RETURNING stdout_path, stderr_path"
        );
        let mut stmt = self.conn.prepare(&sql)?;
        let rows = stmt.query_map(
            rusqlite::params![job, clamp_limit(keep)],
            |r| -> rusqlite::Result<(Option<String>, Option<String>)> {
                Ok((r.get(0)?, r.get(1)?))
            },
        )?;

        let mut orphaned = Vec::new();
        for row in rows {
            let (stdout, stderr) = row?;
            orphaned.extend(stdout.map(PathBuf::from));
            orphaned.extend(stderr.map(PathBuf::from));
        }
        Ok(orphaned)
    }
}

fn row_to_run(row: &rusqlite::Row) -> rusqlite::Result<Result<Run>> {
    let id: String = row.get("id")?;
    let job: String = row.get("job")?;
    let stored_trigger: String = row.get("trigger")?;
    let started_at_ms: i64 = row.get("started_at")?;
    let finished_at_ms: Option<i64> = row.get("finished_at")?;
    let exit_code: Option<i32> = row.get("exit_code")?;
    let duration_ms: Option<i64> = row.get("duration_ms")?;
    let stored_status: String = row.get("status")?;
    let pid: Option<u32> = row.get("pid")?;
    let stdout_path: Option<String> = row.get("stdout_path")?;
    let stderr_path: Option<String> = row.get("stderr_path")?;
    let output_bytes: i64 = row.get("output_bytes")?;
    let message: Option<String> = row.get("message")?;

    Ok((|| {
        Ok(Run {
            id,
            job,
            trigger: Trigger::parse(&stored_trigger)?,
            started_at: from_ms(started_at_ms)?,
            finished_at: finished_at_ms.map(from_ms).transpose()?,
            exit_code,
            duration_ms,
            status: RunStatus::parse(&stored_status)?,
            pid,
            stdout_path: stdout_path.map(PathBuf::from),
            stderr_path: stderr_path.map(PathBuf::from),
            output_bytes: bytes_from_sql(output_bytes),
            message,
        })
    })())
}

fn clamp_limit(limit: usize) -> i64 {
    i64::try_from(limit).unwrap_or(i64::MAX)
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::{clamp_limit, *};
    use crate::Store;

    #[test]
    fn clamp_limit_passes_through_small_values() {
        assert_eq!(clamp_limit(0), 0);
        assert_eq!(clamp_limit(50), 50);
    }

    #[test]
    fn clamp_limit_saturates_instead_of_wrapping_negative() {
        assert_eq!(clamp_limit(usize::MAX), i64::MAX);
        assert_eq!(
            clamp_limit(usize::try_from(i64::MAX).unwrap_or(usize::MAX)),
            i64::MAX
        );
    }

    fn ts(s: &str) -> Timestamp {
        s.parse().unwrap()
    }

    #[test]
    fn start_then_finish_records_full_outcome() {
        let store = Store::open_in_memory().unwrap();
        let t0 = ts("2026-08-23T02:00:00Z");

        store
            .start_run(
                "r1",
                "backup",
                Trigger::Manual,
                t0,
                Path::new("/tmp/r1.out"),
                Path::new("/tmp/r1.err"),
            )
            .unwrap();

        let running = store.last_run("backup").unwrap().unwrap();
        assert_eq!(running.status, RunStatus::Running);
        assert_eq!(running.exit_code, None);
        assert_eq!(running.pid, None, "the row exists before the process does");

        store.set_run_pid("r1", 4242).unwrap();
        assert_eq!(store.last_run("backup").unwrap().unwrap().pid, Some(4242));

        store
            .finish_run(
                "r1",
                RunStatus::Success,
                Some(0),
                t0 + jiff::Span::new().seconds(12),
                512,
            )
            .unwrap();

        let done = store.last_run("backup").unwrap().unwrap();
        assert_eq!(done.status, RunStatus::Success);
        assert_eq!(done.exit_code, Some(0));
        assert_eq!(done.duration_ms, Some(12_000));
        assert_eq!(done.output_bytes, 512);
    }

    #[test]
    fn recent_runs_are_newest_first_and_limited() {
        let store = Store::open_in_memory().unwrap();
        let t0 = ts("2026-08-23T02:00:00Z");

        for i in 0..5 {
            let id = format!("r{i}");
            let at = t0 + jiff::Span::new().minutes(i);
            store
                .start_run(
                    &id,
                    "job",
                    Trigger::Schedule,
                    at,
                    Path::new("/tmp/o"),
                    Path::new("/tmp/e"),
                )
                .unwrap();
        }

        let runs = store.recent_runs(Some("job"), 3).unwrap();
        assert_eq!(runs.len(), 3);
        assert_eq!(runs[0].id, "r4");
        assert_eq!(runs[2].id, "r2");
    }

    #[test]
    fn recent_runs_tolerates_extreme_limit_without_error() {
        let store = Store::open_in_memory().unwrap();
        let t0 = ts("2026-08-23T02:00:00Z");

        for i in 0..5 {
            let id = format!("r{i}");
            let at = t0 + jiff::Span::new().minutes(i);
            store
                .start_run(
                    &id,
                    "job",
                    Trigger::Schedule,
                    at,
                    Path::new("/tmp/o"),
                    Path::new("/tmp/e"),
                )
                .unwrap();
        }

        let runs = store.recent_runs(Some("job"), usize::MAX).unwrap();
        assert_eq!(runs.len(), 5);
        assert_eq!(runs[0].id, "r4");
        assert_eq!(runs[4].id, "r0");
    }

    #[test]
    fn recent_runs_spans_all_jobs_when_no_job_filter_is_given() {
        let store = Store::open_in_memory().unwrap();
        let t0 = ts("2026-08-23T02:00:00Z");

        store
            .start_run(
                "a",
                "one",
                Trigger::Manual,
                t0,
                Path::new("/tmp/o"),
                Path::new("/tmp/e"),
            )
            .unwrap();
        store
            .start_run(
                "b",
                "two",
                Trigger::Manual,
                t0 + jiff::Span::new().minutes(1),
                Path::new("/tmp/o"),
                Path::new("/tmp/e"),
            )
            .unwrap();

        let runs = store.recent_runs(None, 10).unwrap();
        assert_eq!(runs.len(), 2);
        assert_eq!(runs[0].job, "two");
    }

    #[test]
    fn last_run_is_none_when_job_is_unknown() {
        let store = Store::open_in_memory().unwrap();
        assert!(store.last_run("never-ran").unwrap().is_none());
    }

    #[test]
    fn get_run_distinguishes_overlapping_runs_of_one_job() {
        let store = Store::open_in_memory().unwrap();
        let t0 = ts("2026-08-23T02:00:00Z");

        store
            .start_run(
                "older",
                "backup",
                Trigger::Schedule,
                t0,
                Path::new("/tmp/older.out"),
                Path::new("/tmp/older.err"),
            )
            .unwrap();
        store
            .start_run(
                "newer",
                "backup",
                Trigger::Manual,
                t0 + jiff::Span::new().seconds(1),
                Path::new("/tmp/newer.out"),
                Path::new("/tmp/newer.err"),
            )
            .unwrap();

        store
            .finish_run(
                "older",
                RunStatus::Failure,
                Some(3),
                t0 + jiff::Span::new().seconds(9),
                10,
            )
            .unwrap();
        store
            .finish_run(
                "newer",
                RunStatus::Success,
                Some(0),
                t0 + jiff::Span::new().seconds(2),
                20,
            )
            .unwrap();

        assert_eq!(store.last_run("backup").unwrap().unwrap().id, "newer");

        let older = store.get_run("older").unwrap().unwrap();
        assert_eq!(older.exit_code, Some(3));
        assert_eq!(older.stdout_path, Some(PathBuf::from("/tmp/older.out")));

        let newer = store.get_run("newer").unwrap().unwrap();
        assert_eq!(newer.exit_code, Some(0));
        assert_eq!(newer.stdout_path, Some(PathBuf::from("/tmp/newer.out")));
    }

    #[test]
    fn every_column_of_a_run_row_lands_in_its_own_field() {
        let store = Store::open_in_memory().unwrap();
        let t0 = ts("2026-08-23T02:00:00Z");

        store
            .start_run(
                "the-id",
                "the-job",
                Trigger::After("the-parent".to_string()),
                t0,
                Path::new("/tmp/the-stdout"),
                Path::new("/tmp/the-stderr"),
            )
            .unwrap();
        store.set_run_pid("the-id", 31_337).unwrap();
        store
            .finish_run(
                "the-id",
                RunStatus::Timeout,
                Some(77),
                t0 + jiff::Span::new().seconds(5),
                4096,
            )
            .unwrap();
        store.set_run_message("the-id", "the-message").unwrap();

        let run = store.get_run("the-id").unwrap().unwrap();
        assert_eq!(run.id, "the-id");
        assert_eq!(run.job, "the-job");
        assert_eq!(run.trigger, Trigger::After("the-parent".to_string()));
        assert_eq!(run.started_at, t0);
        assert_eq!(run.finished_at, Some(t0 + jiff::Span::new().seconds(5)));
        assert_eq!(run.exit_code, Some(77));
        assert_eq!(run.duration_ms, Some(5_000));
        assert_eq!(run.status, RunStatus::Timeout);
        assert_eq!(run.pid, Some(31_337));
        assert_eq!(run.stdout_path, Some(PathBuf::from("/tmp/the-stdout")));
        assert_eq!(run.stderr_path, Some(PathBuf::from("/tmp/the-stderr")));
        assert_eq!(run.output_bytes, 4096);
        assert_eq!(run.message.as_deref(), Some("the-message"));
    }

    #[test]
    fn get_run_is_none_when_id_is_unknown() {
        let store = Store::open_in_memory().unwrap();
        assert!(store.get_run("no-such-run").unwrap().is_none());
    }

    #[test]
    fn set_run_pid_is_an_error_when_id_is_unknown() {
        let store = Store::open_in_memory().unwrap();
        assert!(store.set_run_pid("no-such-run", 1).is_err());
    }

    #[test]
    fn run_message_is_absent_until_set_then_persists() {
        let store = Store::open_in_memory().unwrap();
        let t0 = ts("2026-08-23T02:00:00Z");
        store
            .start_run(
                "r1",
                "backup",
                Trigger::Manual,
                t0,
                Path::new("/tmp/o"),
                Path::new("/tmp/e"),
            )
            .unwrap();
        assert_eq!(store.get_run("r1").unwrap().unwrap().message, None);

        store
            .finish_run("r1", RunStatus::Failure, None, t0, 0)
            .unwrap();
        store
            .set_run_message(
                "r1",
                "resolving secret \"PGPASSWORD\": resolver ended with exit 1",
            )
            .unwrap();

        let run = store.get_run("r1").unwrap().unwrap();
        assert!(run.message.unwrap().contains("PGPASSWORD"));
    }

    #[test]
    fn finish_run_names_the_run_it_could_not_find_when_id_is_unknown() {
        let store = Store::open_in_memory().unwrap();

        let err = store
            .finish_run(
                "no-such-run",
                RunStatus::Success,
                Some(0),
                ts("2026-08-23T02:00:00Z"),
                0,
            )
            .unwrap_err();

        assert!(
            format!("{err:#}").contains("no-such-run"),
            "an operator reading this at 3am must learn which run was missing, got: {err:#}"
        );
    }

    #[test]
    fn set_run_message_is_an_error_when_id_is_unknown() {
        let store = Store::open_in_memory().unwrap();
        assert!(store.set_run_message("no-such-run", "x").is_err());
    }

    #[test]
    fn backwards_clock_step_cannot_record_a_negative_duration() {
        let store = Store::open_in_memory().unwrap();
        let t0 = ts("2026-08-23T02:00:00Z");

        store
            .start_run(
                "r1",
                "backup",
                Trigger::Manual,
                t0,
                Path::new("/tmp/o"),
                Path::new("/tmp/e"),
            )
            .unwrap();
        store
            .finish_run(
                "r1",
                RunStatus::Success,
                Some(0),
                t0 - jiff::Span::new().seconds(30),
                0,
            )
            .unwrap();

        assert_eq!(
            store.last_run("backup").unwrap().unwrap().duration_ms,
            Some(0)
        );
    }

    #[test]
    fn running_count_counts_only_running_rows_for_the_named_job() {
        let store = Store::open_in_memory().unwrap();
        let t0 = ts("2026-08-23T02:00:00Z");

        store
            .start_run(
                "r1",
                "backup",
                Trigger::Schedule,
                t0,
                Path::new("/tmp/o"),
                Path::new("/tmp/e"),
            )
            .unwrap();
        store
            .start_run(
                "r2",
                "backup",
                Trigger::Schedule,
                t0,
                Path::new("/tmp/o"),
                Path::new("/tmp/e"),
            )
            .unwrap();
        store
            .start_run(
                "r3",
                "other",
                Trigger::Schedule,
                t0,
                Path::new("/tmp/o"),
                Path::new("/tmp/e"),
            )
            .unwrap();
        store
            .finish_run("r2", RunStatus::Success, Some(0), t0, 0)
            .unwrap();

        assert_eq!(store.running_count("backup").unwrap(), 1);
        assert_eq!(store.running_count("other").unwrap(), 1);
        assert_eq!(store.running_count("no-such-job").unwrap(), 0);
    }

    #[test]
    fn status_and_trigger_round_trip_through_strings() {
        for s in [
            RunStatus::Running,
            RunStatus::Success,
            RunStatus::Failure,
            RunStatus::Timeout,
            RunStatus::Unknown,
            RunStatus::Missed,
        ] {
            assert_eq!(RunStatus::parse(s.as_str()).unwrap(), s);
        }
        for t in [
            Trigger::Schedule,
            Trigger::Manual,
            Trigger::Catchup,
            Trigger::After("backup".to_string()),
        ] {
            assert_eq!(Trigger::parse(&t.to_db_string()).unwrap(), t);
        }
        assert!(RunStatus::parse("bogus").is_err());
    }

    #[test]
    fn after_trigger_is_rejected_not_parsed_as_empty_when_it_names_nothing() {
        assert!(Trigger::parse("after:").is_err());
    }
}
