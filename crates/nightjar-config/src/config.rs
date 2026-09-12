use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use nightjar_core::limits::MAX_SLEEP;
use nightjar_core::paths::Paths;
use serde::Deserialize;

use super::job::{Job, parse_duration};

const MIN_HEARTBEAT: Duration = Duration::from_secs(1);

const MAX_CATCHUP: usize = 100;

const MAX_QUEUE_DEPTH: usize = 100;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Config {
    pub heartbeat_interval: Duration,
    pub retention_runs: usize,
    pub retention_age: Duration,
    pub output_cap: u64,
    pub shell: Option<String>,
    pub login_shell: bool,
    pub catchup_max: usize,
    pub queue_depth: usize,
    pub secrets_resolver: Option<String>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            heartbeat_interval: Duration::from_secs(30),
            retention_runs: 50,
            retention_age: Duration::from_secs(90 * 86_400),
            output_cap: 10 * 1024 * 1024,
            shell: None,
            login_shell: true,
            catchup_max: 10,
            queue_depth: 1,
            secrets_resolver: None,
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawConfig {
    heartbeat_interval: Option<String>,
    retention_runs: Option<usize>,
    retention_age: Option<String>,
    output_cap: Option<String>,
    shell: Option<String>,
    login_shell: Option<bool>,
    catchup_max: Option<usize>,
    queue_depth: Option<usize>,
    #[serde(default)]
    secrets: RawSecretsConfig,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawSecretsConfig {
    #[serde(default)]
    resolver: Option<String>,
}

const UNITS: [(&str, u64); 4] = [
    ("GB", 1024 * 1024 * 1024),
    ("MB", 1024 * 1024),
    ("KB", 1024),
    ("B", 1),
];

pub(crate) fn parse_size(text: &str) -> Result<u64> {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        bail!("invalid size {text:?}: expected a number optionally followed by KB, MB, or GB");
    }
    let upper = trimmed.to_ascii_uppercase();
    let (digits, bytes_per_unit): (&str, u64) = UNITS
        .iter()
        .find_map(|&(suffix, size)| upper.strip_suffix(suffix).map(|n| (n, size)))
        .unwrap_or((upper.as_str(), 1));
    let count: u64 = digits
        .trim()
        .parse()
        .with_context(|| format!("invalid size {text:?}: {:?} is not a number", digits.trim()))?;
    count
        .checked_mul(bytes_per_unit)
        .ok_or_else(|| anyhow::anyhow!("invalid size {text:?}: value too large"))
}

impl Config {
    /// # Errors
    /// fails if config.toml is unreadable, is not valid toml, or holds a value out of range
    pub fn load(paths: &Paths) -> Result<Self> {
        let path = paths.config_dir.join("config.toml");
        let text = match std::fs::read_to_string(&path) {
            Ok(text) => text,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Self::default()),
            Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
        };
        Self::from_toml(&text, &path)
    }

    fn from_toml(text: &str, path: &Path) -> Result<Self> {
        let raw: RawConfig =
            toml::from_str(text).map_err(|e| anyhow::anyhow!("parsing {}: {e}", path.display()))?;

        let mut cfg = Self::default();
        if let Some(written) = raw.heartbeat_interval {
            let interval = parse_duration(&written)
                .map_err(|e| anyhow::anyhow!("{}: heartbeat_interval: {e}", path.display()))?;
            if interval < MIN_HEARTBEAT || interval > MAX_SLEEP {
                bail!(
                    "{}: heartbeat_interval: {written:?} is outside {}s..={}s; below that the \
                     daemon spins instead of sleeping, above it the loop's own ceiling applies \
                     anyway",
                    path.display(),
                    MIN_HEARTBEAT.as_secs(),
                    MAX_SLEEP.as_secs()
                );
            }
            cfg.heartbeat_interval = interval;
        }
        if let Some(runs) = raw.retention_runs {
            if runs == 0 {
                bail!(
                    "{}: retention_runs: 0 would delete every run the moment it finishes, \
                     leaving nothing for `status`, `logs`, or an `after` chain to read; \
                     use 1 or more",
                    path.display()
                );
            }
            cfg.retention_runs = runs;
        }
        if let Some(written) = raw.retention_age {
            let age = parse_duration(&written)
                .map_err(|e| anyhow::anyhow!("{}: retention_age: {e}", path.display()))?;
            if age.is_zero() {
                bail!(
                    "{}: retention_age: {written:?} would delete every run the moment it \
                     finishes; use a positive duration",
                    path.display()
                );
            }
            cfg.retention_age = age;
        }
        if let Some(written) = raw.output_cap {
            cfg.output_cap = parse_size(&written)
                .map_err(|e| anyhow::anyhow!("{}: output_cap: {e}", path.display()))?;
        }
        if let Some(shell) = raw.shell {
            cfg.shell = if shell.is_empty() { None } else { Some(shell) };
        }
        if let Some(login_shell) = raw.login_shell {
            cfg.login_shell = login_shell;
        }
        if let Some(max) = raw.catchup_max {
            if !(1..=MAX_CATCHUP).contains(&max) {
                bail!(
                    "{}: catchup_max: {max} is outside 1..={MAX_CATCHUP}; it is a simultaneous \
                     fork count under `overlap = \"parallel\"`, so set `catchup = \"none\"` on a \
                     job that should never be made up",
                    path.display()
                );
            }
            cfg.catchup_max = max;
        }
        if let Some(depth) = raw.queue_depth {
            if !(1..=MAX_QUEUE_DEPTH).contains(&depth) {
                bail!(
                    "{}: queue_depth: {depth} is outside 1..={MAX_QUEUE_DEPTH}; it caps how \
                     many firings may wait behind a job that is still running",
                    path.display()
                );
            }
            cfg.queue_depth = depth;
        }
        if let Some(resolver) = raw.secrets.resolver {
            if resolver.is_empty() {
                cfg.secrets_resolver = None;
            } else if resolver.contains("{}") {
                cfg.secrets_resolver = Some(resolver);
            } else {
                bail!(
                    "{}: secrets.resolver: {resolver:?} must contain \"{{}}\" as a placeholder \
                     for the secret's own location",
                    path.display()
                );
            }
        }
        Ok(cfg)
    }

    pub fn apply_defaults(&self, job: &mut Job) {
        if job.shell.is_none() {
            job.shell.clone_from(&self.shell);
        }
        if job.login_shell.is_none() {
            job.login_shell = Some(self.login_shell);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_yields_every_default_when_file_is_absent() {
        let tmp = tempfile::tempdir().unwrap();
        let c = Config::load(&Paths::for_root(tmp.path())).unwrap();
        assert_eq!(c.retention_runs, 50);
        assert_eq!(c.catchup_max, 10);
        assert_eq!(c, Config::default());
    }

    #[test]
    fn config_overrides_only_what_it_names_when_it_is_partial() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("config.toml"), "retention_runs = 5\n").unwrap();
        let c = Config::load(&Paths::for_root(tmp.path())).unwrap();
        assert_eq!(c.retention_runs, 5, "named key overridden");
        assert_eq!(c.catchup_max, 10, "unnamed key keeps its default");
    }

    #[test]
    fn key_is_rejected_by_name_when_it_is_unknown() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("config.toml"), "retention_runz = 5\n").unwrap();
        let err = Config::load(&Paths::for_root(tmp.path()))
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("retention_runz"),
            "a typo must be named: {err}"
        );
    }

    #[test]
    fn config_names_the_file_and_the_line_when_malformed() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("config.toml"), "retention_runs = = 5\n").unwrap();
        let err = Config::load(&Paths::for_root(tmp.path()))
            .unwrap_err()
            .to_string();
        assert!(err.contains("config.toml"), "got: {err}");
    }

    #[test]
    fn every_key_can_be_overridden_at_once() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(
            tmp.path().join("config.toml"),
            r#"
heartbeat_interval = "10s"
retention_runs     = 5
retention_age      = "7d"
output_cap         = "1MB"
shell              = "/bin/zsh"
login_shell        = false
catchup_max        = 3
queue_depth        = 4
"#,
        )
        .unwrap();
        let c = Config::load(&Paths::for_root(tmp.path())).unwrap();
        assert_eq!(c.heartbeat_interval, Duration::from_secs(10));
        assert_eq!(c.retention_runs, 5);
        assert_eq!(c.retention_age, Duration::from_secs(7 * 86_400));
        assert_eq!(c.output_cap, 1024 * 1024);
        assert_eq!(c.shell.as_deref(), Some("/bin/zsh"));
        assert!(!c.login_shell);
        assert_eq!(c.catchup_max, 3);
        assert_eq!(c.queue_depth, 4);
    }

    #[test]
    fn retention_runs_is_rejected_when_it_is_zero() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("config.toml"), "retention_runs = 0\n").unwrap();
        let err = Config::load(&Paths::for_root(tmp.path()))
            .unwrap_err()
            .to_string();
        assert!(err.contains("retention_runs"), "got: {err}");
    }

    #[test]
    fn retention_age_is_rejected_when_it_is_zero() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("config.toml"), "retention_age = \"0s\"\n").unwrap();
        let err = Config::load(&Paths::for_root(tmp.path()))
            .unwrap_err()
            .to_string();
        assert!(err.contains("retention_age"), "got: {err}");
    }

    #[test]
    fn queue_depth_defaults_to_one() {
        let tmp = tempfile::tempdir().unwrap();
        let c = Config::load(&Paths::for_root(tmp.path())).unwrap();
        assert_eq!(c.queue_depth, 1);
    }

    #[test]
    fn queue_depth_is_accepted_and_acted_on() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("config.toml"), "queue_depth = 5\n").unwrap();
        let c = Config::load(&Paths::for_root(tmp.path())).unwrap();
        assert_eq!(c.queue_depth, 5);
    }

    #[test]
    fn queue_depth_is_rejected_when_it_is_zero() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("config.toml"), "queue_depth = 0\n").unwrap();
        let err = Config::load(&Paths::for_root(tmp.path()))
            .unwrap_err()
            .to_string();
        assert!(err.contains("queue_depth"), "got: {err}");
    }

    #[test]
    fn queue_depth_is_rejected_when_past_the_ceiling() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("config.toml"), "queue_depth = 101\n").unwrap();
        let err = Config::load(&Paths::for_root(tmp.path()))
            .unwrap_err()
            .to_string();
        assert!(err.contains("queue_depth"), "got: {err}");
    }

    #[test]
    fn heartbeat_interval_is_rejected_when_the_daemon_cannot_sleep_for_it() {
        for value in ["0s", "60s"] {
            let tmp = tempfile::tempdir().unwrap();
            std::fs::write(
                tmp.path().join("config.toml"),
                format!("heartbeat_interval = \"{value}\"\n"),
            )
            .unwrap();
            let err = Config::load(&Paths::for_root(tmp.path()))
                .unwrap_err()
                .to_string();
            assert!(err.contains("heartbeat_interval"), "{value}: {err}");
        }
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(
            tmp.path().join("config.toml"),
            "heartbeat_interval = \"1s\"\n",
        )
        .unwrap();
        assert_eq!(
            Config::load(&Paths::for_root(tmp.path()))
                .unwrap()
                .heartbeat_interval,
            Duration::from_secs(1),
            "the lower bound itself must be accepted"
        );
    }

    #[test]
    fn catchup_max_is_rejected_when_it_would_fork_without_limit() {
        for value in [0usize, MAX_CATCHUP + 1] {
            let tmp = tempfile::tempdir().unwrap();
            std::fs::write(
                tmp.path().join("config.toml"),
                format!("catchup_max = {value}\n"),
            )
            .unwrap();
            let err = Config::load(&Paths::for_root(tmp.path()))
                .unwrap_err()
                .to_string();
            assert!(err.contains("catchup_max"), "{value}: {err}");
        }
    }

    #[test]
    fn global_shell_defaults_fill_in_only_what_a_job_left_unset() {
        let cfg = Config {
            shell: Some("/bin/bash".into()),
            login_shell: false,
            ..Config::default()
        };

        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("a.toml");

        std::fs::write(&path, "command = \"true\"\nschedule = \"hourly\"\n").unwrap();
        let mut unset = Job::load(&path).unwrap();
        cfg.apply_defaults(&mut unset);
        assert_eq!(unset.shell.as_deref(), Some("/bin/bash"));
        assert_eq!(unset.login_shell, Some(false));

        std::fs::write(
            &path,
            "command = \"true\"\nschedule = \"hourly\"\nshell = \"/bin/zsh\"\nlogin_shell = true\n",
        )
        .unwrap();
        let mut named = Job::load(&path).unwrap();
        cfg.apply_defaults(&mut named);
        assert_eq!(named.shell.as_deref(), Some("/bin/zsh"));
        assert_eq!(named.login_shell, Some(true));
    }

    #[test]
    fn shell_means_use_the_environment_default_when_it_is_an_empty_string() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("config.toml"), "shell = \"\"\n").unwrap();
        let c = Config::load(&Paths::for_root(tmp.path())).unwrap();
        assert_eq!(c.shell, None);
    }

    #[test]
    fn output_cap_accepts_binary_suffixes() {
        assert_eq!(parse_size("10MB").unwrap(), 10 * 1024 * 1024);
        assert_eq!(parse_size("512KB").unwrap(), 512 * 1024);
        assert_eq!(parse_size("1GB").unwrap(), 1024 * 1024 * 1024);
        assert_eq!(parse_size("100").unwrap(), 100);
        assert_eq!(parse_size("100B").unwrap(), 100);
        assert!(parse_size("").is_err());
        assert!(parse_size("abc").is_err());
    }

    #[test]
    fn secrets_resolver_is_absent_by_default() {
        let tmp = tempfile::tempdir().unwrap();
        let c = Config::load(&Paths::for_root(tmp.path())).unwrap();
        assert_eq!(c.secrets_resolver, None);
    }

    #[test]
    fn secrets_resolver_is_loaded_when_configured() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(
            tmp.path().join("config.toml"),
            "[secrets]\nresolver = \"op read {}\"\n",
        )
        .unwrap();
        let c = Config::load(&Paths::for_root(tmp.path())).unwrap();
        assert_eq!(c.secrets_resolver.as_deref(), Some("op read {}"));
    }

    #[test]
    fn secrets_resolver_is_rejected_when_it_has_no_placeholder() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(
            tmp.path().join("config.toml"),
            "[secrets]\nresolver = \"op read\"\n",
        )
        .unwrap();
        let err = Config::load(&Paths::for_root(tmp.path()))
            .unwrap_err()
            .to_string();
        assert!(err.contains("secrets.resolver"), "got: {err}");
        assert!(err.contains("{}"), "got: {err}");
    }

    #[test]
    fn secrets_resolver_means_unset_when_it_is_an_empty_string() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(
            tmp.path().join("config.toml"),
            "[secrets]\nresolver = \"\"\n",
        )
        .unwrap();
        let c = Config::load(&Paths::for_root(tmp.path())).unwrap();
        assert_eq!(c.secrets_resolver, None);
    }

    #[test]
    fn key_is_rejected_by_name_when_it_is_unknown_and_inside_secrets() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(
            tmp.path().join("config.toml"),
            "[secrets]\nresolverr = \"op read {}\"\n",
        )
        .unwrap();
        let err = Config::load(&Paths::for_root(tmp.path()))
            .unwrap_err()
            .to_string();
        assert!(err.contains("resolverr"), "got: {err}");
    }

    #[test]
    fn typo_is_still_rejected_when_other_keys_in_the_config_are_valid() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(
            tmp.path().join("config.toml"),
            "retention_runs = 5\ncatchup_maxx = 3\n",
        )
        .unwrap();
        let err = Config::load(&Paths::for_root(tmp.path()))
            .unwrap_err()
            .to_string();
        assert!(err.contains("catchup_maxx"), "got: {err}");
    }
}
