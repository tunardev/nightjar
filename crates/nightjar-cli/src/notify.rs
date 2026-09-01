use anyhow::{Result, bail};
use nightjar_config::OnFailure;
use nightjar_core::clock::Clock;
use nightjar_core::clock::SystemClock;
use nightjar_core::paths::Paths;
pub use nightjar_runner::notify::{
    Alert, Notifier, NotifyOutcome, RealNotifier, RecordingNotifier, send_and_stamp_cooldown,
};
use nightjar_store::Store;

/// The wrapper that recorded the run may already be gone by the time this
/// runs. The `Alert` is reconstructed from arguments instead of reused.
pub fn cmd_notify(
    job: &str,
    kind: &str,
    exit_code: Option<i32>,
    on_failure: &OnFailure,
) -> Result<i32> {
    let alert = match kind {
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
    };

    let paths = Paths::resolve()?;
    let store = Store::open(&paths.db_path)?;
    // Empty. This process never receives a resolved secret. There is
    // nothing here for redaction to guard.
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
