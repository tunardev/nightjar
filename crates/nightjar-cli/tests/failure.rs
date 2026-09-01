use jiff::{Span, Timestamp};
use nightjar_cli::config::Config;
use nightjar_cli::paths::Paths;
use nightjar_cli::runner::capture::TRUNCATION_MARKER;
use nightjar_cli::store::Store;
use nightjar_cli::store::run::{Run, RunStatus, Trigger};
use std::io::{BufRead, BufReader};
use std::os::unix::process::ExitStatusExt;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_nightjar")
}

struct Home {
    tmp: tempfile::TempDir,
    paths: Paths,
}

impl Home {
    fn new() -> Home {
        let tmp = tempfile::tempdir().unwrap();
        let paths = Paths::for_root(tmp.path());
        paths.ensure_dirs().unwrap();
        Store::open(&paths.db_path).unwrap();
        Home { tmp, paths }
    }

    fn root(&self) -> &Path {
        self.tmp.path()
    }

    fn write_job(&self, name: &str, body: &str) {
        std::fs::write(self.paths.jobs_dir.join(format!("{name}.toml")), body).unwrap();
    }

    fn write_config(&self, body: &str) {
        std::fs::write(self.paths.config_dir.join("config.toml"), body).unwrap();
    }

    fn store(&self) -> Store {
        Store::open(&self.paths.db_path).unwrap()
    }

    fn command(&self) -> Command {
        let mut cmd = Command::new(bin());
        cmd.env("NIGHTJAR_HOME", self.root())
            .env("HOME", self.root())
            .env("TZ", "UTC");
        cmd
    }

    fn exec(&self, job: &str, run_id: &str) -> ChildGuard {
        let child = self
            .command()
            .args([
                "exec",
                &format!("--job={job}"),
                &format!("--run={run_id}"),
                "--trigger=manual",
            ])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        ChildGuard(child)
    }
}

struct ChildGuard(Child);

impl Drop for ChildGuard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

struct DaemonProcess {
    child: Child,
    up: Arc<AtomicBool>,
}

impl DaemonProcess {
    fn start(home: &Home) -> DaemonProcess {
        let mut child = home
            .command()
            .arg("daemon")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let stderr = child.stderr.take().unwrap();
        let up = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&up);
        std::thread::spawn(move || {
            for line in BufReader::new(stderr).lines().map_while(Result::ok) {
                if line.contains("daemon started") {
                    flag.store(true, Ordering::SeqCst);
                }
            }
        });
        DaemonProcess { child, up }
    }

    fn is_up(&self) -> bool {
        wait_for(Duration::from_secs(20), || {
            self.up.load(Ordering::SeqCst).then_some(())
        })
        .is_some()
    }

    fn pid(&self) -> u32 {
        self.child.id()
    }

    fn is_alive(&mut self) -> bool {
        matches!(self.child.try_wait(), Ok(None))
    }

    fn stop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }

    fn sigkill(&mut self) -> std::process::ExitStatus {
        signal(self.child.id(), libc::SIGKILL);
        self.child.wait().unwrap()
    }
}

