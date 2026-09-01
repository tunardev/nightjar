use crate::Store;
use anyhow::{Result, bail};
use jiff::Timestamp;
use rusqlite::OptionalExtension;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunStatus {
    Running,
    Success,
    Failure,
    Timeout,
    Unknown,
    Missed,
    /// Killed for breaching a `[limits]` cap.
    Limit,
}

impl RunStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            RunStatus::Running => "running",
            RunStatus::Success => "success",
            RunStatus::Failure => "failure",
            RunStatus::Timeout => "timeout",
            RunStatus::Unknown => "unknown",
            RunStatus::Missed => "missed",
            RunStatus::Limit => "limit",
        }
    }

    pub fn parse(s: &str) -> Result<Self> {
        Ok(match s {
            "running" => RunStatus::Running,
            "success" => RunStatus::Success,
            "failure" => RunStatus::Failure,
            "timeout" => RunStatus::Timeout,
            "unknown" => RunStatus::Unknown,
            "missed" => RunStatus::Missed,
            "limit" => RunStatus::Limit,
            other => bail!("unknown run status {other:?}"),
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Trigger {
    Schedule,
    Manual,
    Catchup,
    /// Carries the parent's job name, not a run id.
    After(String),
}

impl Trigger {
    pub fn to_db_string(&self) -> String {
        match self {
            Trigger::Schedule => "schedule".to_string(),
            Trigger::Manual => "manual".to_string(),
            Trigger::Catchup => "catchup".to_string(),
            Trigger::After(parent) => format!("after:{parent}"),
        }
    }

    pub fn parse(s: &str) -> Result<Self> {
        Ok(match s {
            "schedule" => Trigger::Schedule,
            "manual" => Trigger::Manual,
            "catchup" => Trigger::Catchup,
            other => match other.strip_prefix("after:") {
                Some(parent) if !parent.is_empty() => Trigger::After(parent.to_string()),
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
    /// A safe-to-show reason for a terminal status. Today, only which
    /// secret failed to resolve. Never the resolver's own stderr — it
    /// usually contains the secret.
    pub message: Option<String>,
}

pub(crate) fn ms(t: Timestamp) -> i64 {
    t.as_millisecond()
}
pub(crate) fn from_ms(v: i64) -> Result<Timestamp> {
    Ok(Timestamp::from_millisecond(v)?)
}

/// A row retention may delete: any that isn't a success still waiting for
/// `Daemon::fire_after_triggers` to stamp it. See `prune_runs`.
const AFTER_HANDLED: &str = "NOT (status = 'success' AND after_fired_at IS NULL)";

/// Every column of `runs`, in the order `row_to_run` reads them.
const RUN_COLUMNS: &str = "id, job, trigger, started_at, finished_at, exit_code,
                           duration_ms, status, pid, stdout_path, stderr_path, output_bytes,
                           message";

impl Store {
    /// Called before the process spawns. `set_run_pid` fills in the pid
    /// once the process exists.
    ///
    /// An id already held by a `missed` row is superseded, not rejected.
    /// Catch-up writes placeholders before spawning, and the make-up run
    /// reuses that id. Any other id collision is still an error.
    // by value: callers already own a `Trigger`. `After`'s owned `String`
    // costs less than borrowing would save.
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
                 output_bytes = 0
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
            bail!("start_run: run {id:?} already exists and is not a catch-up placeholder");
        }
        Ok(())
    }

    /// Records one never-run occurrence. Returns the id that now owns it:
    /// `id`, or whichever id already held it.
    ///
    /// The caller mints a fresh id each call. A daemon killed mid-gap
    /// could otherwise create a second row for the same occurrence and
    /// double-count it.
    ///
    /// `stdout_path` and `stderr_path` stay NULL. No process ran, so
    /// there's nothing to unlink.
    #[allow(clippy::needless_pass_by_value)] // see start_run's own note
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

    /// The wrapper's pid, not the job's own shell's. The wrapper outlives
    /// the shell on purpose (`runner::exec::execute`).
    pub fn set_run_pid(&self, id: &str, pid: u32) -> Result<()> {
        let affected = self.conn.execute(
            "UPDATE runs SET pid = ?2 WHERE id = ?1",
            rusqlite::params![id, pid],
        )?;
        if affected == 0 {
            bail!("set_run_pid: no run found with id {id:?}");
        }
        Ok(())
    }

    pub fn finish_run(
        &self,
        id: &str,
        status: RunStatus,
        exit_code: Option<i32>,
        finished: Timestamp,
        output_bytes: u64,
    ) -> Result<()> {
        let started: i64 =
            self.conn
                .query_row("SELECT started_at FROM runs WHERE id = ?1", [id], |r| {
                    r.get(0)
                })?;
        // wall clock can step backwards mid-run (NTP on a resumed laptop).
        // A negative duration would render as "-0.5s".
        let duration = (ms(finished) - started).max(0);

        let affected = self.conn.execute(
            "UPDATE runs
                SET finished_at = ?2, exit_code = ?3, duration_ms = ?4,
                    status = ?5, output_bytes = ?6
              WHERE id = ?1",
            rusqlite::params![
                id,
                ms(finished),
                exit_code,
                duration,
                status.as_str(),
                output_bytes
            ],
        )?;
        if affected == 0 {
            bail!("finish_run: no run found with id {id:?}");
        }
        Ok(())
    }

    /// Only the secret-resolution failure path calls this.
    pub fn set_run_message(&self, id: &str, message: &str) -> Result<()> {
        let affected = self.conn.execute(
            "UPDATE runs SET message = ?2 WHERE id = ?1",
            rusqlite::params![id, message],
        )?;
        if affected == 0 {
            bail!("set_run_message: no run found with id {id:?}");
        }
        Ok(())
    }

    /// Finishes a run if it is not already terminal. Returns whether this
    /// call won that race. A missing or already-finished row returns
    /// `Ok(false)`, not an error, because callers here are speculative.
    pub fn finish_unfinished_run(
        &self,
        id: &str,
        status: RunStatus,
        exit_code: Option<i32>,
        finished: Timestamp,
        output_bytes: u64,
    ) -> Result<bool> {
        // computed in the same UPDATE, not a prior SELECT, to avoid a race
        // with another writer. Same clock-step clamp as `finish_run`.
        let affected = self.conn.execute(
            "UPDATE runs
                SET finished_at = ?2, exit_code = ?3,
                    duration_ms = MAX(?2 - started_at, 0),
                    status = ?4, output_bytes = ?5
              WHERE id = ?1 AND finished_at IS NULL",
            rusqlite::params![id, ms(finished), exit_code, status.as_str(), output_bytes],
        )?;
        Ok(affected > 0)
    }

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

    /// By id, not `last_run`'s "newest by `started_at`". Two runs of one
    /// job can overlap, so newest-by-time isn't identity.
    pub fn get_run(&self, id: &str) -> Result<Option<Run>> {
        let sql = format!("SELECT {RUN_COLUMNS} FROM runs WHERE id = ?1");
        let mut stmt = self.conn.prepare(&sql)?;
        let mut rows = stmt.query_map([id], row_to_run)?;
        match rows.next() {
            Some(row) => Ok(Some(row??)),
            None => Ok(None),
        }
    }

    pub fn last_run(&self, job: &str) -> Result<Option<Run>> {
        Ok(self.recent_runs(Some(job), 1)?.into_iter().next())
    }

    pub fn running_count(&self, job: &str) -> Result<usize> {
        let n: i64 = self.conn.query_row(
            "SELECT count(*) FROM runs WHERE job = ?1 AND status = 'running'",
            [job],
            |r| r.get(0),
        )?;
        Ok(usize::try_from(n).unwrap_or(0))
    }

    /// How many of `job`'s occurrences `overlap = "queue"` has set aside.
    /// Checked against `queue_depth` before enqueuing another.
    pub fn queued_count(&self, job: &str) -> Result<usize> {
        let n: i64 = self.conn.query_row(
            "SELECT count(*) FROM queued_runs WHERE job = ?1",
            [job],
            |r| r.get(0),
        )?;
        Ok(usize::try_from(n).unwrap_or(0))
    }

    /// Sets aside one occurrence of `job` for later. `overlap = "queue"`'s
    /// alternative to recording it `missed`. The returned id becomes the
    /// eventual run's id once dequeued.
    pub fn enqueue_run(&self, job: &str, due_at: Timestamp) -> Result<String> {
        let id = uuid::Uuid::now_v7().to_string();
        self.conn.execute(
            "INSERT INTO queued_runs (id, job, due_at) VALUES (?1, ?2, ?3)",
            rusqlite::params![id, job, ms(due_at)],
        )?;
        Ok(id)
    }

    /// Pops `job`'s oldest queued occurrence. One statement, so a
    /// concurrent reader can never see a row counted by both
    /// `queued_count` and an in-progress dequeue.
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

    /// Marks `run_id`'s `after` children handled. No later pass fires them
    /// twice. Stamped whether or not the run had children — both cases
    /// mean nothing further is owed.
    pub fn set_after_fired_at(&self, run_id: &str, at: Timestamp) -> Result<()> {
        self.conn.execute(
            "UPDATE runs SET after_fired_at = ?2 WHERE id = ?1",
            rusqlite::params![run_id, ms(at)],
        )?;
        Ok(())
    }

    /// Every successful run whose `after` children were never handled.
    /// What a daemon that died between a parent succeeding and its child
    /// starting leaves behind. A still-running parent is excluded, since
    /// it owes nothing yet.
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

    /// `Daemon::reconcile` reads this every tick to find rows nobody will
    /// ever finish — a crashed daemon's, a `SIGKILLed` wrapper's.
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

    /// Retention reads this instead of the jobs directory. A deleted
    /// job's run history and output files still need pruning.
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

    /// Prunes `job`'s finished history. Returns the output paths of every
    /// deleted row, for the caller to unlink.
    ///
    /// `missed` rows carry the occurrence's own time, not a real run
    /// time, so they get their own keep-count instead of sharing
    /// `keep_runs`.
    ///
    /// `running` rows are never counted toward a keep-count or deleted.
    /// An in-flight run has no defensible age against finished ones.
    ///
    /// A successful run whose `after` children have not been handled yet
    /// (`after_fired_at IS NULL`) is never deleted either: the daemon
    /// reads it on its next pass to fire or record those children, and a
    /// prune in between would silently lose the trigger.
    pub fn prune_runs(
        &self,
        job: &str,
        keep_runs: usize,
        keep_missed: usize,
    ) -> Result<Vec<PathBuf>> {
        let mut orphaned =
            self.prune_class(job, "status NOT IN ('running', 'missed')", keep_runs)?;
        orphaned.extend(self.prune_class(job, "status = 'missed'", keep_missed)?);
        Ok(orphaned)
    }

    /// Deletes `job`'s finished rows older than `cutoff`. Returns the
    /// output paths of every row removed, for the caller to unlink.
    ///
    /// Separate from `prune_runs`: a keep-count bounds history size, an
    /// age bounds how long output sits on disk, and a job can breach
    /// either without the other.
    ///
    /// `running` rows are excluded, same as `prune_class`: unfinished,
    /// not old. So are successful rows still owed an `after` pass; see
    /// `prune_runs`.
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

    /// One keep-count applied to one class of `job`'s rows. The inner
    /// `SELECT` and outer `DELETE` run as one statement, so the set to
    /// keep is fixed before anything is removed.
    ///
    /// `class` is interpolated into SQL. It must only ever be a literal
    /// from `prune_runs` — never anything derived from a job name or a
    /// row.
    fn prune_class(&self, job: &str, class: &str, keep: usize) -> Result<Vec<PathBuf>> {
        let sql = format!(
            "DELETE FROM runs
              WHERE job = ?1 AND {class} AND {AFTER_HANDLED}
                AND id NOT IN (
                    SELECT id FROM runs
                     WHERE job = ?1 AND {class}
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
    let finished: Option<i64> = row.get(4)?;
    let status_s: String = row.get(7)?;
    let trigger_s: String = row.get(2)?;
    let started: i64 = row.get(3)?;
    let stdout: Option<String> = row.get(9)?;
    let stderr: Option<String> = row.get(10)?;
    let bytes: u64 = row.get(11)?;
    let pid: Option<u32> = row.get(8)?;
    let message: Option<String> = row.get(12)?;

    Ok((|| {
        Ok(Run {
            id: row.get(0)?,
            job: row.get(1)?,
            trigger: Trigger::parse(&trigger_s)?,
            started_at: from_ms(started)?,
            finished_at: finished.map(from_ms).transpose()?,
            exit_code: row.get(5)?,
            duration_ms: row.get(6)?,
            status: RunStatus::parse(&status_s)?,
            pid,
            stdout_path: stdout.map(PathBuf::from),
            stderr_path: stderr.map(PathBuf::from),
            output_bytes: bytes,
            message,
        })
    })())
}

// `as i64` truncates usize::MAX to -1. SQLite reads a negative LIMIT as
// no limit, silently disabling the row cap instead of erroring.
fn clamp_limit(limit: usize) -> i64 {
    i64::try_from(limit).unwrap_or(i64::MAX)
}

#[cfg(test)]
mod limit_tests {
    use super::clamp_limit;

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
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Store;
    use std::path::Path;

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
                "resolving secret \"PGPASSWORD\": resolver exited with 1",
            )
            .unwrap();

        let run = store.get_run("r1").unwrap().unwrap();
        assert!(run.message.unwrap().contains("PGPASSWORD"));
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
