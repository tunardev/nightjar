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
    while start.elapsed() < total {
        if stop_requested() {
            return;
        }
        std::thread::sleep(STOP_POLL.min(total.checked_sub(start.elapsed()).unwrap()));
    }
}
