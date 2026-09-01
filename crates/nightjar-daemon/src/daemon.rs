use crate::spawn::{ExecSpawner, SpawnedExec, Spawner};
use anyhow::{Context, Result, bail};
use jiff::{Timestamp, tz::TimeZone};
use nightjar_config::Config;
use nightjar_core::clock::Clock;
use nightjar_core::paths::Paths;
use nightjar_runner::notify::{Notifier, RealNotifier};
use nightjar_store::Store;
use std::collections::{HashMap, HashSet};
use std::fs::File;
use std::io;
use std::os::unix::io::AsRawFd;
use std::sync::{Arc, Mutex};

pub struct Daemon {
    pub(crate) paths: Paths,
    pub(crate) clock: Arc<dyn Clock>,
    pub(crate) tz: TimeZone,
    /// Absent means "not yet computed".
    pub(crate) next_run_at: HashMap<String, Timestamp>,
    pub(crate) store: Store,
    pub(crate) spawner: Arc<dyn Spawner>,
    pub(crate) notifier: Arc<dyn Notifier>,
    /// Jobs with a live overdue-alert dispatch thread. Caps dispatch
    /// threads at one per job. This is a backstop in case a send outlives
    /// `ALERT_COOLDOWN` itself.
    pub(crate) overdue_dispatch_in_flight: Arc<Mutex<HashSet<String>>>,
    /// When an overdue alert was last attempted for a job, success or not.
    /// The store only remembers a successful send. Without this, a down
    /// channel would retry every tick instead of backing off to
    /// `ALERT_COOLDOWN`. This field is in-memory: a restart re-alerts once.
    pub(crate) overdue_last_attempt: HashMap<String, Timestamp>,
    pub(crate) children: Vec<SpawnedExec>,
    pub(crate) consecutive_failures: u32,
    pub(crate) last_sweep: Option<Timestamp>,
    /// Read only by `sweep_due`. Its own field, not a synthetic baseline for
    /// `last_sweep`, so the first sweep can use `RETENTION_STARTUP_DEFER`
    /// and later ones use `RETENTION_SWEEP`.
    pub(crate) started_at: Timestamp,
    /// Whether this process has watched an interval go by itself: a tick
    /// that found no gap, or one whose catch-up committed. Until then, any
    /// elapsed time is unaccounted for. See `ordinary_tick_ceiling`.
    pub(crate) has_watched_the_clock: bool,
    _lock: DaemonLock,
    pub(crate) config: Config,
}

impl Daemon {
    pub fn new(paths: Paths, clock: Arc<dyn Clock>) -> Result<Daemon> {
        Daemon::with_spawner(paths, clock, Arc::new(ExecSpawner))
    }

    pub fn with_notifier(
        paths: Paths,
        clock: Arc<dyn Clock>,
        notifier: Arc<dyn Notifier>,
    ) -> Result<Daemon> {
        Daemon::with_spawner_and_notifier(paths, clock, Arc::new(ExecSpawner), notifier)
    }

    pub fn with_spawner(
        paths: Paths,
        clock: Arc<dyn Clock>,
        spawner: Arc<dyn Spawner>,
    ) -> Result<Daemon> {
        Daemon::with_spawner_and_notifier(paths, clock, spawner, Arc::new(RealNotifier))
    }

    /// Tests can't use `TZ` for this: `std::env::set_var` is unsound with
    /// concurrent test threads and leaks into other tests in the binary.
    pub fn with_spawner_and_tz(
        paths: Paths,
        clock: Arc<dyn Clock>,
        spawner: Arc<dyn Spawner>,
        tz: TimeZone,
    ) -> Result<Daemon> {
        Daemon::build(paths, clock, spawner, Arc::new(RealNotifier), tz)
    }

    pub fn with_spawner_and_notifier(
        paths: Paths,
        clock: Arc<dyn Clock>,
        spawner: Arc<dyn Spawner>,
        notifier: Arc<dyn Notifier>,
    ) -> Result<Daemon> {
        Daemon::build(paths, clock, spawner, notifier, TimeZone::system())
    }

    fn build(
        paths: Paths,
        clock: Arc<dyn Clock>,
        spawner: Arc<dyn Spawner>,
        notifier: Arc<dyn Notifier>,
        tz: TimeZone,
    ) -> Result<Daemon> {
        paths.ensure_dirs()?;
        // Before the lock or the store. A malformed config must fail here,
        // not fall back to defaults silently.
        let config = Config::load(&paths)?;
        // Before the store is opened. A second daemon must back off before
        // it can race the first on anything, including the store.
        let lock = acquire_lock(&paths.lock_path)?;
        let store = Store::open(&paths.db_path)?;
        eprintln!("nightjar: daemon started (pid {})", std::process::id());
        // `TimeZone::system()` silently falls back to UTC if it can't
        // resolve a zone (no tzdata, a broken /etc/localtime). Printing it
        // gives a 2am backup that runs at 9pm a visible cause.
        eprintln!(
            "nightjar: scheduling in {}",
            tz.iana_name().unwrap_or("an unnamed fixed-offset zone")
        );
        let startup = clock.now();
        let daemon = Daemon {
            clock,
            tz,
            next_run_at: HashMap::new(),
            store,
            spawner,
            notifier,
            overdue_dispatch_in_flight: Arc::new(Mutex::new(HashSet::new())),
            overdue_last_attempt: HashMap::new(),
            children: Vec::new(),
            consecutive_failures: 0,
            last_sweep: None,
            started_at: startup,
            has_watched_the_clock: false,
            _lock: lock,
            config,
            paths,
        };

        // A head start, not the only pass. `status` stays honest about a
        // previous daemon's rows even if the first tick is slow or fails.
        daemon.reconcile()?;

        // No catch-up here. Closing a laptop's lid suspends this process,
        // it doesn't restart it. `tick`'s own per-pass comparison already
        // covers both cases.
        Ok(daemon)
    }
}

/// Takes an exclusive, non-blocking advisory lock on `lock_path`, so a
/// second `nightjar daemon` refuses to start. Without it, two daemons could
/// both see `running_count() == 0` and both call `start_run`.
///
/// The lock, not the file's existence, is the liveness signal. Nothing
/// holds the flock once every descriptor on it closes, so a stale file can
/// never block a fresh daemon.
fn acquire_lock(lock_path: &std::path::Path) -> Result<DaemonLock> {
    let file = std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(false)
        .open(lock_path)
        .with_context(|| format!("opening lock file {}", lock_path.display()))?;

    // SAFETY: `file` stays open for the whole call. Its fd is passed to
    // `flock` exactly as libc expects.
    let rc = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    if rc != 0 {
        let err = io::Error::last_os_error();
        if err.kind() == io::ErrorKind::WouldBlock {
            bail!(
                "another nightjar daemon is already running (lock held at {})",
                lock_path.display()
            );
        }
        return Err(err).with_context(|| format!("locking {}", lock_path.display()));
    }
    Ok(DaemonLock(file))
}

/// Released on `Drop` via `LOCK_UN`, not by letting the fd close. A forked
/// child shares the same open file description until it execs, so closing
/// the fd alone would leave the lock held through that fork/exec window.
struct DaemonLock(File);

impl Drop for DaemonLock {
    fn drop(&mut self) {
        // SAFETY: `self.0` owns the fd until it drops, right after this call.
        unsafe { libc::flock(self.0.as_raw_fd(), libc::LOCK_UN) };
    }
}
