use anyhow::{Result, bail};
use nightjar_config::OnFailure;
use nightjar_core::clock::{Clock, SystemClock};
use nightjar_core::paths::Paths;
pub use nightjar_runner::notify::{
    Alert, Notifier, NotifyOutcome, RealNotifier, RecordingNotifier, send_and_stamp_cooldown,
};
use nightjar_store::Store;

fn alert_for(job: &str, kind: &str, exit_code: Option<i32>) -> Result<Alert> {
    Ok(match kind {
        "failed" => Alert::Failed {
            job: job.to_string(),
            exit_code,
        },
        "timed_out" => Alert::TimedOut {
            job: job.to_string(),
        },
        "limit_exceeded" => Alert::LimitExceeded {
            job: job.to_string(),
        },
        other => bail!("unknown alert kind: {other:?}"),
    })
}

/// # Errors
/// fails if `kind` is not a known alert, or the store cannot be opened
pub fn cmd_notify(
    job: &str,
    kind: &str,
    exit_code: Option<i32>,
    on_failure: &OnFailure,
) -> Result<i32> {
    let alert = alert_for(job, kind, exit_code)?;

    let paths = Paths::resolve()?;
    let store = Store::open(&paths.db_path)?;
    send_and_stamp_cooldown(
        &alert,
        on_failure,
        &store,
        SystemClock.now(),
        &RealNotifier,
        &[],
    );
    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_kind_the_runner_emits_maps_to_an_alert_that_names_the_job() {
        let cases = [
            (
                "failed",
                Alert::Failed {
                    job: "backup".to_string(),
                    exit_code: Some(3),
                },
            ),
            (
                "timed_out",
                Alert::TimedOut {
                    job: "backup".to_string(),
                },
            ),
            (
                "limit_exceeded",
                Alert::LimitExceeded {
                    job: "backup".to_string(),
                },
            ),
        ];
        for (kind, expected) in cases {
            let alert = alert_for("backup", kind, Some(3)).unwrap();
            assert_eq!(
                format!("{alert:?}"),
                format!("{expected:?}"),
                "kind {kind:?} mapped to the wrong alert"
            );
        }
    }

    #[test]
    fn a_failure_with_no_exit_code_is_still_an_alert_not_an_error() {
        let alert = alert_for("backup", "failed", None).unwrap();
        assert!(matches!(
            alert,
            Alert::Failed {
                exit_code: None,
                ..
            }
        ));
    }

    #[test]
    fn an_unknown_kind_is_refused_and_the_refusal_quotes_it_back() {
        let e = alert_for("backup", "exploded", None).unwrap_err();
        assert!(format!("{e:#}").contains("\"exploded\""), "got: {e:#}");
    }
}
