use anyhow::{Context, Result, bail};
use std::ffi::OsStr;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone)]
pub struct Paths {
    pub config_dir: PathBuf,
    pub jobs_dir: PathBuf,
    pub data_dir: PathBuf,
    pub runs_dir: PathBuf,
    pub db_path: PathBuf,
    pub lock_path: PathBuf,
}

impl Paths {
    pub fn for_root(root: &Path) -> Self {
        Self {
            config_dir: root.to_path_buf(),
            jobs_dir: root.join("jobs"),
            data_dir: root.to_path_buf(),
            runs_dir: root.join("runs"),
            db_path: root.join("nightjar.db"),
            lock_path: root.join("daemon.lock"),
        }
    }

    /// Unlike the XDG vars, a relative `NIGHTJAR_HOME` isn't treated as
    /// unset. It anchors to `cwd` instead — see below for why.
    pub fn resolve_from(
        nightjar_home: Option<&OsStr>,
        xdg_config: Option<&OsStr>,
        xdg_data: Option<&OsStr>,
        home: Option<&OsStr>,
        cwd: &Path,
    ) -> Result<Self> {
        if let Some(root) = nightjar_home {
            if !root.is_empty() {
                let p = Path::new(root);
                // A relative `runs_dir` would defeat `is_within_runs_dir`'s
                // containment check and silently disable retention. Anchor to
                // `cwd`, not the XDG default, so this still works here.
                let root = if p.is_absolute() {
                    p.to_path_buf()
                } else {
                    cwd.join(p)
                };
                return Ok(Self::for_root(&root));
            }
        }

        let home_path = home.filter(|h| !h.is_empty()).context("HOME is not set")?;
        let home = PathBuf::from(home_path);

        let config_dir = match xdg_config {
            Some(x) if !x.is_empty() => {
                let p = PathBuf::from(x);
                if p.is_absolute() {
                    p.join("nightjar")
                } else {
                    home.join(".config/nightjar")
                }
            }
            _ => home.join(".config/nightjar"),
        };
        let data_dir = match xdg_data {
            Some(x) if !x.is_empty() => {
                let p = PathBuf::from(x);
                if p.is_absolute() {
                    p.join("nightjar")
                } else {
                    home.join(".local/share/nightjar")
                }
            }
            _ => home.join(".local/share/nightjar"),
        };

        Ok(Self {
            jobs_dir: config_dir.join("jobs"),
            runs_dir: data_dir.join("runs"),
            db_path: data_dir.join("nightjar.db"),
            lock_path: data_dir.join("daemon.lock"),
            config_dir,
            data_dir,
        })
    }

    pub fn resolve() -> Result<Self> {
        let cwd = std::env::current_dir()
            .context("resolving the current working directory (needed to make a relative NIGHTJAR_HOME absolute)")?;
        Self::resolve_from(
            std::env::var_os("NIGHTJAR_HOME").as_deref(),
            std::env::var_os("XDG_CONFIG_HOME").as_deref(),
            std::env::var_os("XDG_DATA_HOME").as_deref(),
            std::env::var_os("HOME").as_deref(),
            &cwd,
        )
    }

    /// Directories this creates are private to the user (`0700`). Job
    /// files can hold `[env]` values and run output can hold anything a
    /// job printed, and on a shared machine a home directory is often
    /// world-readable. A directory that already exists is left as it is:
    /// its permissions are the user's choice, not this tool's.
    pub fn ensure_dirs(&self) -> Result<()> {
        for d in [
            &self.config_dir,
            &self.jobs_dir,
            &self.data_dir,
            &self.runs_dir,
        ] {
            create_private_dir(d)?;
        }
        Ok(())
    }

    pub fn run_output(&self, job: &str, run_id: &str) -> (PathBuf, PathBuf) {
        let dir = self.runs_dir.join(job);
        (
            dir.join(format!("{run_id}.out")),
            dir.join(format!("{run_id}.err")),
        )
    }

    pub fn job_file(&self, job: &str) -> Result<PathBuf> {
        validate_job_name(job)?;
        Ok(self.jobs_dir.join(format!("{job}.toml")))
    }
}

/// Creates `dir` with mode `0700` if it does not exist. Parents that are
/// missing are created with the default mode: `~/.config` or
/// `~/.local/share` are shared with every other tool and not this one's
/// to lock down.
fn create_private_dir(dir: &Path) -> Result<()> {
    use std::os::unix::fs::DirBuilderExt;

    if dir.is_dir() {
        return Ok(());
    }
    if let Some(parent) = dir.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    match std::fs::DirBuilder::new().mode(0o700).create(dir) {
        Ok(()) => Ok(()),
        // Lost a race with another nightjar process creating the same dir.
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists && dir.is_dir() => Ok(()),
        Err(e) => Err(e).with_context(|| format!("creating {}", dir.display())),
    }
}

