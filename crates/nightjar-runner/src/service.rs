use anyhow::{Context, Result, bail};
use nightjar_core::paths::Paths;
use std::ffi::OsStr;
use std::path::{Path, PathBuf};

const LAUNCHD_LABEL: &str = "com.nightjar.daemon";

pub struct ServicePlan {
    pub unit_path: PathBuf,
    pub contents: String,
    pub register: Vec<String>,
    pub unregister: Vec<String>,
}

/// Pure: builds the unit text and register/unregister argv, but writes and
/// runs nothing. `install_root` is a parameter, not derived from `paths`.
/// launchd only scans `~/Library/LaunchAgents`, and systemd user units
/// must be under `~/.config/systemd/user`. `NIGHTJAR_HOME` governs
/// neither.
pub fn plan(paths: &Paths, exe: &Path, install_root: &Path) -> Result<ServicePlan> {
    if cfg!(target_os = "macos") {
        plan_launchd(paths, exe, install_root)
    } else if cfg!(target_os = "linux") {
        plan_systemd(paths, exe, install_root)
    } else {
        bail!("service install is only supported on macOS and Linux");
    }
}

fn log_path(paths: &Paths) -> PathBuf {
    paths.data_dir.join("nightjar.log")
}

/// The shell launchd should exec the daemon through, resolved the same
/// way `exec::shell_for` resolves it for a job: `$SHELL`, then `/bin/sh`.
///
/// Called from inside `nightjar service install`, invoked directly by the
/// user, so `$SHELL` is their real login shell. Launchd's own minimal
/// environment would not otherwise supply that.
fn install_shell() -> String {
    std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".to_string())
}

fn plan_launchd(paths: &Paths, exe: &Path, install_root: &Path) -> Result<ServicePlan> {
    let unit_path = install_root.join(format!("{LAUNCHD_LABEL}.plist"));
    let contents = render_launchd_plist(exe, &log_path(paths), &install_shell())?;
    let target = unit_value(&unit_path)?.to_owned();
    Ok(ServicePlan {
        register: vec![
            "launchctl".into(),
            "load".into(),
            "-w".into(),
            target.clone(),
        ],
        unregister: vec!["launchctl".into(), "unload".into(), "-w".into(), target],
        unit_path,
        contents,
    })
}

const SYSTEMD_UNIT_NAME: &str = "nightjar.service";

fn plan_systemd(paths: &Paths, exe: &Path, install_root: &Path) -> Result<ServicePlan> {
    let unit_path = install_root.join(SYSTEMD_UNIT_NAME);
    let contents = render_systemd_unit(exe, &log_path(paths))?;
    Ok(ServicePlan {
        register: vec![
            "systemctl".into(),
            "--user".into(),
            "enable".into(),
            "--now".into(),
            SYSTEMD_UNIT_NAME.into(),
        ],
        // `disable`, unlike `enable`, only accepts unit names, not file paths.
        unregister: vec![
            "systemctl".into(),
            "--user".into(),
            "disable".into(),
            "--now".into(),
            SYSTEMD_UNIT_NAME.into(),
        ],
        unit_path,
        contents,
    })
}

/// Resolves the real location a service manager reads units from.
/// `override_root` wins over everything, the same way `NIGHTJAR_HOME`
/// wins in `Paths::resolve_from`. This lets a test redirect it at a temp
/// directory instead of the caller's real home.
pub fn install_root_from(
    override_root: Option<&OsStr>,
    xdg_config_home: Option<&OsStr>,
    home: Option<&OsStr>,
) -> Result<PathBuf> {
    if let Some(root) = override_root
        && !root.is_empty()
    {
        return Ok(PathBuf::from(root));
    }

    if cfg!(target_os = "macos") {
        let home = home.filter(|h| !h.is_empty()).context("HOME is not set")?;
        Ok(PathBuf::from(home).join("Library/LaunchAgents"))
    } else if cfg!(target_os = "linux") {
        if let Some(xdg) = xdg_config_home.filter(|x| !x.is_empty()) {
            let xdg_path = PathBuf::from(xdg);
            if xdg_path.is_absolute() {
                return Ok(xdg_path.join("systemd/user"));
            }
        }
        let home = home.filter(|h| !h.is_empty()).context("HOME is not set")?;
        Ok(PathBuf::from(home).join(".config/systemd/user"))
    } else {
        bail!("service install is only supported on macOS and Linux");
    }
}

