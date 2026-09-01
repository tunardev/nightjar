#![allow(dead_code)]

use nightjar_core::clock::{Clock, FixedClock};
use nightjar_core::paths::Paths;
use nightjar_daemon::{Daemon, Spawner};
use nightjar_store::Store;
use nightjar_store::run::{RunStatus, Trigger};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

pub fn setup(jobs: &[(&str, &str)]) -> (tempfile::TempDir, Paths) {
    let tmp = tempfile::tempdir().unwrap();
    let paths = Paths::for_root(tmp.path());
    paths.ensure_dirs().unwrap();
    for (name, body) in jobs {
        std::fs::write(paths.jobs_dir.join(format!("{name}.toml")), body).unwrap();
    }
    (tmp, paths)
}

pub struct FakeSpawner {
    fail: AtomicBool,
    fail_only: Mutex<Option<String>>,
    attempts: AtomicUsize,
    per_job: Mutex<std::collections::HashMap<String, usize>>,
    pids: Mutex<Vec<u32>>,
    db_path: std::path::PathBuf,
    clock: Arc<dyn Clock>,
}

impl FakeSpawner {
    pub fn new(fail: bool, db_path: std::path::PathBuf, clock: Arc<dyn Clock>) -> Arc<FakeSpawner> {
        Arc::new(FakeSpawner {
            fail: AtomicBool::new(fail),
            fail_only: Mutex::new(None),
            attempts: AtomicUsize::new(0),
            per_job: Mutex::new(std::collections::HashMap::new()),
            pids: Mutex::new(Vec::new()),
            db_path,
            clock,
        })
    }

    pub fn failing_only(
        job: &str,
        db_path: std::path::PathBuf,
        clock: Arc<dyn Clock>,
    ) -> Arc<FakeSpawner> {
        let s = FakeSpawner::new(false, db_path, clock);
        *s.fail_only.lock().unwrap() = Some(job.to_string());
        s
    }

    pub fn attempts(&self) -> usize {
        self.attempts.load(Ordering::SeqCst)
    }

    pub fn count_for(&self, job: &str) -> usize {
        self.per_job.lock().unwrap().get(job).copied().unwrap_or(0)
    }

    pub fn count(&self) -> usize {
        self.attempts()
    }

    pub fn pids(&self) -> Vec<u32> {
        self.pids.lock().unwrap().clone()
    }

    pub fn start_succeeding(&self) {
        self.fail.store(false, Ordering::SeqCst);
    }
}

