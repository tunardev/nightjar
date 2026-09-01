use crate::Store;
use crate::run::{Run, from_ms, ms};
use anyhow::Result;
use jiff::Timestamp;
use rusqlite::OptionalExtension;

#[derive(Debug, Clone)]
pub struct JobState {
    pub job: String,
    pub last_failed_at: Option<Timestamp>,
    pub next_run_at: Option<Timestamp>,
    pub consecutive_failures: u32,
    pub last_notified_at: Option<Timestamp>,
}

/// Overdue means the daemon said a job would run and then it didn't. A
/// stored `next_run_at` in the past is normal for a few seconds after a
/// run, so a job that has since run isn't overdue.
///
/// The returned time is what an OVERDUE alert names as the job's
/// expected run time, so callers don't have to re-derive it.
pub fn overdue_since(
    state: Option<&JobState>,
    last: Option<&Run>,
    now: Timestamp,
) -> Option<Timestamp> {
    let next = state.and_then(|s| s.next_run_at)?;
    if next > now {
        return None;
    }
    match last {
        Some(r) if r.started_at >= next => None,
        _ => Some(next),
    }
}

impl Store {
    pub fn set_next_run(&self, job: &str, next: Option<Timestamp>) -> Result<()> {
        self.conn.execute(
            "INSERT INTO job_state (job, next_run_at) VALUES (?1, ?2)
             ON CONFLICT(job) DO UPDATE SET next_run_at = ?2",
            rusqlite::params![job, next.map(ms)],
        )?;
        Ok(())
    }

    pub fn job_state(&self, job: &str) -> Result<Option<JobState>> {
        let row = self
            .conn
            .query_row(
                "SELECT job, last_failed_at, next_run_at, consecutive_failures, last_notified_at
                   FROM job_state WHERE job = ?1",
                [job],
                job_state_row,
            )
            .optional()?;
        row.transpose()
    }

    pub fn all_job_states(&self) -> Result<Vec<JobState>> {
        let mut stmt = self.conn.prepare(
            "SELECT job, last_failed_at, next_run_at, consecutive_failures, last_notified_at
               FROM job_state",
        )?;
        let rows = stmt.query_map([], job_state_row)?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r??);
        }
        Ok(out)
    }

    /// A job deleted and recreated under the same name would otherwise
    /// read the dead job's `next_run_at`. That renders a false OVERDUE
    /// until the next tick overwrites it.
    pub fn delete_job_state(&self, job: &str) -> Result<()> {
        self.conn
            .execute("DELETE FROM job_state WHERE job = ?1", [job])?;
        Ok(())
    }

    /// `clear_failure_count`, called on success, has no timestamp to give
    /// `last_failed_at`. So only the failure path ever sets it.
    pub fn record_failure_and_count(&self, job: &str, at: Timestamp) -> Result<u32> {
        let count: i64 = self.conn.query_row(
            "INSERT INTO job_state (job, last_failed_at, consecutive_failures)
             VALUES (?1, ?2, 1)
             ON CONFLICT(job) DO UPDATE
                 SET last_failed_at = ?2, consecutive_failures = consecutive_failures + 1
             RETURNING consecutive_failures",
            rusqlite::params![job, ms(at)],
            |r| r.get(0),
        )?;
        Ok(u32::try_from(count).unwrap_or(u32::MAX))
    }

    /// A job that just recovered must alert immediately on its next
    /// failure, not stay suppressed by the old streak's
    /// `last_notified_at`.
    pub fn clear_failure_count(&self, job: &str) -> Result<()> {
        self.conn.execute(
            "UPDATE job_state SET consecutive_failures = 0, last_notified_at = NULL
              WHERE job = ?1",
            [job],
        )?;
        Ok(())
    }

    pub fn last_notified_at(&self, job: &str) -> Result<Option<Timestamp>> {
        let row: Option<Option<i64>> = self
            .conn
            .query_row(
                "SELECT last_notified_at FROM job_state WHERE job = ?1",
                [job],
                |r| r.get(0),
            )
            .optional()?;
        row.flatten().map(from_ms).transpose()
    }

    pub fn set_last_notified_at(&self, job: &str, at: Timestamp) -> Result<()> {
        self.conn.execute(
            "INSERT INTO job_state (job, last_notified_at) VALUES (?1, ?2)
             ON CONFLICT(job) DO UPDATE SET last_notified_at = ?2",
            rusqlite::params![job, ms(at)],
        )?;
        Ok(())
    }

    /// The OVERDUE cooldown, kept apart from `last_notified_at`.
    /// `clear_failure_count` nulls that column on every success — right
    /// for the discrete Failed/TimedOut alert, wrong for a flapping
    /// job's OVERDUE cooldown on every brief recovery.
    pub fn last_overdue_alert_at(&self, job: &str) -> Result<Option<Timestamp>> {
        let row: Option<Option<i64>> = self
            .conn
            .query_row(
                "SELECT last_overdue_alert_at FROM job_state WHERE job = ?1",
                [job],
                |r| r.get(0),
            )
            .optional()?;
        row.flatten().map(from_ms).transpose()
    }

    pub fn set_last_overdue_alert_at(&self, job: &str, at: Timestamp) -> Result<()> {
        self.conn.execute(
            "INSERT INTO job_state (job, last_overdue_alert_at) VALUES (?1, ?2)
             ON CONFLICT(job) DO UPDATE SET last_overdue_alert_at = ?2",
            rusqlite::params![job, ms(at)],
        )?;
        Ok(())
    }
}