pub fn install_root() -> Result<PathBuf> {
    install_root_from(
        std::env::var_os("NIGHTJAR_SERVICE_INSTALL_ROOT").as_deref(),
        std::env::var_os("XDG_CONFIG_HOME").as_deref(),
        std::env::var_os("HOME").as_deref(),
    )
}

fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

/// `to_string_lossy` would substitute U+FFFD and point the unit at the
/// wrong path. That's worse than refusing outright. XML 1.0 cannot
/// represent most control characters, and a newline would end a systemd
/// directive early and start a new one.
fn unit_value(path: &Path) -> Result<&str> {
    let s = path.to_str().with_context(|| {
        format!(
            "{} is not valid UTF-8 and cannot go in a unit file",
            path.display()
        )
    })?;
    if let Some(c) = s.chars().find(|c| c.is_control()) {
        bail!(
            "{} contains the control character {c:?}, which cannot be escaped in a unit file",
            path.display()
        );
    }
    Ok(s)
}

/// Single-quotes `s` for a POSIX shell command line. A path containing a
/// single quote cannot be represented safely this way, so the caller must
/// refuse rather than emit a plist whose shell command breaks out.
fn shell_single_quote(s: &str) -> Result<String> {
    if s.contains('\'') {
        bail!("{s:?} contains a single quote and cannot be safely quoted for a shell command");
    }
    Ok(format!("'{s}'"))
}

fn render_launchd_plist(exe: &Path, log_path: &Path, shell: &str) -> Result<String> {
    let quoted_exe = shell_single_quote(unit_value(exe)?)?;
    let command = xml_escape(&format!("exec {quoted_exe} daemon"));
    let shell = xml_escape(unit_value(Path::new(shell))?);
    let log = xml_escape(unit_value(log_path)?);
    Ok(format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key>
    <string>{LAUNCHD_LABEL}</string>
    <key>ProgramArguments</key>
    <array>
        <string>{shell}</string>
        <string>-lc</string>
        <string>{command}</string>
    </array>
    <key>RunAtLoad</key>
    <true/>
    <key>KeepAlive</key>
    <dict>
        <key>SuccessfulExit</key>
        <false/>
    </dict>
    <key>StandardOutPath</key>
    <string>{log}</string>
    <key>StandardErrorPath</key>
    <string>{log}</string>
</dict>
</plist>
"#
    ))
}

