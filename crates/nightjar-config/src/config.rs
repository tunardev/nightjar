use anyhow::{Context, Result, bail};
use nightjar_core::limits::MAX_SLEEP;
use nightjar_core::paths::Paths;
use serde::Deserialize;
use std::time::Duration;

use super::job::{Job, parse_duration};

/// Below this, `sleep_for` returns zero. The tick loop then spins on `SQLite`.
const MIN_HEARTBEAT: Duration = Duration::from_secs(1);

/// Bounds simultaneous forks under `overlap = "parallel"` and the in-memory
/// window `plan_catch_up` holds.
const MAX_CATCHUP: usize = 100;

/// Rows held in `queued_runs` per job. Past this, an occurrence falls back
/// to `missed`, same as `overlap = "skip"`.
const MAX_QUEUE_DEPTH: usize = 100;

#[derive(Debug, Clone, PartialEq)]
pub struct Config {
    /// Also the daemon's idle-loop sleep ceiling.
    pub heartbeat_interval: Duration,
    pub retention_runs: usize,
    pub retention_age: Duration,
    pub output_cap: u64,
    /// `None` means "use $SHELL", matching the job-level field's default.
    pub shell: Option<String>,
    pub login_shell: bool,
    /// Cap on make-up runs per catch-up pass. Beyond it, occurrences are
    /// still recorded `missed`.
    pub catchup_max: usize,
    /// Max occurrences `overlap = "queue"` holds. Past this, it falls back
    /// to `missed`, same as `overlap = "skip"`.
    pub queue_depth: usize,
    /// Command template for fetching every job's `[secrets]`, e.g.
    /// `op read {}`. `None` means no job may declare secrets.
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

pub(crate) fn parse_size(s: &str) -> Result<u64> {
    let trimmed = s.trim();
    if trimmed.is_empty() {
        bail!("invalid size {s:?}: expected a number optionally followed by KB, MB, or GB");
    }
    let upper = trimmed.to_ascii_uppercase();
    let (num, mult): (&str, u64) = if let Some(n) = upper.strip_suffix("GB") {
        (n, 1024 * 1024 * 1024)
    } else if let Some(n) = upper.strip_suffix("MB") {
        (n, 1024 * 1024)
    } else if let Some(n) = upper.strip_suffix("KB") {
        (n, 1024)
    } else if let Some(n) = upper.strip_suffix('B') {
        (n, 1)
    } else {
        (upper.as_str(), 1)
    };
    let n: u64 = num
        .trim()
        .parse()
        .with_context(|| format!("invalid size {s:?}: {:?} is not a number", num.trim()))?;
    n.checked_mul(mult)
        .ok_or_else(|| anyhow::anyhow!("invalid size {s:?}: value too large"))
}

impl Config {
    pub fn load(paths: &Paths) -> Result<Self> {
        let path = paths.config_dir.join("config.toml");
        let text = match std::fs::read_to_string(&path) {
            Ok(t) => t,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Self::default()),
            Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
        };

        // `.with_context()` would hide toml's own message behind anyhow's
        // non-alternate `Display`. See the same note on `Job::load`'s
        // schedule error.
        let raw: RawConfig = toml::from_str(&text)
            .map_err(|e| anyhow::anyhow!("parsing {}: {e}", path.display()))?;

        let mut cfg = Self::default();
        if let Some(v) = raw.heartbeat_interval {
            let d = parse_duration(&v)
                .map_err(|e| anyhow::anyhow!("{}: heartbeat_interval: {e}", path.display()))?;
            if d < MIN_HEARTBEAT || d > MAX_SLEEP {
                bail!(
                    "{}: heartbeat_interval: {v:?} is outside {}s..={}s; below that the daemon \
                     spins instead of sleeping, above it the loop's own ceiling applies anyway",
                    path.display(),
                    MIN_HEARTBEAT.as_secs(),
                    MAX_SLEEP.as_secs()
                );
            }
            cfg.heartbeat_interval = d;
        }
        if let Some(v) = raw.retention_runs {
            if v == 0 {
                bail!(
                    "{}: retention_runs: 0 would delete every run the moment it finishes, \
                     leaving nothing for `status`, `logs`, or an `after` chain to read; \
                     use 1 or more",
                    path.display()
                );
            }
            cfg.retention_runs = v;
        }
        if let Some(v) = raw.retention_age {
            let d = parse_duration(&v)
                .map_err(|e| anyhow::anyhow!("{}: retention_age: {e}", path.display()))?;
            if d.is_zero() {
                bail!(
                    "{}: retention_age: {v:?} would delete every run the moment it finishes; \
                     use a positive duration",
                    path.display()
                );
            }
            cfg.retention_age = d;
        }
        if let Some(v) = raw.output_cap {
            cfg.output_cap = parse_size(&v)
                .map_err(|e| anyhow::anyhow!("{}: output_cap: {e}", path.display()))?;
        }
        if let Some(v) = raw.shell {
            cfg.shell = if v.is_empty() { None } else { Some(v) };
        }
        if let Some(v) = raw.login_shell {
            cfg.login_shell = v;
        }
        if let Some(v) = raw.catchup_max {
            if !(1..=MAX_CATCHUP).contains(&v) {
                bail!(
                    "{}: catchup_max: {v} is outside 1..={MAX_CATCHUP}; it is a simultaneous \
                     fork count under `overlap = \"parallel\"`. Use `catchup = \"none\"` on a \
                     job that should never be made up",
                    path.display()
                );
            }
            cfg.catchup_max = v;
        }
        if let Some(v) = raw.queue_depth {
            if !(1..=MAX_QUEUE_DEPTH).contains(&v) {
                bail!(
                    "{}: queue_depth: {v} is outside 1..={MAX_QUEUE_DEPTH}",
                    path.display()
                );
            }
            cfg.queue_depth = v;
        }
        if let Some(v) = raw.secrets.resolver {
            if v.is_empty() {
                cfg.secrets_resolver = None;
            } else if v.contains("{}") {
                cfg.secrets_resolver = Some(v);
            } else {
                bail!(
                    "{}: secrets.resolver: {v:?} must contain \"{{}}\" as a placeholder \
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