impl Drop for DaemonProcess {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[derive(Default)]
struct Orphans {
    processes: Vec<u32>,
    groups: Vec<u32>,
}

impl Drop for Orphans {
    fn drop(&mut self) {
        for group in &self.groups {
            kill_group(*group, libc::SIGKILL);
        }
        for pid in &self.processes {
            signal(*pid, libc::SIGKILL);
        }
    }
}

fn signal(pid: u32, sig: libc::c_int) {
    let Ok(pid) = libc::pid_t::try_from(pid) else {
        return;
    };
    if pid > 1 {
        unsafe { libc::kill(pid, sig) };
    }
}

fn kill_group(pgid: u32, sig: libc::c_int) {
    let Ok(pgid) = libc::pid_t::try_from(pgid) else {
        return;
    };
    if pgid > 1 {
        unsafe { libc::kill(-pgid, sig) };
    }
}

fn is_alive(pid: u32) -> bool {
    match libc::pid_t::try_from(pid) {
        Ok(p) if p > 1 => unsafe { libc::kill(p, 0) == 0 },
        _ => false,
    }
}

fn wait_for<T>(limit: Duration, mut f: impl FnMut() -> Option<T>) -> Option<T> {
    let deadline = Instant::now() + limit;
    loop {
        if let Some(value) = f() {
            return Some(value);
        }
        if Instant::now() >= deadline {
            return None;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn stop_and_quiesce(daemon: &mut DaemonProcess, store: &Store, job: &str) {
    daemon.stop();
    assert!(
        wait_for(Duration::from_secs(30), || {
            (store.running_count(job).ok()? == 0).then_some(())
        })
        .is_some(),
        "runs of {job:?} were still in flight long after the daemon stopped"
    );
}

fn runs(store: &Store, job: &str) -> Vec<Run> {
    store.recent_runs(Some(job), 100_000).unwrap()
}

fn read_pids(path: &Path) -> Option<Vec<u32>> {
    let text = std::fs::read_to_string(path).ok()?;
    let pids: Vec<u32> = text
        .split_whitespace()
        .map(|s| s.parse().ok())
        .collect::<Option<_>>()?;
    (!pids.is_empty()).then_some(pids)
}

#[derive(Debug, PartialEq, Eq)]
struct CatchUp {
    ran: usize,
    missed: usize,
}

fn catch_up_counts(store: &Store, job: &str) -> CatchUp {
    let rows = runs(store, job);
    CatchUp {
        ran: rows
            .iter()
            .filter(|r| r.trigger == Trigger::Catchup && r.status != RunStatus::Missed)
            .count(),
        missed: rows
            .iter()
            .filter(|r| r.status == RunStatus::Missed)
            .count(),
    }
}

fn watermark(store: &Store) -> Option<Timestamp> {
    store
        .daemon_heartbeat()
        .unwrap()
        .and_then(|beat| beat.caught_up_through)
}

fn fabricate_gap(home: &Home, gap: Span) -> Timestamp {
    let since = Timestamp::now().checked_sub(gap).unwrap();
    let store = home.store();
    store
        .write_heartbeat(since, std::process::id(), "0.0.0-fabricated")
        .unwrap();
    store.set_caught_up_through(since).unwrap();
    since
}

fn wait_for_catch_up(store: &Store, since: Timestamp) -> Timestamp {
    wait_for(Duration::from_secs(30), || {
        watermark(store).filter(|w| *w > since)
    })
    .expect("the daemon never consumed the fabricated gap")
}

fn occurrences(since: Timestamp, until: Timestamp, period: i64) -> usize {
    let count = until.as_second().div_euclid(period) - since.as_second().div_euclid(period);
    usize::try_from(count).unwrap()
}

#[test]
#[ignore = "SIGKILLs a real wrapper mid-run; run with --ignored"]
fn wrapper_reconciles_to_unknown_when_sigkilled() {
    let home = Home::new();
    let pidfile = home.root().join("stuck.pids");
    home.write_job(
        "stuck",
        &format!(
            "command = \"echo $$ > {} ; sleep 600\"\n\
             schedule = \"0 0 1 1 *\"\n\
             enabled = false\n\
             shell = \"/bin/sh\"\n\
             login_shell = false\n",
            pidfile.display()
        ),
    );

    let store = home.store();
    let run_id = "wrapper-sigkill";
    let mut orphans = Orphans::default();
    let mut wrapper = home.exec("stuck", run_id);

    let group = wait_for(Duration::from_secs(30), || {
        read_pids(&pidfile)?.first().copied()
    })
    .expect("the job never reported its process group");
    orphans.groups.push(group);

    let recorded_pid = wait_for(Duration::from_secs(30), || store.get_run(run_id).ok()??.pid)
        .expect("the wrapper never recorded a running row with a pid");
    assert_eq!(
        recorded_pid,
        wrapper.0.id(),
        "the row must carry the wrapper's own pid, which is what reconciliation probes"
    );

    signal(wrapper.0.id(), libc::SIGKILL);
    let died = wrapper.0.wait().unwrap();
    assert_eq!(
        died.signal(),
        Some(libc::SIGKILL),
        "the wrapper must have died by the kill rather than finishing on its own"
    );

    let stale = store.get_run(run_id).unwrap().unwrap();
    assert_eq!(
        stale.status,
        RunStatus::Running,
        "the killed wrapper must have left a stale running row, or there is nothing to reconcile"
    );
    assert!(stale.finished_at.is_none());

    let daemon = DaemonProcess::start(&home);
    assert!(daemon.is_up(), "the daemon never started");

    let settled = wait_for(Duration::from_secs(30), || {
        let run = store.get_run(run_id).ok()??;
        run.finished_at.map(|_| run.status)
    })
    .expect("the row stayed `running`; such a row silences an overlap=skip job forever");

    assert_eq!(
        settled,
        RunStatus::Unknown,
        "a run nobody can account for is `unknown`, never a verdict"
    );
}

const OUTAGE: Duration = Duration::from_secs(6);

#[test]
#[ignore = "SIGKILLs a real daemon and waits out an outage; run with --ignored"]
fn daemon_recovers_and_accounts_for_gap_when_sigkilled() {
    let home = Home::new();
    home.write_job(
        "beat",
        "command = \"true\"\n\
         schedule = \"* * * * * *\"\n\
         catchup = \"once\"\n\
         overlap = \"skip\"\n\
         shell = \"/bin/sh\"\n\
         login_shell = false\n",
    );
    let store = home.store();

    let mut first = DaemonProcess::start(&home);
    assert!(first.is_up(), "the first daemon never started");
    assert!(
        wait_for(Duration::from_secs(30), || {
            runs(&store, "beat")
                .iter()
                .any(|r| r.status == RunStatus::Success)
                .then_some(())
        })
        .is_some(),
        "the first daemon never completed a run, so there is no established cadence to interrupt"
    );

    let died = first.sigkill();
    assert_eq!(died.signal(), Some(libc::SIGKILL));

    let since = watermark(&store).expect("the first daemon left no watermark");
    std::thread::sleep(OUTAGE);

    let mut second = DaemonProcess::start(&home);
    assert!(
        second.is_up(),
        "a SIGKILLed daemon releases its flock only by dying, and the successor could not take it"
    );

    let outage_secs = i64::try_from(OUTAGE.as_secs()).unwrap();
    let after_outage = since.checked_add(Span::new().seconds(outage_secs)).unwrap();
    assert!(
        wait_for(Duration::from_secs(30), || {
            runs(&store, "beat")
                .iter()
                .any(|r| {
                    r.trigger == Trigger::Schedule
                        && r.status == RunStatus::Success
                        && r.started_at >= after_outage
                })
                .then_some(())
        })
        .is_some(),
        "the restarted daemon never resumed running the job on schedule"
    );
    stop_and_quiesce(&mut second, &store, "beat");

    let mut accounted: Vec<i64> = runs(&store, "beat")
        .iter()
        .filter(|r| r.status == RunStatus::Missed)
        .map(|r| r.started_at.as_second())
        .collect();
    accounted.sort_unstable();

    assert!(
        accounted.len() >= 4,
        "{outage_secs}s outage accounted for {} occurrences",
        accounted.len()
    );
    assert_eq!(
        accounted.first().copied(),
        Some(since.as_second() + 1),
        "accounting must start at the first occurrence after the dead daemon's watermark"
    );
    let contiguous = i64::try_from(
        accounted
            .windows(2)
            .take_while(|w| w[1] - w[0] == 1)
            .count(),
    )
    .unwrap();
    let reached = accounted[0] + contiguous;
    let target = since.as_second() + outage_secs - 2;
    assert!(
        reached >= target,
        "run from {} reached {reached}, short of {target}: {accounted:?}",
        accounted[0]
    );
}

#[test]
#[ignore = "drives three real daemons through a fabricated six-hour gap; run with --ignored"]
fn six_hour_gap_honours_each_catchup_policy_and_records_rest() {
    six_hour_gap("none", "skip", |_| 0);
    six_hour_gap("once", "skip", |_| 1);
    six_hour_gap("all", "parallel", |occurrences| occurrences);
}

fn six_hour_gap(catchup: &str, overlap: &str, expected_runs: fn(usize) -> usize) {
    let home = Home::new();
    home.write_job(
        "hourly",
        &format!(
            "command = \"true\"\n\
             schedule = \"0 * * * *\"\n\
             catchup = \"{catchup}\"\n\
             overlap = \"{overlap}\"\n\
             shell = \"/bin/sh\"\n\
             login_shell = false\n"
        ),
    );
    let store = home.store();
    let since = fabricate_gap(&home, Span::new().hours(6));

    let mut daemon = DaemonProcess::start(&home);
    assert!(daemon.is_up(), "the daemon never started");
    let consumed = wait_for_catch_up(&store, since);

    let expected = occurrences(since, consumed, 3600);
    assert!(
        (6..=7).contains(&expected),
        "expected 6 or 7 occurrences, got {expected}"
    );
    let want_ran = expected_runs(expected);

    let counts = wait_for(Duration::from_secs(30), || {
        let counts = catch_up_counts(&store, "hourly");
        (counts.ran == want_ran && counts.ran + counts.missed == expected).then_some(counts)
    })
    .unwrap_or_else(|| {
        panic!(
            "catchup = {catchup:?} never settled at {want_ran} make-up run(s) with the \
             remaining {expected} occurrences recorded: {:?}",
            catch_up_counts(&store, "hourly")
        )
    });
    stop_and_quiesce(&mut daemon, &store, "hourly");

    assert_eq!(
        counts.ran, want_ran,
        "catchup = {catchup:?} must make up {want_ran} of {expected} occurrences"
    );
    assert_eq!(
        counts.missed,
        expected - want_ran,
        "catchup = {catchup:?}: every occurrence that did not run must be recorded missed"
    );
}

const FLOOD_BYTES: u64 = 4 * 1024 * 1024;
const FLOOD_CAP: u64 = 64 * 1024;

#[test]
#[ignore = "pushes megabytes through a real capture pipeline; run with --ignored"]
fn job_does_not_take_down_daemon_when_it_floods_output() {
    let home = Home::new();
    home.write_config(&format!("output_cap = \"{FLOOD_CAP}\"\n"));
    home.write_job(
        "flood",
        &format!(
            "command = \"yes nightjar | head -c {FLOOD_BYTES}\"\n\
             schedule = \"* * * * * *\"\n\
             catchup = \"none\"\n\
             overlap = \"skip\"\n\
             shell = \"/bin/sh\"\n\
             login_shell = false\n"
        ),
    );
    let store = home.store();

    let mut daemon = DaemonProcess::start(&home);
    assert!(daemon.is_up(), "the daemon never started");

    let finished = wait_for(Duration::from_secs(60), || {
        let done: Vec<Run> = runs(&store, "flood")
            .into_iter()
            .filter(|r| r.finished_at.is_some() && r.status != RunStatus::Missed)
            .collect();
        (done.len() >= 2).then_some(done)
    })
    .expect("the daemon did not complete two flooding runs");

    assert!(
        daemon.is_alive(),
        "the daemon must outlive a job that saturates the output cap"
    );
    stop_and_quiesce(&mut daemon, &store, "flood");

    for run in &finished {
        assert_eq!(
            run.status,
            RunStatus::Success,
            "a job whose output was truncated still succeeded"
        );
        assert_eq!(
            run.output_bytes, FLOOD_BYTES,
            "the row must record what the job produced, not what survived the cap"
        );

        let captured = std::fs::read(run.stdout_path.as_ref().unwrap()).unwrap();
        assert_eq!(
            captured.len() as u64,
            FLOOD_CAP + TRUNCATION_MARKER.len() as u64,
            "the capture file must hold the cap plus one marker"
        );
        assert!(
            captured.ends_with(TRUNCATION_MARKER.as_bytes()),
            "a truncated capture must say so"
        );
    }
}

#[test]
#[ignore = "leaves a real background descendant to be killed; run with --ignored"]
fn job_is_still_killed_at_timeout_when_it_forks_a_descendant() {
    let home = Home::new();
    let pidfile = home.root().join("forker.pids");
    home.write_job(
        "forker",
        &format!(
            "command = \"sleep 600 & echo $$ $! > {} ; exit 0\"\n\
             schedule = \"0 0 1 1 *\"\n\
             timeout = \"2s\"\n\
             shell = \"/bin/sh\"\n\
             login_shell = false\n",
            pidfile.display()
        ),
    );

    let store = home.store();
    let run_id = "forking-timeout";
    let mut orphans = Orphans::default();
    let mut wrapper = home.exec("forker", run_id);

    let pids = wait_for(Duration::from_secs(30), || {
        let pids = read_pids(&pidfile)?;
        (pids.len() == 2).then_some(pids)
    })
    .expect("the job never reported its own group and its background descendant");
    let (group, descendant) = (pids[0], pids[1]);
    orphans.groups.push(group);
    orphans.processes.push(descendant);
    assert!(
        is_alive(descendant),
        "the descendant must be running, or the timeout has nothing to escape it"
    );

    let outcome = wait_for(Duration::from_secs(30), || {
        let run = store.get_run(run_id).ok()??;
        run.finished_at.map(|_| run)
    })
    .expect(
        "the run never finished: the 2s timeout did not fire while a descendant held the pipes",
    );
    let _ = wrapper.0.wait();

    assert_eq!(
        outcome.status,
        RunStatus::Timeout,
        "the wrapper decides, not the job's own shell, which exited 0 while the run continued"
    );
    assert!(
        wait_for(Duration::from_secs(5), || (!is_alive(descendant))
            .then_some(()))
        .is_some(),
        "descendant outlived timeout, still holds pipes"
    );
    assert_eq!(
        outcome.exit_code,
        Some(0),
        "the shell's clean exit code is kept; only `status` says the run was cut short"
    );
    assert!(
        outcome.duration_ms.unwrap() < 10_000,
        "a 2s timeout must end the run near 2s, not once some later backstop notices: {:?}ms",
        outcome.duration_ms
    );
}

#[test]
#[ignore = "spawns real make-up runs behind a fabricated six-hour gap; run with --ignored"]
fn catch_up_never_exceeds_overlap_policy_when_gap_is_large() {
    let unbounded = Config::default().catchup_max;
    large_gap_catch_up("skip", 1);
    large_gap_catch_up("queue", 1);
    large_gap_catch_up("parallel", unbounded);
}

fn large_gap_catch_up(overlap: &str, want_ran: usize) {
    let home = Home::new();
    home.write_job(
        "minutely",
        &format!(
            "command = \"true\"\n\
             schedule = \"* * * * *\"\n\
             catchup = \"all\"\n\
             overlap = \"{overlap}\"\n\
             shell = \"/bin/sh\"\n\
             login_shell = false\n"
        ),
    );
    let store = home.store();
    let since = fabricate_gap(&home, Span::new().hours(6));

    let mut daemon = DaemonProcess::start(&home);
    assert!(daemon.is_up(), "the daemon never started");
    let consumed = wait_for_catch_up(&store, since);

    let expected = occurrences(since, consumed, 60);
    assert!(
        (360..=361).contains(&expected),
        "expected 360 or 361 occurrences, got {expected}"
    );

    let counts = wait_for(Duration::from_secs(60), || {
        let counts = catch_up_counts(&store, "minutely");
        (counts.ran == want_ran && counts.ran + counts.missed == expected).then_some(counts)
    })
    .unwrap_or_else(|| {
        panic!(
            "overlap = {overlap:?} never settled at {want_ran} make-up run(s) with the \
             remaining {expected} occurrences recorded: {:?}",
            catch_up_counts(&store, "minutely")
        )
    });
    stop_and_quiesce(&mut daemon, &store, "minutely");

    assert_eq!(
        counts.ran, want_ran,
        "overlap = {overlap:?}, {expected} occurrences"
    );
    assert_eq!(
        counts.missed,
        expected - want_ran,
        "overlap = {overlap:?}: every occurrence the policy refused to run must be recorded missed"
    );
}

const CRASH_WINDOW_JOBS: usize = 60;

fn kill_when_the_watermark_moves(
    daemon: &mut DaemonProcess,
    store: &Store,
    since: Timestamp,
) -> Timestamp {
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        if let Some(consumed) = watermark(store).filter(|w| *w > since) {
            daemon.sigkill();
            return consumed;
        }
        assert!(
            Instant::now() < deadline,
            "the daemon never consumed the fabricated gap"
        );
        std::thread::sleep(Duration::from_millis(1));
    }
}

fn quiesce(store: &Store, jobs: &[String]) {
    assert!(
        wait_for(Duration::from_secs(60), || {
            jobs.iter()
                .all(|job| store.running_count(job).unwrap_or(1) == 0)
                .then_some(())
        })
        .is_some(),
        "make-up runs were still in flight long after the daemon was killed"
    );
}

#[test]
#[ignore = "SIGKILLs a real daemon inside its catch-up commit window; run with --ignored"]
fn daemon_loses_no_occurrence_when_killed_the_instant_it_consumes_a_gap() {
    let home = Home::new();
    let jobs: Vec<String> = (0..CRASH_WINDOW_JOBS)
        .map(|i| format!("beat{i:02}"))
        .collect();
    for job in &jobs {
        home.write_job(
            job,
            "command = \"true\"\n\
             schedule = \"* * * * * *\"\n\
             catchup = \"once\"\n\
             overlap = \"skip\"\n\
             shell = \"/bin/sh\"\n\
             login_shell = false\n",
        );
    }
    let store = home.store();
    let since = fabricate_gap(&home, Span::new().minutes(2));

    let mut daemon = DaemonProcess::start(&home);
    assert!(daemon.is_up(), "the daemon never started");

    let consumed = kill_when_the_watermark_moves(&mut daemon, &store, since);
    let owed = occurrences(since, consumed, 1);
    assert!(
        (118..=180).contains(&owed),
        "the fabricated two-minute gap should be about 120 occurrences; got {owed}"
    );
    quiesce(&store, &jobs);

    let short: Vec<(&String, CatchUp)> = jobs
        .iter()
        .map(|job| (job, catch_up_counts(&store, job)))
        .filter(|(_, counts)| counts.ran + counts.missed != owed)
        .collect();
    assert!(
        short.is_empty(),
        "{} of {} jobs short of {owed} occurrences: {:?}",
        short.len(),
        jobs.len(),
        &short[..short.len().min(3)]
    );
}

fn assert_nothing_behind_the_watermark(store: &Store, job: &str, since: Timestamp) {
    let until = watermark(store).expect("the daemon left no watermark");
    let owed = occurrences(since, until, 1);
    assert!(owed >= 4, "{owed} occurrence(s) in ({since}, {until}]");
    let rows = runs(store, job);
    let recorded = rows.iter().filter(|r| r.started_at > since).count();
    assert!(
        recorded >= owed,
        "{} missing: {recorded} of {owed} occurrence(s) in ({since}, {until}]",
        owed.saturating_sub(recorded)
    );

    let mut missed: Vec<i64> = rows
        .iter()
        .filter(|r| r.status == RunStatus::Missed)
        .map(|r| r.started_at.as_second())
        .collect();
    missed.sort_unstable();
    let mut distinct = missed.clone();
    distinct.dedup();
    assert_eq!(
        distinct, missed,
        "one occurrence, one row: {job:?} has more than one `missed` row for the same second"
    );
}

fn first_watermark(store: &Store) -> Timestamp {
    wait_for(Duration::from_secs(20), || watermark(store))
        .expect("the daemon never wrote a watermark")
}

fn wait_past(store: &Store, since: Timestamp) {
    wait_for(Duration::from_secs(20), || {
        watermark(store).filter(|w| *w > since)
    })
    .expect("the watermark never advanced");
}

#[test]
#[ignore = "stalls a real daemon for several seconds; run with --ignored"]
fn tick_leaves_no_occurrence_without_row_when_it_outran_schedule() {
    let home = Home::new();
    home.write_job(
        "beat",
        "command = \"true\"\n\
         schedule = \"* * * * * *\"\n\
         overlap = \"parallel\"\n\
         shell = \"/bin/sh\"\n\
         login_shell = false\n",
    );
    let store = home.store();
    let mut daemon = DaemonProcess::start(&home);
    assert!(daemon.is_up(), "the daemon never started");

    let first = first_watermark(&store);
    signal(daemon.pid(), libc::SIGSTOP);
    std::thread::sleep(Duration::from_secs(6));
    signal(daemon.pid(), libc::SIGCONT);
    wait_past(&store, first);

    stop_and_quiesce(&mut daemon, &store, "beat");
    assert_nothing_behind_the_watermark(&store, "beat", first);
}

#[test]
#[ignore = "runs a real daemon for several seconds; run with --ignored"]
fn occurrence_is_recorded_not_silenced_when_overlap_policy_skips_it() {
    let home = Home::new();
    home.write_job(
        "busy",
        "command = \"sleep 4\"\n\
         schedule = \"* * * * * *\"\n\
         overlap = \"skip\"\n\
         shell = \"/bin/sh\"\n\
         login_shell = false\n",
    );
    let store = home.store();
    let mut daemon = DaemonProcess::start(&home);
    assert!(daemon.is_up(), "the daemon never started");

    let first = first_watermark(&store);
    std::thread::sleep(Duration::from_secs(8));
    wait_past(&store, first);

    stop_and_quiesce(&mut daemon, &store, "busy");
    assert_nothing_behind_the_watermark(&store, "busy", first);
    assert!(
        runs(&store, "busy")
            .iter()
            .any(|r| r.status == RunStatus::Missed),
        "nothing was skipped by the overlap policy"
    );
}

#[test]
#[ignore = "drives a real daemon through a gap that already holds rows; run with --ignored"]
fn gap_does_not_record_rows_a_second_time_when_it_already_holds_them() {
    let home = Home::new();
    home.write_job(
        "beat",
        "command = \"true\"\n\
         schedule = \"* * * * * *\"\n\
         catchup = \"none\"\n\
         overlap = \"skip\"\n\
         shell = \"/bin/sh\"\n\
         login_shell = false\n",
    );
    let store = home.store();
    let since = fabricate_gap(&home, Span::new().seconds(20));

    let predecessor: Vec<i64> = (1..=5).map(|n| since.as_second() + n).collect();
    for second in &predecessor {
        store
            .record_missed_run(
                &format!("predecessor-{second}"),
                "beat",
                Trigger::Schedule,
                Timestamp::from_second(*second).unwrap(),
            )
            .unwrap();
    }

    let mut daemon = DaemonProcess::start(&home);
    assert!(daemon.is_up(), "the daemon never started");
    wait_for_catch_up(&store, since);
    stop_and_quiesce(&mut daemon, &store, "beat");

    let mut seconds: Vec<i64> = runs(&store, "beat")
        .iter()
        .filter(|r| r.status == RunStatus::Missed)
        .map(|r| r.started_at.as_second())
        .collect();
    seconds.sort_unstable();
    let mut distinct = seconds.clone();
    distinct.dedup();

    let doubled: Vec<i64> = predecessor
        .iter()
        .copied()
        .filter(|s| seconds.iter().filter(|x| *x == s).count() > 1)
        .collect();
    assert!(
        doubled.is_empty(),
        "catch-up wrote a second row for {doubled:?}"
    );
    assert_eq!(
        distinct, seconds,
        "no occurrence may hold two `missed` rows: {seconds:?}"
    );
}
