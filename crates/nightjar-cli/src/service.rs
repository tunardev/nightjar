use crate::status::daemon_state;
use anyhow::{Context, Result, bail};
use nightjar_core::clock::{Clock, SystemClock};
use nightjar_core::format::relative_time;
use nightjar_core::paths::Paths;
use nightjar_runner::service;
use nightjar_store::Store;
use std::path::Path;

/// Set only by the integration tests' `nj_dry_run` helper. This stops
/// the suite from ever registering or tearing down a real daemon.
const DRY_RUN_ENV: &str = "NIGHTJAR_SERVICE_DRY_RUN";

pub trait CommandRunner {
    fn run(&self, argv: &[String]) -> Result<()>;
}

struct RealRunner;

impl CommandRunner for RealRunner {
    fn run(&self, argv: &[String]) -> Result<()> {
        let [prog, rest @ ..] = argv else {
            bail!("empty command");
        };
        let status = std::process::Command::new(prog)
            .args(rest)
            .status()
            .with_context(|| format!("running {}", argv.join(" ")))?;
        if !status.success() {
            bail!("{} exited with {status}", argv.join(" "));
        }
        Ok(())
    }
}

pub fn cmd_install() -> Result<i32> {
    let paths = Paths::resolve()?;
    let exe = std::env::current_exe().context("resolving the current executable's path")?;
    let root = service::install_root()?;
    let dry_run = std::env::var_os(DRY_RUN_ENV).is_some();
    do_install(&paths, &exe, &root, dry_run, &RealRunner)
}

pub fn cmd_uninstall() -> Result<i32> {
    let paths = Paths::resolve()?;
    let exe = std::env::current_exe().context("resolving the current executable's path")?;
    let root = service::install_root()?;
    let dry_run = std::env::var_os(DRY_RUN_ENV).is_some();
    do_uninstall(&paths, &exe, &root, dry_run, &RealRunner)
}

pub fn cmd_status() -> Result<i32> {
    let paths = Paths::resolve()?;
    let exe = std::env::current_exe().context("resolving the current executable's path")?;
    let root = service::install_root()?;
    println!("{}", status_line(&paths, &exe, &root)?);
    Ok(0)
}

fn do_install(
    paths: &Paths,
    exe: &Path,
    root: &Path,
    dry_run: bool,
    runner: &dyn CommandRunner,
) -> Result<i32> {
    let plan = service::plan(paths, exe, root)?;
    if let Some(parent) = plan.unit_path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    std::fs::write(&plan.unit_path, &plan.contents)
        .with_context(|| format!("writing {}", plan.unit_path.display()))?;
    println!("wrote {}", plan.unit_path.display());
    println!("registers: {} daemon", exe.display());

    if dry_run {
        println!("dry run: not registering with the OS ({DRY_RUN_ENV} is set)");
    } else {
        runner.run(&plan.register)?;
        println!("registered.");
    }
    maybe_enable_linger(cfg!(target_os = "linux"), dry_run, runner);
    Ok(0)
}

/// A systemd `--user` unit stops at logout unless lingering is enabled.
fn maybe_enable_linger(is_linux: bool, dry_run: bool, runner: &dyn CommandRunner) {
    if !is_linux || dry_run {
        return;
    }
    let Some(user) = std::env::var("USER").ok().filter(|u| !u.is_empty()) else {
        eprintln!(
            "nightjar: warning: USER is not set, cannot enable lingering; run \
             `loginctl enable-linger <user>` yourself so the daemon survives logout"
        );
        return;
    };
    let argv = vec![
        "loginctl".to_string(),
        "enable-linger".to_string(),
        user.clone(),
    ];
    if let Err(e) = runner.run(&argv) {
        eprintln!(
            "nightjar: warning: could not enable lingering ({e:#}); the service is \
             installed, but will stop at logout until you run \
             `loginctl enable-linger {user}` yourself"
        );
    }
}

fn do_uninstall(
    paths: &Paths,
    exe: &Path,
    root: &Path,
    dry_run: bool,
    runner: &dyn CommandRunner,
) -> Result<i32> {
    let plan = service::plan(paths, exe, root)?;
    if !plan.unit_path.exists() {
        println!("nothing installed at {}", plan.unit_path.display());
        return Ok(0);
    }

    // `launchctl unload -w <path>` needs the path to still exist.
    // Whether `systemctl disable` also takes a path is untested.
    if dry_run {
        println!("dry run: not unregistering with the OS ({DRY_RUN_ENV} is set)");
    } else {
        runner.run(&plan.unregister)?;
    }
    std::fs::remove_file(&plan.unit_path)
        .with_context(|| format!("removing {}", plan.unit_path.display()))?;
    println!("removed {}", plan.unit_path.display());
    Ok(0)
}