/// A job name becomes part of a path inside `jobs_dir`. Unvalidated, a
/// name like `../../../etc/hosts` would escape it entirely.
pub fn validate_job_name(job: &str) -> Result<()> {
    if job.is_empty() {
        bail!("job name must not be empty");
    }
    if job.contains('/') || job.contains('\\') {
        bail!("invalid job name {job:?}: a job name is a filename, not a path");
    }
    if job.contains("..") {
        bail!("invalid job name {job:?}: must not contain \"..\"");
    }
    if let Some(c) = job.chars().find(|c| c.is_control()) {
        bail!("invalid job name {job:?}: must not contain the control character {c:?}");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nightjar_home_overrides_all_locations() {
        let tmp = tempfile::tempdir().unwrap();
        let p = Paths::for_root(tmp.path());

        assert_eq!(p.jobs_dir, tmp.path().join("jobs"));
        assert_eq!(p.db_path, tmp.path().join("nightjar.db"));
        assert_eq!(p.runs_dir, tmp.path().join("runs"));
        assert_eq!(p.lock_path, tmp.path().join("daemon.lock"));
    }

    #[test]
    fn ensure_dirs_creates_everything_and_is_idempotent() {
        let tmp = tempfile::tempdir().unwrap();
        let p = Paths::for_root(tmp.path());

        p.ensure_dirs().unwrap();
        assert!(p.jobs_dir.is_dir());
        assert!(p.runs_dir.is_dir());

        p.ensure_dirs().unwrap();
        assert!(p.jobs_dir.is_dir());
    }

    fn mode_of(path: &Path) -> u32 {
        use std::os::unix::fs::PermissionsExt;
        std::fs::metadata(path).unwrap().permissions().mode() & 0o777
    }

    #[test]
    fn ensure_dirs_creates_directories_only_the_user_can_read() {
        let tmp = tempfile::tempdir().unwrap();
        let p = Paths::for_root(&tmp.path().join("nested").join("home"));

        p.ensure_dirs().unwrap();

        for dir in [&p.config_dir, &p.jobs_dir, &p.data_dir, &p.runs_dir] {
            assert_eq!(mode_of(dir), 0o700, "{}", dir.display());
        }
        assert_ne!(
            mode_of(&tmp.path().join("nested")),
            0o700,
            "a shared parent directory is not nightjar's to lock down"
        );
    }

    #[test]
    fn ensure_dirs_leaves_an_existing_directory_permissions_alone() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        let p = Paths::for_root(tmp.path());
        std::fs::create_dir(&p.jobs_dir).unwrap();
        std::fs::set_permissions(&p.jobs_dir, std::fs::Permissions::from_mode(0o755)).unwrap();

        p.ensure_dirs().unwrap();

        assert_eq!(mode_of(&p.jobs_dir), 0o755);
    }

    #[test]
    fn run_output_paths_are_namespaced_by_job() {
        let tmp = tempfile::tempdir().unwrap();
        let p = Paths::for_root(tmp.path());
        let (out, err) = p.run_output("backup", "0192abc");

        assert_eq!(out, tmp.path().join("runs/backup/0192abc.out"));
        assert_eq!(err, tmp.path().join("runs/backup/0192abc.err"));
    }

    #[test]
    fn job_file_lands_inside_the_jobs_directory() {
        let tmp = tempfile::tempdir().unwrap();
        let p = Paths::for_root(tmp.path());

        assert_eq!(
            p.job_file("backup").unwrap(),
            tmp.path().join("jobs/backup.toml")
        );
    }

    #[test]
    fn job_file_rejects_names_that_escape_the_jobs_directory() {
        let tmp = tempfile::tempdir().unwrap();
        let p = Paths::for_root(tmp.path());

        for name in [
            "../../../etc/hosts",
            "..",
            "a/b",
            "sub/../../x",
            "",
            "nul\0byte",
            "line\nbreak",
            "tab\there",
        ] {
            let err = p.job_file(name).unwrap_err();
            assert!(
                err.to_string().contains("job name"),
                "name {name:?} gave: {err}"
            );
        }
    }

    #[test]
    fn resolve_from_wins_over_everything_when_nightjar_home_is_set() {
        let tmp = tempfile::tempdir().unwrap();
        let nh = tmp.path().to_str().unwrap();

        let p = Paths::resolve_from(
            Some(OsStr::new(nh)),
            Some(OsStr::new("/other/config")),
            Some(OsStr::new("/other/data")),
            Some(OsStr::new("/other/home")),
            Path::new("/unused-cwd"),
        )
        .unwrap();

        assert_eq!(p.jobs_dir, tmp.path().join("jobs"));
        assert_eq!(p.db_path, tmp.path().join("nightjar.db"));
        assert_eq!(p.runs_dir, tmp.path().join("runs"));
        assert_eq!(p.lock_path, tmp.path().join("daemon.lock"));
        assert_eq!(p.config_dir, tmp.path());
        assert_eq!(p.data_dir, tmp.path());
    }

    #[test]
    fn resolve_from_treats_nightjar_home_as_unset_when_it_is_empty() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().to_str().unwrap();

        let p = Paths::resolve_from(
            Some(OsStr::new("")),
            None,
            None,
            Some(OsStr::new(home)),
            Path::new("/unused-cwd"),
        )
        .unwrap();

        assert_eq!(p.config_dir, PathBuf::from(home).join(".config/nightjar"));
        assert_eq!(
            p.data_dir,
            PathBuf::from(home).join(".local/share/nightjar")
        );
    }

    #[test]
    fn resolve_from_anchors_nightjar_home_to_cwd_when_it_is_relative() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().join("home");
        let cwd = tmp.path().join("cwd");

        let p = Paths::resolve_from(
            Some(OsStr::new("relative-nightjar-home")),
            None,
            None,
            Some(home.as_os_str()),
            &cwd,
        )
        .unwrap();

        assert_eq!(
            p.config_dir,
            cwd.join("relative-nightjar-home"),
            "a relative NIGHTJAR_HOME must resolve against cwd, not the HOME default"
        );
        assert!(p.jobs_dir.is_absolute());
    }

    #[test]
    fn resolve_from_ignores_cwd_when_nightjar_home_is_absolute() {
        let tmp = tempfile::tempdir().unwrap();

        let p = Paths::resolve_from(
            Some(tmp.path().as_os_str()),
            None,
            None,
            None,
            Path::new("/should-not-matter"),
        )
        .unwrap();

        assert_eq!(p.config_dir, tmp.path());
    }

    #[test]
    fn resolve_from_joins_nightjar_onto_each_xdg_dir_when_both_are_set_and_absolute() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().join("home");
        let config = tmp.path().join("config");
        let data = tmp.path().join("data");

        let home_str = home.to_str().unwrap();
        let config_str = config.to_str().unwrap();
        let data_str = data.to_str().unwrap();

        let p = Paths::resolve_from(
            None,
            Some(OsStr::new(config_str)),
            Some(OsStr::new(data_str)),
            Some(OsStr::new(home_str)),
            Path::new("/unused-cwd"),
        )
        .unwrap();

        assert_eq!(p.config_dir, config.join("nightjar"));
        assert_eq!(p.data_dir, data.join("nightjar"));
        assert_ne!(p.config_dir.parent(), p.data_dir.parent());
    }

    #[test]
    fn resolve_from_treats_xdg_paths_as_unset_when_they_are_relative() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().to_str().unwrap();

        let p = Paths::resolve_from(
            None,
            Some(OsStr::new("relative/config")),
            Some(OsStr::new("relative/data")),
            Some(OsStr::new(home)),
            Path::new("/unused-cwd"),
        )
        .unwrap();

        assert_eq!(p.config_dir, PathBuf::from(home).join(".config/nightjar"));
        assert_eq!(
            p.data_dir,
            PathBuf::from(home).join(".local/share/nightjar")
        );
    }

    #[test]
    fn resolve_from_treats_xdg_paths_as_unset_when_they_are_empty() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().to_str().unwrap();

        let p = Paths::resolve_from(
            None,
            Some(OsStr::new("")),
            Some(OsStr::new("")),
            Some(OsStr::new(home)),
            Path::new("/unused-cwd"),
        )
        .unwrap();

        assert_eq!(p.config_dir, PathBuf::from(home).join(".config/nightjar"));
        assert_eq!(
            p.data_dir,
            PathBuf::from(home).join(".local/share/nightjar")
        );
    }

    #[test]
    fn resolve_from_returns_error_when_home_is_unset() {
        let err =
            Paths::resolve_from(None, None, None, None, Path::new("/unused-cwd")).unwrap_err();
        assert!(err.to_string().contains("HOME"));
    }

    #[test]
    fn resolve_from_returns_error_when_home_is_empty() {
        let err = Paths::resolve_from(
            None,
            None,
            None,
            Some(OsStr::new("")),
            Path::new("/unused-cwd"),
        )
        .unwrap_err();
        assert!(err.to_string().contains("HOME"));
    }

    #[test]
    fn resolve_from_falls_back_to_home_defaults_when_xdg_is_unset() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().to_str().unwrap();

        let p = Paths::resolve_from(
            None,
            None,
            None,
            Some(OsStr::new(home)),
            Path::new("/unused-cwd"),
        )
        .unwrap();

        assert_eq!(p.config_dir, PathBuf::from(home).join(".config/nightjar"));
        assert_eq!(
            p.data_dir,
            PathBuf::from(home).join(".local/share/nightjar")
        );
    }
}
