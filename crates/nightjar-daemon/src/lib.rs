mod daemon;
mod reconcile;
mod spawn;
mod tick;

pub use daemon::Daemon;
pub use spawn::{ExecSpawner, Spawner};
pub use tick::overlap_allows;

pub(crate) fn secs_i64(d: std::time::Duration) -> i64 {
    i64::try_from(d.as_secs()).expect("duration constant fits in i64 seconds")
}

/// `thread::sleep` retries on EINTR, so the signal alone won't wake it.
const STOP_POLL: std::time::Duration = std::time::Duration::from_millis(200);

static STOP_REQUESTED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

extern "C" fn stop_handler(_sig: libc::c_int) {
    STOP_REQUESTED.store(true, std::sync::atomic::Ordering::SeqCst);
}

/// Makes a deliberate stop exit 0.
///
/// launchd restarts a process killed by a signal. systemd does not.
/// Always exit 0 here, never die by signal.
pub fn install_stop_handlers() {
    unsafe {
        libc::signal(libc::SIGINT, stop_handler as *const () as usize);
        libc::signal(libc::SIGTERM, stop_handler as *const () as usize);
        // A scheduler must outlive whatever was reading its log. Without
        // this, a detached reader (a closed terminal, a rotated pipe)
        // kills the daemon on its next line.
        libc::signal(libc::SIGPIPE, libc::SIG_IGN);
    }
}

pub(crate) fn stop_requested() -> bool {
    STOP_REQUESTED.load(std::sync::atomic::Ordering::SeqCst)
}

pub(crate) fn sleep_unless_stopping(total: std::time::Duration) {
    let start = std::time::Instant::now();
    loop {
        if stop_requested() {
            return;
        }
        // Read once. Two `elapsed()` reads can straddle the deadline, and
        // a `checked_sub` on the second would panic the daemon.
        let remaining = total.saturating_sub(start.elapsed());
        if remaining.is_zero() {
            return;
        }
        std::thread::sleep(STOP_POLL.min(remaining));
    }
}

#[cfg(test)]
mod sleep_tests {
    use super::sleep_unless_stopping;
    use std::time::{Duration, Instant};

    #[test]
    fn zero_duration_returns_immediately() {
        let started = Instant::now();
        sleep_unless_stopping(Duration::ZERO);
        assert!(started.elapsed() < Duration::from_millis(100));
    }

    #[test]
    fn sleeps_for_roughly_the_requested_time() {
        let started = Instant::now();
        sleep_unless_stopping(Duration::from_millis(30));
        let elapsed = started.elapsed();
        assert!(
            elapsed >= Duration::from_millis(30),
            "slept only {elapsed:?}"
        );
        assert!(elapsed < Duration::from_secs(2), "overslept: {elapsed:?}");
    }

    #[test]
    fn tiny_durations_never_panic_when_hammered() {
        for _ in 0..200 {
            sleep_unless_stopping(Duration::from_nanos(1));
        }
    }
}