fn render_systemd_unit(exe: &Path, log_path: &Path) -> Result<String> {
    // `%` introduces a specifier. An unescaped `%h` in a path would expand to
    // the user's home directory.
    let exe = unit_value(exe)?.replace('%', "%%");
    let log = unit_value(log_path)?.replace('%', "%%");
    Ok(format!(
        r#"[Unit]
Description=nightjar scheduler daemon
After=network.target

[Service]
Type=simple
ExecStart="{exe}" daemon
Restart=on-failure
RestartSec=5
KillMode=process
StandardOutput=append:{log}
StandardError=append:{log}

[Install]
WantedBy=default.target
"#
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generated_unit_names_this_binary_and_the_daemon_subcommand() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tempfile::tempdir().unwrap();
        let paths = Paths::for_root(tmp.path());
        let p = plan(&paths, Path::new("/opt/nightjar/bin/nightjar"), root.path()).unwrap();
        assert!(p.contents.contains("/opt/nightjar/bin/nightjar"));
        assert!(p.contents.contains("daemon"));
    }

    #[test]
    fn systemd_disable_is_given_a_unit_name_not_a_file_path() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tempfile::tempdir().unwrap();
        let paths = Paths::for_root(tmp.path());
        let p = plan_systemd(&paths, Path::new("/opt/nightjar/bin/nightjar"), root.path()).unwrap();

        assert_eq!(p.register.last().unwrap(), "nightjar.service");
        assert_eq!(p.unregister.last().unwrap(), "nightjar.service");
        assert!(
            !p.unregister.iter().any(|a| a.contains('/')),
            "no argument to systemctl disable may be a path: {:?}",
            p.unregister
        );
    }

    const EXE: &str = "/usr/local/bin/nightjar";
    const LOG: &str = "/var/log/nightjar/daemon.log";
    const SHELL: &str = "/bin/zsh";

    fn plist() -> String {
        render_launchd_plist(Path::new(EXE), Path::new(LOG), SHELL).unwrap()
    }
    fn unit() -> String {
        render_systemd_unit(Path::new(EXE), Path::new(LOG)).unwrap()
    }

    #[test]
    fn percent_is_doubled_so_systemd_does_not_expand_it_when_it_appears_in_a_path() {
        let u = render_systemd_unit(Path::new("/opt/%h/nightjar"), Path::new(LOG)).unwrap();
        assert!(u.contains("/opt/%%h/nightjar"), "got: {u}");
    }

    #[test]
    fn newline_is_refused_rather_than_injected_when_it_appears_in_a_path() {
        let err = render_systemd_unit(Path::new("/opt/nj\nExecStop=/bin/rm"), Path::new(LOG))
            .unwrap_err();
        assert!(err.to_string().contains("control character"), "got: {err}");
    }

    #[test]
    fn path_is_refused_rather_than_silently_altered_when_it_is_not_valid_utf8() {
        use std::os::unix::ffi::OsStrExt;
        let bad = Path::new(OsStr::from_bytes(b"/opt/ni\xffghtjar"));
        let err = render_launchd_plist(bad, Path::new(LOG), SHELL).unwrap_err();
        assert!(err.to_string().contains("not valid UTF-8"), "got: {err}");
    }

    #[test]
    fn launchd_plist_execs_the_daemon_through_the_users_login_shell() {
        let p = plist();
        assert!(
            p.contains(&format!("<string>{SHELL}</string>")),
            "the shell must be the first ProgramArguments entry; got: {p}"
        );
        assert!(p.contains("<string>-lc</string>"), "got: {p}");
        assert!(
            p.contains(&format!("<string>exec '{EXE}' daemon</string>")),
            "must exec single-quoted binary with `daemon`; got: {p}"
        );
    }

    #[test]
    fn exe_path_is_refused_rather_than_breaking_the_shell_command_when_it_has_a_single_quote() {
        let err =
            render_launchd_plist(Path::new("/opt/ni'ghtjar"), Path::new(LOG), SHELL).unwrap_err();
        assert!(err.to_string().contains("single quote"), "got: {err}");
    }

    #[test]
    fn launchd_plist_restarts_only_when_the_exit_is_unsuccessful() {
        let p = plist();
        assert!(
            p.contains("<key>SuccessfulExit</key>\n        <false/>"),
            "got: {p}"
        );
    }

    #[test]
    fn systemd_unit_restarts_only_when_it_fails() {
        let u = unit();
        assert!(u.contains("Restart=on-failure"), "got: {u}");
    }

    #[test]
    fn systemd_unit_kills_only_the_daemon_not_its_children() {
        let u = unit();
        assert!(u.contains("KillMode=process"), "got: {u}");
    }

    #[test]
    fn planning_writes_nothing_and_runs_nothing() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tempfile::tempdir().unwrap();
        let before_home: Vec<_> = std::fs::read_dir(tmp.path()).unwrap().collect();
        let before_root: Vec<_> = std::fs::read_dir(root.path()).unwrap().collect();
        let p = plan(
            &Paths::for_root(tmp.path()),
            Path::new("/usr/local/bin/nightjar"),
            root.path(),
        )
        .unwrap();
        let after_home: Vec<_> = std::fs::read_dir(tmp.path()).unwrap().collect();
        let after_root: Vec<_> = std::fs::read_dir(root.path()).unwrap().collect();
        assert_eq!(
            before_home.len(),
            after_home.len(),
            "plan() must not touch NIGHTJAR_HOME"
        );
        assert_eq!(
            before_root.len(),
            after_root.len(),
            "plan() must not touch the install root"
        );
        assert!(
            !p.unit_path.exists(),
            "plan() must not create the unit file"
        );
    }

    #[test]
    fn launchd_plist_starts_at_login() {
        let p = plist();
        assert!(p.contains("<key>RunAtLoad</key>\n    <true/>"), "got: {p}");
    }

    #[test]
    fn systemd_unit_is_wanted_by_the_default_target() {
        let u = unit();
        assert!(u.contains("WantedBy=default.target"), "got: {u}");
    }

    #[test]
    fn both_units_log_to_the_exact_path_they_are_given() {
        assert!(
            plist().contains(&format!("<string>{LOG}</string>")),
            "got: {}",
            plist()
        );
        let u = unit();
        assert!(u.contains(&format!("append:{LOG}")), "got: {u}");
    }

    #[test]
    fn log_path_sits_under_the_data_directory() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = Paths::for_root(tmp.path());
        assert_eq!(log_path(&paths), paths.data_dir.join("nightjar.log"));
    }

    #[test]
    fn unit_path_is_rooted_at_the_injected_install_root_not_at_nightjar_home() {
        let home = tempfile::tempdir().unwrap();
        let root = tempfile::tempdir().unwrap();
        let p = plan(
            &Paths::for_root(home.path()),
            Path::new("/usr/local/bin/nightjar"),
            root.path(),
        )
        .unwrap();
        assert!(
            p.unit_path.starts_with(root.path()),
            "unit_path {:?} must be under the injected install root {:?}",
            p.unit_path,
            root.path()
        );
        assert!(
            !p.unit_path.starts_with(home.path()),
            "unit_path {:?} must not derive from NIGHTJAR_HOME {:?}",
            p.unit_path,
            home.path()
        );
    }

    #[test]
    fn install_root_from_override_wins_over_everything() {
        let tmp = tempfile::tempdir().unwrap();
        let root = install_root_from(
            Some(OsStr::new(tmp.path().to_str().unwrap())),
            Some(OsStr::new("/other/config")),
            Some(OsStr::new("/other/home")),
        )
        .unwrap();
        assert_eq!(root, tmp.path());
    }

    #[test]
    fn install_root_from_treats_the_override_as_unset_when_it_is_empty() {
        let home = tempfile::tempdir().unwrap();
        let root =
            install_root_from(Some(OsStr::new("")), None, Some(home.path().as_os_str())).unwrap();
        assert_ne!(root, PathBuf::from(""));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn install_root_from_defaults_to_launchagents_under_home() {
        let home = tempfile::tempdir().unwrap();
        let root = install_root_from(None, None, Some(home.path().as_os_str())).unwrap();
        assert_eq!(root, home.path().join("Library/LaunchAgents"));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn install_root_from_errors_when_home_is_not_set() {
        let err = install_root_from(None, None, None).unwrap_err();
        assert!(err.to_string().contains("HOME"));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn install_root_from_prefers_xdg_config_home_when_it_is_absolute() {
        let home = tempfile::tempdir().unwrap();
        let xdg = tempfile::tempdir().unwrap();
        let root = install_root_from(
            None,
            Some(xdg.path().as_os_str()),
            Some(home.path().as_os_str()),
        )
        .unwrap();
        assert_eq!(root, xdg.path().join("systemd/user"));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn install_root_from_falls_back_to_home_when_xdg_config_home_is_relative() {
        let home = tempfile::tempdir().unwrap();
        let root = install_root_from(
            None,
            Some(OsStr::new("relative")),
            Some(home.path().as_os_str()),
        )
        .unwrap();
        assert_eq!(root, home.path().join(".config/systemd/user"));
    }
}