impl Spawner for FakeSpawner {
    fn spawn(&self, job: &str, run_id: &str, trigger: Trigger) -> anyhow::Result<Child> {
        self.attempts.fetch_add(1, Ordering::SeqCst);
        *self
            .per_job
            .lock()
            .unwrap()
            .entry(job.to_string())
            .or_insert(0) += 1;
        let fail = match &*self.fail_only.lock().unwrap() {
            Some(only) => only == job,
            None => self.fail.load(Ordering::SeqCst),
        };
        if fail {
            anyhow::bail!("fake spawn failure for job {job:?}");
        }
        let child = Command::new("/bin/sh")
            .args(["-c", "exit 0"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()?;
        self.pids.lock().unwrap().push(child.id());

        let store = nightjar_store::Store::open(&self.db_path)
            .expect("opening the store from the fake spawner");
        let stub = std::path::PathBuf::from(format!("/tmp/nightjar-fake-{run_id}"));
        store
            .start_run(run_id, job, trigger, self.clock.now(), &stub, &stub)
            .expect("recording the fake run's start");
        store
            .set_run_pid(run_id, child.id())
            .expect("recording the fake run's pid");

        Ok(child)
    }
}

pub fn pid_exists(pid: u32) -> bool {
    libc::pid_t::try_from(pid).is_ok_and(|p| unsafe { libc::kill(p, 0) == 0 })
}

pub fn daemon_with_counting_spawner(
    paths: Paths,
    clock: Arc<FixedClock>,
) -> (Daemon, Arc<FakeSpawner>) {
    let spawner = FakeSpawner::new(false, paths.db_path.clone(), clock.clone());
    let d = Daemon::with_spawner(paths, clock, spawner.clone()).unwrap();
    (d, spawner)
}

pub fn missed_rows(paths: &Paths, job: &str) -> usize {
    let store = nightjar_store::Store::open(&paths.db_path).unwrap();
    store
        .recent_runs(Some(job), 1000)
        .unwrap()
        .into_iter()
        .filter(|r| r.status == RunStatus::Missed)
        .count()
}

pub fn write_finished_run_with_files(
    store: &Store,
    paths: &Paths,
    job: &str,
    id: &str,
    at: jiff::Timestamp,
) {
    let dir = paths.runs_dir.join(job);
    std::fs::create_dir_all(&dir).unwrap();
    let out = dir.join(format!("{id}.out"));
    let err = dir.join(format!("{id}.err"));
    std::fs::write(&out, "stdout").unwrap();
    std::fs::write(&err, "stderr").unwrap();
    store
        .start_run(id, job, Trigger::Schedule, at, &out, &err)
        .unwrap();
    store
        .finish_run(id, RunStatus::Success, Some(0), at, 0)
        .unwrap();
}

pub struct AbandoningSpawner {
    attempts: AtomicUsize,
    db_path: std::path::PathBuf,
    clock: Arc<dyn Clock>,
}

impl AbandoningSpawner {
    pub fn new(db_path: std::path::PathBuf, clock: Arc<dyn Clock>) -> Arc<AbandoningSpawner> {
        Arc::new(AbandoningSpawner {
            attempts: AtomicUsize::new(0),
            db_path,
            clock,
        })
    }

    pub fn count(&self) -> usize {
        self.attempts.load(Ordering::SeqCst)
    }
}

impl Spawner for AbandoningSpawner {
    fn spawn(&self, job: &str, run_id: &str, trigger: Trigger) -> anyhow::Result<Child> {
        self.attempts.fetch_add(1, Ordering::SeqCst);
        let child = Command::new("/bin/sh")
            .args(["-c", "exit 3"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()?;
        let store = Store::open(&self.db_path)?;
        let stub = std::path::PathBuf::from(format!("/tmp/nightjar-abandoned-{run_id}"));
        store.start_run(run_id, job, trigger, self.clock.now(), &stub, &stub)?;
        store.set_run_pid(run_id, std::process::id())?;
        Ok(child)
    }
}

/// Spawns a child that exits 0 and records the run as a finished success
/// on the spot, the way a real `nightjar exec` would have by the time the
/// daemon reaps it. Leaves `after_fired_at` NULL: stamping is the daemon's
/// job.
pub struct SucceedingSpawner {
    per_job: Mutex<std::collections::HashMap<String, usize>>,
    db_path: std::path::PathBuf,
    clock: Arc<dyn Clock>,
}

impl SucceedingSpawner {
    pub fn new(db_path: std::path::PathBuf, clock: Arc<dyn Clock>) -> Arc<SucceedingSpawner> {
        Arc::new(SucceedingSpawner {
            per_job: Mutex::new(std::collections::HashMap::new()),
            db_path,
            clock,
        })
    }

    pub fn count_for(&self, job: &str) -> usize {
        self.per_job.lock().unwrap().get(job).copied().unwrap_or(0)
    }
}

impl Spawner for SucceedingSpawner {
    fn spawn(&self, job: &str, run_id: &str, trigger: Trigger) -> anyhow::Result<Child> {
        *self
            .per_job
            .lock()
            .unwrap()
            .entry(job.to_string())
            .or_insert(0) += 1;
        let child = Command::new("/bin/sh")
            .args(["-c", "exit 0"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()?;
        let store = Store::open(&self.db_path)?;
        let stub = std::path::PathBuf::from(format!("/tmp/nightjar-succeeded-{run_id}"));
        let now = self.clock.now();
        store.start_run(run_id, job, trigger, now, &stub, &stub)?;
        store.set_run_pid(run_id, child.id())?;
        store.finish_run(run_id, RunStatus::Success, Some(0), now, 0)?;
        Ok(child)
    }
}