fn job_state_row(row: &rusqlite::Row) -> rusqlite::Result<Result<JobState>> {
    let job: String = row.get(0)?;
    let last_failed: Option<i64> = row.get(1)?;
    let next: Option<i64> = row.get(2)?;
    let consecutive_failures: i64 = row.get(3)?;
    let last_notified: Option<i64> = row.get(4)?;
    Ok((|| {
        Ok(JobState {
            job,
            last_failed_at: last_failed.map(from_ms).transpose()?,
            next_run_at: next.map(from_ms).transpose()?,
            consecutive_failures: u32::try_from(consecutive_failures).unwrap_or(u32::MAX),
            last_notified_at: last_notified.map(from_ms).transpose()?,
        })
    })())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::run::{RunStatus, Trigger};
    use std::path::PathBuf;

    fn ts(s: &str) -> Timestamp {
        s.parse().unwrap()
    }

    fn state(next_run_at: Option<Timestamp>) -> JobState {
        JobState {
            job: "j".into(),
            last_failed_at: None,
            next_run_at,
            consecutive_failures: 0,
            last_notified_at: None,
        }
    }

    fn run_started_at(t: Timestamp) -> Run {
        Run {
            id: "r".into(),
            job: "j".into(),
            trigger: Trigger::Schedule,
            started_at: t,
            finished_at: None,
            exit_code: None,
            duration_ms: None,
            status: RunStatus::Success,
            pid: None,
            stdout_path: None as Option<PathBuf>,
            stderr_path: None,
            output_bytes: 0,
            message: None,
        }
    }

    #[test]
    fn job_is_not_overdue_when_no_job_state_exists_at_all() {
        let now = ts("2026-06-01T00:00:00Z");
        assert!(overdue_since(None, None, now).is_none());
    }

    #[test]
    fn job_state_row_is_not_overdue_when_next_run_at_is_absent() {
        let now = ts("2026-06-01T00:00:00Z");
        let s = state(None);
        assert!(overdue_since(Some(&s), None, now).is_none());
    }

    #[test]
    fn future_next_run_is_not_overdue_regardless_of_run_history() {
        let now = ts("2026-06-01T00:00:00Z");
        let future = now + jiff::Span::new().hours(1);
        let s = state(Some(future));
        assert!(overdue_since(Some(&s), None, now).is_none());

        let stale_run = run_started_at(now - jiff::Span::new().hours(24));
        assert!(overdue_since(Some(&s), Some(&stale_run), now).is_none());
    }

    #[test]
    fn past_next_run_is_overdue_when_no_run_exists_at_all() {
        let now = ts("2026-06-01T00:00:00Z");
        let past = now - jiff::Span::new().hours(2);
        let s = state(Some(past));
        assert_eq!(overdue_since(Some(&s), None, now), Some(past));
    }

    #[test]
    fn past_next_run_is_overdue_when_its_only_run_predates_it() {
        let now = ts("2026-06-01T00:00:00Z");
        let past = now - jiff::Span::new().hours(2);
        let s = state(Some(past));
        let old_run = run_started_at(past - jiff::Span::new().hours(24));
        assert_eq!(overdue_since(Some(&s), Some(&old_run), now), Some(past));
    }

    #[test]
    fn past_next_run_is_not_overdue_when_satisfied_by_a_later_run() {
        let now = ts("2026-06-01T00:00:00Z");
        let past = now - jiff::Span::new().hours(2);
        let s = state(Some(past));
        let fresh_run = run_started_at(past + jiff::Span::new().minutes(1));
        assert!(overdue_since(Some(&s), Some(&fresh_run), now).is_none());
    }

    #[test]
    fn next_run_is_already_overdue_when_equal_to_now_with_no_run() {
        let now = ts("2026-06-01T00:00:00Z");
        let s = state(Some(now));
        assert_eq!(overdue_since(Some(&s), None, now), Some(now));
    }

    #[test]
    fn run_counts_as_satisfying_it_when_starting_exactly_at_next_run_at() {
        let now = ts("2026-06-01T00:00:00Z");
        let past = now - jiff::Span::new().hours(2);
        let s = state(Some(past));
        let run = run_started_at(past);
        assert!(overdue_since(Some(&s), Some(&run), now).is_none());
    }
}
