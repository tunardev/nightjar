use std::collections::{HashMap, HashSet};
use std::fs::File;
use std::io;
use std::os::unix::io::AsRawFd;
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result, bail};
use jiff::Timestamp;
use jiff::tz::TimeZone;
use nightjar_config::Config;
use nightjar_core::clock::Clock;
use nightjar_core::paths::Paths;
use nightjar_runner::notify::{Notifier, RealNotifier};
use nightjar_store::Store;

use crate::spawn::{ExecSpawner, SpawnedExec, Spawner};

#[derive(Default)]
pub struct JobMemory {
    pub(crate) armed_for: Option<Timestamp>,
    pub(crate) overdue_alert_attempted_at: Option<Timestamp>,
    pub(crate) accounted_through: Option<Timestamp>,
}

pub struct Daemon {
    pub(crate) paths: Paths,
    pub(crate) clock: Arc<dyn Clock>,
    pub(crate) tz: TimeZone,
    pub(crate) job_memory: HashMap<String, JobMemory>,
    pub(crate) store: Store,
    pub(crate) spawner: Arc<dyn Spawner>,
    pub(crate) notifier: Arc<dyn Notifier>,
    pub(crate) overdue_dispatch_in_flight: Arc<Mutex<HashSet<String>>>,
    pub(crate) children: Vec<SpawnedExec>,
    pub(crate) consecutive_failures: u32,
    pub(crate) logged_refusals: HashSet<String>,
    pub(crate) last_sweep: Option<Timestamp>,
    pub(crate) started_at: Timestamp,
    pub(crate) has_watched_the_clock: bool,
    pub(crate) logged_warnings: HashSet<String>,
    _lock: DaemonLock,
    pub(crate) config: Config,
}

impl Daemon {
    /// # Errors
    /// fails if the directories, config, lock file, store, or startup reconcile fails
    pub fn new(paths: Paths, clock: Arc<dyn Clock>) -> Result<Self> {
        Self::with_spawner(paths, clock, Arc::new(ExecSpawner))
    }

    /// # Errors
    /// fails for the reasons `Daemon::new` does
    pub fn with_notifier(
        paths: Paths,
        clock: Arc<dyn Clock>,
        notifier: Arc<dyn Notifier>,
    ) -> Result<Self> {
        Self::with_spawner_and_notifier(paths, clock, Arc::new(ExecSpawner), notifier)
    }

    /// # Errors
    /// fails for the reasons `Daemon::new` does
    pub fn with_spawner(
        paths: Paths,
        clock: Arc<dyn Clock>,
        spawner: Arc<dyn Spawner>,
    ) -> Result<Self> {
        Self::with_spawner_and_notifier(paths, clock, spawner, Arc::new(RealNotifier))
    }

    /// # Errors
    /// fails for the reasons `Daemon::new` does
    pub fn with_spawner_and_tz(
        paths: Paths,
        clock: Arc<dyn Clock>,
        spawner: Arc<dyn Spawner>,
        tz: TimeZone,
    ) -> Result<Self> {
        Self::build(paths, clock, spawner, Arc::new(RealNotifier), tz)
    }

    /// # Errors
    /// fails for the reasons `Daemon::new` does
    pub fn with_spawner_and_notifier(
        paths: Paths,
        clock: Arc<dyn Clock>,
        spawner: Arc<dyn Spawner>,
        notifier: Arc<dyn Notifier>,
    ) -> Result<Self> {
        Self::build(paths, clock, spawner, notifier, system_time_zone())
    }

    fn build(
        paths: Paths,
        clock: Arc<dyn Clock>,
        spawner: Arc<dyn Spawner>,
        notifier: Arc<dyn Notifier>,
        tz: TimeZone,
    ) -> Result<Self> {
        paths.ensure_dirs()?;
        let config = Config::load(&paths)?;
        let lock = acquire_lock(&paths.lock_path)?;
        let store = Store::open(&paths.db_path)?;
        nightjar_core::log!("daemon started (pid {})", std::process::id());
        nightjar_core::log!(
            "scheduling in {}",
            tz.iana_name().unwrap_or("an unnamed fixed-offset zone")
        );
        let startup = clock.now();
        let daemon = Self {
            clock,
            tz,
            job_memory: HashMap::new(),
            store,
            spawner,
            notifier,
            overdue_dispatch_in_flight: Arc::new(Mutex::new(HashSet::new())),
            children: Vec::new(),
            consecutive_failures: 0,
            logged_refusals: HashSet::new(),
            last_sweep: None,
            started_at: startup,
            has_watched_the_clock: false,
            logged_warnings: HashSet::new(),
            _lock: lock,
            config,
            paths,
        };

        daemon.reconcile()?;

        Ok(daemon)
    }

    pub(crate) fn memory_for(&mut self, job: &str) -> &mut JobMemory {
        self.job_memory.entry(job.to_string()).or_default()
    }

    pub(crate) fn armed_for(&self, job: &str) -> Option<Timestamp> {
        self.job_memory.get(job).and_then(|m| m.armed_for)
    }
}

fn system_time_zone() -> TimeZone {
    match TimeZone::try_system() {
        Ok(tz) => tz,
        Err(e) => {
            nightjar_core::log!(
                "cannot determine the system time zone ({e}); falling back to UTC, \
                 which is probably not the zone these schedules were written for"
            );
            TimeZone::UTC
        }
    }
}

fn acquire_lock(lock_path: &std::path::Path) -> Result<DaemonLock> {
    let file = std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(false)
        .open(lock_path)
        .with_context(|| format!("opening lock file {}", lock_path.display()))?;

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

struct DaemonLock(File);

impl Drop for DaemonLock {
    fn drop(&mut self) {
        unsafe { libc::flock(self.0.as_raw_fd(), libc::LOCK_UN) };
    }
}