/// A unit file on disk doesn't mean the daemon is alive.
fn status_line(paths: &Paths, exe: &Path, root: &Path) -> Result<String> {
    let plan = service::plan(paths, exe, root)?;
    if !plan.unit_path.exists() {
        return Ok(format!(
            "service not installed (would install to {})",
            plan.unit_path.display()
        ));
    }

    let store = Store::open(&paths.db_path)?;
    let now = SystemClock.now();
    Ok(match daemon_state(&store, now)? {
        Some((beat, false)) => format!(
            "service installed at {}; daemon running (pid {})",
            plan.unit_path.display(),
            beat.pid
        ),
        Some((beat, true)) => format!(
            "service installed at {}; daemon not running — last heartbeat {}",
            plan.unit_path.display(),
            relative_time(beat.at, now)
        ),
        None => format!(
            "service installed at {}; daemon not running — it has never started",
            plan.unit_path.display()
        ),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::sync::Mutex;

    struct RecordingRunner {
        calls: Mutex<Vec<Vec<String>>>,
        fail_on: Option<&'static str>,
    }

    impl RecordingRunner {
        fn new() -> Self {
            Self {
                calls: Mutex::new(Vec::new()),
                fail_on: None,
            }
        }

        fn failing_on(prog: &'static str) -> Self {
            Self {
                calls: Mutex::new(Vec::new()),
                fail_on: Some(prog),
            }
        }
    }

    impl CommandRunner for RecordingRunner {
        fn run(&self, argv: &[String]) -> Result<()> {
            self.calls.lock().unwrap().push(argv.to_vec());
            if self
                .fail_on
                .is_some_and(|prog| argv.first().map(String::as_str) == Some(prog))
            {
                bail!("{prog} failed", prog = self.fail_on.unwrap());
            }
            Ok(())
        }
    }

    fn exe() -> PathBuf {
        PathBuf::from("/usr/local/bin/nightjar")
    }

    #[test]
    fn install_writes_exactly_one_unit_file_under_injected_root() {
        let home = tempfile::tempdir().unwrap();
        let root = tempfile::tempdir().unwrap();
        let paths = Paths::for_root(home.path());
        let runner = RecordingRunner::new();

        let code = do_install(&paths, &exe(), root.path(), true, &runner).unwrap();

        assert_eq!(code, 0);
        let entries: Vec<_> = std::fs::read_dir(root.path()).unwrap().collect();
        assert_eq!(entries.len(), 1, "expected exactly one unit file written");
    }

    #[test]
    fn install_never_invokes_runner_when_dry_run_is_set() {
        let home = tempfile::tempdir().unwrap();
        let root = tempfile::tempdir().unwrap();
        let paths = Paths::for_root(home.path());
        let runner = RecordingRunner::new();

        do_install(&paths, &exe(), root.path(), true, &runner).unwrap();

        assert!(
            runner.calls.lock().unwrap().is_empty(),
            "dry run must never register with the OS"
        );
    }

    #[test]
    fn maybe_enable_linger_invokes_loginctl_enable_linger_with_user_when_target_is_linux() {
        let runner = RecordingRunner::new();

        maybe_enable_linger(true, false, &runner);

        let calls = runner.calls.lock().unwrap();
        assert_eq!(calls.len(), 1);
        let user =
            std::env::var("USER").expect("USER must be set for this assertion to mean anything");
        assert_eq!(
            calls[0],
            vec!["loginctl".to_string(), "enable-linger".to_string(), user]
        );
    }

    #[test]
    fn maybe_enable_linger_does_nothing_when_target_is_not_linux() {
        let runner = RecordingRunner::new();

        maybe_enable_linger(false, false, &runner);

        assert!(runner.calls.lock().unwrap().is_empty());
    }

    #[test]
    fn maybe_enable_linger_is_gated_by_dry_run_like_every_other_os_call() {
        let runner = RecordingRunner::new();

        maybe_enable_linger(true, true, &runner);

        assert!(
            runner.calls.lock().unwrap().is_empty(),
            "NIGHTJAR_SERVICE_DRY_RUN must cover the linger attempt too"
        );
    }

    #[test]
    fn install_does_not_fail_when_loginctl_fails() {
        let home = tempfile::tempdir().unwrap();
        let root = tempfile::tempdir().unwrap();
        let paths = Paths::for_root(home.path());
        let runner = RecordingRunner::failing_on("loginctl");

        let code = do_install(&paths, &exe(), root.path(), false, &runner).unwrap();

        assert_eq!(code, 0);
    }

    #[test]
    fn maybe_enable_linger_does_not_panic_when_runner_fails() {
        let runner = RecordingRunner::failing_on("loginctl");
        maybe_enable_linger(true, false, &runner);
        assert_eq!(runner.calls.lock().unwrap().len(), 1);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn do_install_enables_lingering_when_target_is_linux() {
        let home = tempfile::tempdir().unwrap();
        let root = tempfile::tempdir().unwrap();
        let paths = Paths::for_root(home.path());
        let runner = RecordingRunner::new();

        do_install(&paths, &exe(), root.path(), false, &runner).unwrap();

        let calls = runner.calls.lock().unwrap();
        assert!(
            calls
                .iter()
                .any(|c| c.first().map(String::as_str) == Some("loginctl")),
            "expected a loginctl call among {calls:?}"
        );
    }

    #[test]
    fn install_invokes_runner_with_register_argv_when_dry_run_is_not_set() {
        let home = tempfile::tempdir().unwrap();
        let root = tempfile::tempdir().unwrap();
        let paths = Paths::for_root(home.path());
        let runner = RecordingRunner::new();
        let expected = service::plan(&paths, &exe(), root.path()).unwrap();

        do_install(&paths, &exe(), root.path(), false, &runner).unwrap();

        let calls = runner.calls.lock().unwrap();
        assert_eq!(calls[0], expected.register);
        if cfg!(target_os = "linux") {
            assert_eq!(calls.len(), 2, "expected register, then the linger attempt");
            assert_eq!(calls[1][0], "loginctl");
        } else {
            assert_eq!(calls.len(), 1);
        }
    }

    #[test]
    fn uninstall_is_noop_and_never_invokes_runner_when_root_is_fresh() {
        let home = tempfile::tempdir().unwrap();
        let root = tempfile::tempdir().unwrap();
        let paths = Paths::for_root(home.path());
        let runner = RecordingRunner::new();

        let code = do_uninstall(&paths, &exe(), root.path(), false, &runner).unwrap();

        assert_eq!(code, 0);
        assert!(runner.calls.lock().unwrap().is_empty());
    }

    #[test]
    fn uninstall_removes_previously_installed_unit() {
        let home = tempfile::tempdir().unwrap();
        let root = tempfile::tempdir().unwrap();
        let paths = Paths::for_root(home.path());
        let runner = RecordingRunner::new();
        do_install(&paths, &exe(), root.path(), true, &runner).unwrap();
        let plan = service::plan(&paths, &exe(), root.path()).unwrap();
        assert!(plan.unit_path.exists());

        do_uninstall(&paths, &exe(), root.path(), true, &runner).unwrap();

        assert!(!plan.unit_path.exists());
    }

    #[test]
    fn uninstall_unregisters_before_deleting_file_os_was_pointed_at() {
        let home = tempfile::tempdir().unwrap();
        let root = tempfile::tempdir().unwrap();
        let paths = Paths::for_root(home.path());
        let runner = RecordingRunner::new();
        do_install(&paths, &exe(), root.path(), true, &runner).unwrap();
        let expected = service::plan(&paths, &exe(), root.path()).unwrap();

        do_uninstall(&paths, &exe(), root.path(), false, &runner).unwrap();

        let calls = runner.calls.lock().unwrap();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0], expected.unregister);
    }

    #[test]
    fn status_line_reports_not_installed_when_unit_file_is_absent() {
        let home = tempfile::tempdir().unwrap();
        let root = tempfile::tempdir().unwrap();
        let paths = Paths::for_root(home.path());

        let s = status_line(&paths, &exe(), root.path()).unwrap();

        assert!(s.contains("not installed"), "got: {s}");
    }

    #[test]
    fn status_line_reports_installed_but_not_running_when_no_heartbeat_was_ever_written() {
        let home = tempfile::tempdir().unwrap();
        let root = tempfile::tempdir().unwrap();
        let paths = Paths::for_root(home.path());
        let runner = RecordingRunner::new();
        do_install(&paths, &exe(), root.path(), true, &runner).unwrap();
        Store::open(&paths.db_path).unwrap();

        let s = status_line(&paths, &exe(), root.path()).unwrap();

        assert!(s.contains("installed"), "got: {s}");
        assert!(s.contains("not running"), "got: {s}");
        assert!(!s.contains("not installed"), "got: {s}");
    }

    #[test]
    fn status_line_reports_running_when_heartbeat_is_fresh() {
        let home = tempfile::tempdir().unwrap();
        let root = tempfile::tempdir().unwrap();
        let paths = Paths::for_root(home.path());
        let runner = RecordingRunner::new();
        do_install(&paths, &exe(), root.path(), true, &runner).unwrap();
        let store = Store::open(&paths.db_path).unwrap();
        store
            .write_heartbeat(SystemClock.now(), 4242, "0.1.0")
            .unwrap();

        let s = status_line(&paths, &exe(), root.path()).unwrap();

        assert!(s.contains("running"), "got: {s}");
        assert!(!s.contains("not running"), "got: {s}");
        assert!(s.contains("4242"), "got: {s}");
    }

    #[test]
    fn status_line_reports_not_running_when_heartbeat_is_stale() {
        let home = tempfile::tempdir().unwrap();
        let root = tempfile::tempdir().unwrap();
        let paths = Paths::for_root(home.path());
        let runner = RecordingRunner::new();
        do_install(&paths, &exe(), root.path(), true, &runner).unwrap();
        let store = Store::open(&paths.db_path).unwrap();
        let old = SystemClock.now() - jiff::Span::new().hours(2);
        store.write_heartbeat(old, 4242, "0.1.0").unwrap();

        let s = status_line(&paths, &exe(), root.path()).unwrap();

        assert!(s.contains("not running"), "got: {s}");
    }
}
