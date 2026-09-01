use anyhow::{Context, Result, bail};
pub use nightjar_core::limits::Limits;
use serde::Deserialize;
use std::collections::{BTreeMap, HashSet};
use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// Longer than any real job, and far inside what a wall-clock deadline
/// can represent; past it, a run would record `unknown` every time
/// instead of ever starting.
const MAX_TIMEOUT: Duration = Duration::from_secs(365 * 86_400);

/// A chain longer than this is a load error. Each job has at most one
/// parent, so a chain's depth is the only thing bounding it.
const MAX_AFTER_DEPTH: usize = 32;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Catchup {
    None,
    Once,
    All,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Overlap {
    Skip,
    Queue,
    Parallel,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OnFailure {
    #[serde(default)]
    pub notify: bool,
    #[serde(default)]
    pub run: Option<String>,
    #[serde(default)]
    pub webhook: Option<String>,
}

impl OnFailure {
    pub fn has_channel(&self) -> bool {
        self.notify || self.run.is_some() || self.webhook.is_some()
    }
}

/// Resource ceilings applied to the job's own process group, never to the
/// daemon.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RawLimits {
    #[serde(default)]
    memory: Option<String>,
    #[serde(default)]
    cpu_time: Option<String>,
    #[serde(default)]
    processes: Option<u64>,
    #[serde(default)]
    files: Option<u64>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawJob {
    command: String,
    #[serde(default)]
    schedule: Option<String>,
    #[serde(default)]
    after: Option<Vec<String>>,
    #[serde(default)]
    timeout: Option<String>,
    #[serde(default = "default_catchup")]
    catchup: Catchup,
    #[serde(default = "default_overlap")]
    overlap: Overlap,
    #[serde(default)]
    workdir: Option<PathBuf>,
    #[serde(default = "default_true")]
    enabled: bool,
    #[serde(default)]
    shell: Option<String>,
    #[serde(default)]
    login_shell: Option<bool>,
    #[serde(default)]
    env: BTreeMap<String, String>,
    #[serde(default)]
    secrets: BTreeMap<String, String>,
    #[serde(default)]
    on_failure: OnFailure,
    #[serde(default)]
    limits: RawLimits,
}

fn default_catchup() -> Catchup {
    Catchup::Once
}
fn default_overlap() -> Overlap {
    Overlap::Skip
}
fn default_true() -> bool {
    true
}

#[derive(Debug, Clone)]
pub struct Job {
    pub name: String,
    pub command: String,
    /// Exactly one of `schedule`/`after` is ever set. Enforced at load, not
    /// by the type.
    pub schedule: Option<nightjar_schedule::Schedule>,
    /// The single parent. The job file's `after` is a list, but more than
    /// one entry is a load error.
    pub after: Option<String>,
    pub timeout: Option<Duration>,
    pub catchup: Catchup,
    pub overlap: Overlap,
    pub workdir: Option<PathBuf>,
    pub enabled: bool,
    pub shell: Option<String>,
    /// `None` until `Config::apply_defaults` resolves it: job file, then
    /// `config.toml`, then the built-in default.
    pub login_shell: Option<bool>,
    pub env: BTreeMap<String, String>,
    /// Env var name -> the secret's location, e.g. `op://vault/db/password`.
    /// Resolved only at run time (`runner::exec::execute`). `Job::load` has
    /// no `Config` to resolve against.
    pub secrets: BTreeMap<String, String>,
    pub on_failure: OnFailure,
    pub limits: Limits,
    /// Loaded fine, but worth telling the user about: today, an `after`
    /// parent that is disabled. Kept as data so every caller decides
    /// where it goes. A library printing to stderr would corrupt the TUI
    /// and repeat itself on every daemon tick.
    pub warnings: Vec<String>,
}

pub fn parse_duration(s: &str) -> Result<Duration> {
    let s = s.trim();
    if s.len() < 2 {
        bail!("invalid duration {s:?}: expected a number followed by s, m, h, or d");
    }
    if !s.is_ascii() {
        bail!("invalid duration {s:?}: must contain only ASCII characters");
    }
    let (num, unit) = s.split_at(s.len() - 1);
    let n: u64 = num
        .parse()
        .with_context(|| format!("invalid duration {s:?}: {num:?} is not a number"))?;
    let secs = match unit {
        "s" => n,
        "m" => n
            .checked_mul(60)
            .ok_or_else(|| anyhow::anyhow!("invalid duration {s:?}: value too large"))?,
        "h" => n
            .checked_mul(3600)
            .ok_or_else(|| anyhow::anyhow!("invalid duration {s:?}: value too large"))?,
        "d" => n
            .checked_mul(86400)
            .ok_or_else(|| anyhow::anyhow!("invalid duration {s:?}: value too large"))?,
        _ => bail!("invalid duration {s:?}: unit must be s, m, h, or d"),
    };
    Ok(Duration::from_secs(secs))
}

/// Nothing else in the job file goes through a shell, so this is the only
/// place `~` gets expanded.
fn expand_tilde(path: &Path, home: Option<&OsStr>) -> Result<PathBuf> {
    let Ok(rest) = path.strip_prefix("~") else {
        return Ok(path.to_path_buf());
    };
    let home = home
        .filter(|h| !h.is_empty())
        .with_context(|| format!("cannot expand {} because HOME is not set", path.display()))?;
    Ok(PathBuf::from(home).join(rest))
}

/// Whether `shell` swallows `SIGXCPU`, the kernel's signal for an
/// `RLIMIT_CPU` breach. `zsh` swallows it. `sh` and `bash` don't.
fn shell_defeats_cpu_limit(shell: &str) -> bool {
    std::path::Path::new(shell)
        .file_name()
        .and_then(|f| f.to_str())
        .is_some_and(|f| f == "zsh")
}

fn parse_limits(name: &str, raw: &RawLimits, shell: Option<&str>) -> Result<Limits> {
    // `setrlimit` returns EINVAL for `RLIMIT_AS` (and `RLIMIT_DATA`) on
    // macOS. The kernel never enforces it there.
    #[cfg(target_os = "macos")]
    if raw.memory.is_some() {
        bail!(
            "job {name:?}: limits.memory is not supported on macOS — the kernel \
             refuses RLIMIT_AS (EINVAL), so nothing would enforce it. Remove the \
             key, or cap the job from inside its own command"
        );
    }

    let memory = match raw.memory.as_deref() {
        Some(v) => Some(
            crate::config::parse_size(v)
                .map_err(|e| anyhow::anyhow!("job {name:?}: limits.memory: {e}"))?,
        ),
        None => None,
    };
    let cpu_time = match raw.cpu_time.as_deref() {
        Some(v) => Some(
            parse_duration(v)
                .map_err(|e| anyhow::anyhow!("job {name:?}: limits.cpu_time: {e}"))?
                .as_secs(),
        ),
        None => None,
    };

    // `RLIMIT_NPROC = 0` cannot fork at all. `RLIMIT_NOFILE = 0` cannot open
    // the capture files.
    for (field, value) in [
        ("memory", memory),
        ("cpu_time", cpu_time),
        ("processes", raw.processes),
        ("files", raw.files),
    ] {
        if value == Some(0) {
            bail!("job {name:?}: limits.{field}: 0 would stop the job from running at all");
        }
    }

    // Mirrors `runner::exec::shell_for`'s own resolution, so the check
    // matches what the job actually runs under.
    if cpu_time.is_some() {
        let resolved_shell = shell
            .map(str::to_string)
            .or_else(|| std::env::var("SHELL").ok())
            .unwrap_or_else(|| "/bin/sh".to_string());
        if shell_defeats_cpu_limit(&resolved_shell) {
            bail!(
                "job {name:?}: limits.cpu_time cannot be enforced under {resolved_shell} — \
                 it swallows the SIGXCPU the kernel uses to signal the breach, so the \
                 job would run past its ceiling unchecked. Set `shell = \"/bin/sh\"` \
                 on this job (or `shell` in config.toml) to make the limit real"
            );
        }
    }

    Ok(Limits {
        memory,
        cpu_time,
        processes: raw.processes,
        files: raw.files,
    })
}

fn validate_after(name: &str, after: &[String]) -> Result<String> {
    if after.is_empty() {
        bail!("job {name:?}: after must name at least one job");
    }
    if after.len() > 1 {
        bail!(
            "job {name:?}: after names {} jobs; fan-in (waiting on more than one \
             parent) is permanently out of scope — for a workflow tool with join \
             semantics, see dagu",
            after.len()
        );
    }
    if after[0] == name {
        bail!("job {name:?}: after must not name itself");
    }
    Ok(after[0].clone())
}

fn validate_after_graph(entries: &mut [(String, Result<Job>)]) {
    let ok_names: HashSet<String> = entries
        .iter()
        .filter(|(_, r)| r.is_ok())
        .map(|(n, _)| n.clone())
        .collect();

    let names: Vec<String> = entries.iter().map(|(n, _)| n.clone()).collect();
    let mut errors: Vec<(String, String)> = Vec::new();
    let mut warnings: Vec<(String, String)> = Vec::new();

    for name in &names {
        let Some(Ok(job)) = entries.iter().find(|(n, _)| n == name).map(|(_, r)| r) else {
            continue;
        };
        let Some(parent) = &job.after else { continue };

        if !ok_names.contains(parent) {
            errors.push((
                name.clone(),
                format!(
                    "job {name:?}: after names {parent:?}, which does not exist or failed to load"
                ),
            ));
            continue;
        }
        if !entries
            .iter()
            .find(|(n, _)| n == parent)
            .and_then(|(_, r)| r.as_ref().ok())
            .is_some_and(|p| p.enabled)
        {
            warnings.push((
                name.clone(),
                format!(
                    "job {name:?}: after names {parent:?}, which is disabled — \
                     {name:?} will never fire until it is re-enabled"
                ),
            ));
        }

        // `chain` never includes the scheduled root. Pushing it would
        // overcount a chain of exactly `MAX_AFTER_DEPTH` by one and wrongly
        // reject it.
        let mut chain = vec![name.clone()];
        let mut current = parent.clone();
        loop {
            let Some(Ok(current_job)) = entries.iter().find(|(n, _)| n == &current).map(|(_, r)| r)
            else {
                errors.push((
                    name.clone(),
                    format!(
                        "job {name:?}: after chain includes {current:?}, which does not exist or failed to load"
                    ),
                ));
                break;
            };
            let Some(grandparent) = &current_job.after else {
                break;
            };
            if chain.contains(&current) {
                chain.push(current);
                errors.push((
                    name.clone(),
                    format!("job {name:?}: after forms a cycle: {}", chain.join(" -> ")),
                ));
                break;
            }
            chain.push(current.clone());
            if chain.len() > MAX_AFTER_DEPTH {
                errors.push((
                    name.clone(),
                    format!(
                        "job {name:?}: after chain is deeper than {MAX_AFTER_DEPTH} jobs: {}",
                        chain.join(" -> ")
                    ),
                ));
                break;
            }
            current.clone_from(grandparent);
        }
    }

    for (name, message) in errors {
        if let Some(entry) = entries.iter_mut().find(|(n, _)| n == &name) {
            entry.1 = Err(anyhow::anyhow!(message));
        }
    }
    for (name, message) in warnings {
        if let Some((_, Ok(job))) = entries.iter_mut().find(|(n, _)| n == &name) {
            job.warnings.push(message);
        }
    }
}

impl Job {
    pub fn load(path: &Path) -> Result<Job> {
        let name = path
            .file_stem()
            .and_then(|s| s.to_str())
            .with_context(|| format!("job file has no usable name: {}", path.display()))?
            .to_string();

        let text =
            std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
        let raw: RawJob =
            toml::from_str(&text).with_context(|| format!("parsing {}", path.display()))?;
        Self::from_raw(name, raw)
    }

    /// Every check `load` performs, on text that may not be on disk yet.
    /// `nightjar add` validates what it is about to write with this, so a
    /// bad `--timeout` is refused instead of saved as a job that can
    /// never load.
    pub fn from_toml_str(name: &str, text: &str) -> Result<Job> {
        let raw: RawJob = toml::from_str(text).with_context(|| format!("parsing job {name:?}"))?;
        Self::from_raw(name.to_string(), raw)
    }

    fn from_raw(name: String, raw: RawJob) -> Result<Job> {
        if raw.command.trim().is_empty() {
            bail!("job {name:?}: command must not be empty");
        }

        let after = match (&raw.schedule, &raw.after) {
            (Some(_), Some(_)) => bail!(
                "job {name:?}: schedule and after are mutually exclusive — \
                 a job is either scheduled or triggered, never both"
            ),
            (None, None) => bail!("job {name:?}: must set either schedule or after"),
            (Some(_), None) => None,
            (None, Some(after)) => Some(validate_after(&name, after)?),
        };

        let schedule = match raw.schedule.as_deref() {
            Some(s) if s.trim().is_empty() => bail!("job {name:?}: schedule must not be empty"),
            // `.with_context()` would hide the error's detail behind anyhow's
            // plain `Display`. That drops the context chain.
            Some(s) => Some(
                nightjar_schedule::Schedule::parse(s)
                    .map_err(|e| anyhow::anyhow!("job {name:?}: schedule: {e:#}"))?,
            ),
            None => None,
        };

        let timeout = match raw.timeout.as_deref() {
            Some(t) => {
                let d = parse_duration(t).with_context(|| format!("job {name:?}: timeout"))?;
                if d > MAX_TIMEOUT {
                    bail!(
                        "job {name:?}: timeout: {t:?} is longer than the {}-day maximum",
                        MAX_TIMEOUT.as_secs() / 86_400
                    );
                }
                Some(d)
            }
            None => None,
        };

        let workdir = match raw.workdir {
            Some(w) => Some(
                expand_tilde(&w, std::env::var_os("HOME").as_deref())
                    .with_context(|| format!("job {name:?}: workdir"))?,
            ),
            None => None,
        };

        let limits = parse_limits(&name, &raw.limits, raw.shell.as_deref())?;

        Ok(Job {
            name,
            command: raw.command,
            schedule,
            after,
            timeout,
            catchup: raw.catchup,
            overlap: raw.overlap,
            workdir,
            enabled: raw.enabled,
            shell: raw.shell,
            login_shell: raw.login_shell,
            env: raw.env,
            secrets: raw.secrets,
            on_failure: raw.on_failure,
            limits,
            warnings: Vec::new(),
        })
    }

    pub fn load_all(jobs_dir: &Path) -> Vec<(String, Result<Job>)> {
        let Ok(entries) = std::fs::read_dir(jobs_dir) else {
            return Vec::new();
        };

        let mut out: Vec<(String, Result<Job>)> = entries
            .filter_map(std::result::Result::ok)
            .map(|e| e.path())
            // macOS's default filesystem already treats `Backup.TOML` and
            // `backup.toml` as the same file.
            .filter(|p| {
                p.extension()
                    .and_then(|x| x.to_str())
                    .is_some_and(|x| x.eq_ignore_ascii_case("toml"))
            })
            .map(|p| {
                let name = p
                    .file_stem()
                    .and_then(|s| s.to_str())
                    .unwrap_or("<unnamed>")
                    .to_string();
                (name, Job::load(&p))
            })
            .collect();

        out.sort_by(|a, b| a.0.cmp(&b.0));
        validate_after_graph(&mut out);
        out
    }

    pub fn schedule_source(&self) -> Option<&str> {
        self.schedule
            .as_ref()
            .map(nightjar_schedule::Schedule::source)
    }
}

/// `Ok(None)` is reachable, not dead code. `jiff-cron`'s search is bounded
/// at year 2100, so a schedule like `0 0 30 2 *` (day 30 of February) never
/// matches a real date.
pub fn next_column(job: &Job, tz: &jiff::tz::TimeZone, now: jiff::Timestamp) -> String {
    let Some(schedule) = &job.schedule else {
        // `after` is always `Some` here. `Job::load` guarantees it.
        return format!("after {}", job.after.as_deref().unwrap_or("?"));
    };
    match schedule.next_after(now, tz) {
        Ok(Some(t)) => nightjar_core::format::relative_future(t, now),
        Ok(None) => "none".to_string(),
        Err(_) => "—".to_string(),
    }
}

/// `Job::load_all` returns an empty `Vec` for a missing directory, an
/// unreadable one, and an empty one alike. Callers that need to tell these
/// apart must probe first.
#[derive(Clone, Copy)]
pub enum JobsDirState {
    Missing,
    Present,
}

pub fn probe_jobs_dir(jobs_dir: &Path) -> Result<JobsDirState> {
    match std::fs::read_dir(jobs_dir) {
        Ok(_) => Ok(JobsDirState::Present),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(JobsDirState::Missing),
        Err(e) => {
            Err(e).with_context(|| format!("cannot read jobs directory {}", jobs_dir.display()))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(dir: &Path, name: &str, body: &str) -> PathBuf {
        let p = dir.join(format!("{name}.toml"));
        std::fs::write(&p, body).unwrap();
        p
    }

    #[test]
    fn job_gets_default_values_when_only_command_and_schedule_are_given() {
        let tmp = tempfile::tempdir().unwrap();
        let p = write(
            tmp.path(),
            "backup",
            r#"
command  = "echo hi"
schedule = "daily at 2am"
"#,
        );
        let job = Job::load(&p).unwrap();

        assert_eq!(job.name, "backup");
        assert_eq!(job.command, "echo hi");
        assert!(job.enabled);
        assert_eq!(job.catchup, Catchup::Once);
        assert_eq!(job.overlap, Overlap::Skip);
        assert_eq!(
            job.login_shell, None,
            "unset in the file, so `Config::apply_defaults` decides"
        );
        assert_eq!(job.timeout, None);
    }

    #[test]
    fn job_parses_every_field_when_fully_specified() {
        let tmp = tempfile::tempdir().unwrap();
        let p = write(
            tmp.path(),
            "sync",
            r#"
command     = "rsync -a a b"
schedule    = "every 15 minutes"
timeout     = "30m"
catchup     = "all"
overlap     = "parallel"
workdir     = "/tmp"
enabled     = false
shell       = "/bin/bash"
login_shell = false

[env]
FOO = "bar"

[secrets]
PGPASSWORD = "op://vault/db/password"

[on_failure]
notify  = true
run     = "say failed"
webhook = "https://example.com/hook"
"#,
        );
        let job = Job::load(&p).unwrap();

        assert_eq!(job.timeout, Some(Duration::from_secs(1800)));
        assert_eq!(job.catchup, Catchup::All);
        assert_eq!(job.overlap, Overlap::Parallel);
        assert_eq!(job.workdir, Some(PathBuf::from("/tmp")));
        assert!(!job.enabled);
        assert_eq!(job.shell.as_deref(), Some("/bin/bash"));
        assert_eq!(job.login_shell, Some(false));
        assert_eq!(job.env.get("FOO").map(String::as_str), Some("bar"));
        assert_eq!(
            job.secrets.get("PGPASSWORD").map(String::as_str),
            Some("op://vault/db/password")
        );
        assert!(job.on_failure.notify);
        assert_eq!(job.on_failure.run.as_deref(), Some("say failed"));
        assert_eq!(
            job.on_failure.webhook.as_deref(),
            Some("https://example.com/hook")
        );
    }

    #[test]
    fn job_has_an_empty_secrets_map_when_no_secrets_block_is_present() {
        let tmp = tempfile::tempdir().unwrap();
        let p = write(
            tmp.path(),
            "plain",
            "command = \"true\"\nschedule = \"hourly\"\n",
        );
        let job = Job::load(&p).unwrap();
        assert!(job.secrets.is_empty());
    }

    #[test]
    fn secret_value_is_rejected_and_names_the_key_when_it_is_not_a_string() {
        let tmp = tempfile::tempdir().unwrap();
        let p = write(
            tmp.path(),
            "badsecret",
            "command = \"true\"\nschedule = \"hourly\"\n\n[secrets]\nPGPASSWORD = 5\n",
        );
        let err = Job::load(&p).unwrap_err();
        assert!(
            format!("{err:#}").contains("PGPASSWORD"),
            "message was: {err:#}"
        );
    }

    #[test]
    fn typo_is_rejected_and_names_the_key_when_it_appears_in_a_top_level_field() {
        let tmp = tempfile::tempdir().unwrap();
        let p = write(
            tmp.path(),
            "typo",
            r#"
command  = "sleep 3"
schedule = "hourly"
timout   = "1s"
"#,
        );
        let err = Job::load(&p).unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("timout"),
            "error must name the unknown key; got: {msg}"
        );
    }

    #[test]
    fn typo_is_rejected_and_names_the_key_when_it_appears_inside_on_failure() {
        let tmp = tempfile::tempdir().unwrap();
        let p = write(
            tmp.path(),
            "alerting",
            r#"
command  = "true"
schedule = "hourly"

[on_failure]
webook = "https://example.com/hook"
"#,
        );
        let err = Job::load(&p).unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("webook"),
            "error must name the unknown key; got: {msg}"
        );
    }

    #[test]
    fn expand_tilde_resolves_home_and_leaves_other_paths_alone() {
        let home = OsStr::new("/home/nightjar");

        assert_eq!(
            expand_tilde(Path::new("~"), Some(home)).unwrap(),
            PathBuf::from("/home/nightjar")
        );
        assert_eq!(
            expand_tilde(Path::new("~/backups"), Some(home)).unwrap(),
            PathBuf::from("/home/nightjar/backups")
        );
        assert_eq!(
            expand_tilde(Path::new("/tmp"), Some(home)).unwrap(),
            PathBuf::from("/tmp")
        );
        assert_eq!(
            expand_tilde(Path::new("~other/x"), Some(home)).unwrap(),
            PathBuf::from("~other/x")
        );
    }

    #[test]
    fn expand_tilde_says_so_instead_of_blaming_the_shell_when_home_is_not_set() {
        let err = expand_tilde(Path::new("~"), None).unwrap_err().to_string();
        assert!(err.contains("HOME"), "error was: {err}");
    }

    #[test]
    fn workdir_tilde_is_expanded_at_load_time() {
        let tmp = tempfile::tempdir().unwrap();
        let p = write(
            tmp.path(),
            "tilde",
            "command = \"pwd\"\nschedule = \"hourly\"\nworkdir = \"~\"\n",
        );

        let job = Job::load(&p).unwrap();
        let home = PathBuf::from(std::env::var_os("HOME").expect("HOME is set under cargo test"));
        assert_eq!(job.workdir, Some(home));
    }

    #[test]
    fn command_is_rejected_with_a_useful_message_when_it_is_empty() {
        let tmp = tempfile::tempdir().unwrap();
        let p = write(tmp.path(), "bad", "command = \"\"\nschedule = \"hourly\"\n");
        let err = Job::load(&p).unwrap_err().to_string();
        assert!(err.contains("command"), "message was: {err}");
    }

    #[test]
    fn timeout_is_rejected_when_it_is_longer_than_a_year() {
        let tmp = tempfile::tempdir().unwrap();
        let p = write(
            tmp.path(),
            "forever",
            "command = \"true\"\nschedule = \"hourly\"\ntimeout = \"400d\"\n",
        );
        let err = Job::load(&p).unwrap_err().to_string();
        assert!(
            err.contains("timeout") && err.contains("400d"),
            "got: {err}"
        );

        let p = write(
            tmp.path(),
            "year",
            "command = \"true\"\nschedule = \"hourly\"\ntimeout = \"365d\"\n",
        );
        assert_eq!(
            Job::load(&p).unwrap().timeout,
            Some(Duration::from_secs(365 * 86_400)),
            "the maximum itself is allowed"
        );
    }

    #[test]
    fn duration_suffixes_s_m_h_d_all_parse() {
        assert_eq!(parse_duration("45s").unwrap(), Duration::from_secs(45));
        assert_eq!(parse_duration("10m").unwrap(), Duration::from_secs(600));
        assert_eq!(parse_duration("2h").unwrap(), Duration::from_secs(7200));
        assert_eq!(parse_duration("1d").unwrap(), Duration::from_secs(86400));
        assert!(parse_duration("10x").is_err());
        assert!(parse_duration("").is_err());
    }

    #[test]
    fn load_all_does_not_hide_valid_jobs_when_one_file_is_malformed() {
        let tmp = tempfile::tempdir().unwrap();
        write(
            tmp.path(),
            "good",
            "command = \"true\"\nschedule = \"hourly\"\n",
        );
        write(tmp.path(), "broken", "command = = =\n");

        let loaded = Job::load_all(tmp.path());
        assert_eq!(loaded.len(), 2);

        let good = loaded.iter().find(|(n, _)| n == "good").unwrap();
        let broken = loaded.iter().find(|(n, _)| n == "broken").unwrap();
        assert!(good.1.is_ok());
        assert!(broken.1.is_err());
    }

    #[test]
    fn load_all_matches_the_toml_extension_case_insensitively() {
        let tmp = tempfile::tempdir().unwrap();
        write(
            tmp.path(),
            "lower",
            "command = \"true\"\nschedule = \"hourly\"\n",
        );
        std::fs::write(
            tmp.path().join("Backup.TOML"),
            "command = \"true\"\nschedule = \"hourly\"\n",
        )
        .unwrap();
        std::fs::write(
            tmp.path().join("Mixed.ToMl"),
            "command = \"true\"\nschedule = \"hourly\"\n",
        )
        .unwrap();
        std::fs::write(tmp.path().join("not-a-job.txt"), "ignored").unwrap();

        let mut names: Vec<String> = Job::load_all(tmp.path())
            .into_iter()
            .map(|(n, _)| n)
            .collect();
        names.sort();
        assert_eq!(names, vec!["Backup", "Mixed", "lower"]);
    }

    #[test]
    fn parse_duration_rejects_non_ascii() {
        let err = parse_duration("10Ω").unwrap_err();
        assert!(err.to_string().contains("ASCII"), "error was: {err}");
    }

    #[test]
    fn parse_duration_rejects_overflow() {
        let err = parse_duration("300000000000000d").unwrap_err();
        assert!(err.to_string().contains("too large"), "error was: {err}");
    }

    #[test]
    fn load_all_does_not_hide_valid_jobs_when_a_timeout_value_is_malformed() {
        let tmp = tempfile::tempdir().unwrap();
        write(
            tmp.path(),
            "valid_job",
            "command = \"echo hello\"\nschedule = \"hourly\"\n",
        );
        write(
            tmp.path(),
            "bad_timeout",
            "command = \"echo bye\"\nschedule = \"hourly\"\ntimeout = \"10Ω\"\n",
        );

        let loaded = Job::load_all(tmp.path());
        assert_eq!(loaded.len(), 2);

        let valid = loaded.iter().find(|(n, _)| n == "valid_job").unwrap();
        let bad = loaded.iter().find(|(n, _)| n == "bad_timeout").unwrap();
        assert!(valid.1.is_ok());
        assert!(bad.1.is_err());
    }

    #[test]
    fn schedule_fails_at_load_not_at_fire_time_when_it_is_invalid() {
        let tmp = tempfile::tempdir().unwrap();
        let p = write(
            tmp.path(),
            "bad",
            "command = \"true\"\nschedule = \"every other tuesday\"\n",
        );
        let err = Job::load(&p).unwrap_err().to_string();
        assert!(err.contains("every other tuesday"), "message was: {err}");
    }

    #[test]
    fn schedule_is_parsed_and_source_preserved_when_it_is_valid() {
        let tmp = tempfile::tempdir().unwrap();
        let p = write(
            tmp.path(),
            "ok",
            "command = \"true\"\nschedule = \"daily at 2am\"\n",
        );
        let job = Job::load(&p).unwrap();
        assert_eq!(job.schedule_source(), Some("daily at 2am"));
    }

    #[test]
    fn load_all_does_not_hide_other_jobs_when_one_job_has_a_bad_schedule() {
        let tmp = tempfile::tempdir().unwrap();
        write(
            tmp.path(),
            "good",
            "command = \"true\"\nschedule = \"hourly\"\n",
        );
        write(
            tmp.path(),
            "badsched",
            "command = \"true\"\nschedule = \"nonsense\"\n",
        );
        let loaded = Job::load_all(tmp.path());
        assert_eq!(loaded.len(), 2);
        assert!(loaded.iter().find(|(n, _)| n == "good").unwrap().1.is_ok());
        assert!(
            loaded
                .iter()
                .find(|(n, _)| n == "badsched")
                .unwrap()
                .1
                .is_err()
        );
    }

    #[test]
    fn job_is_rejected_when_it_has_both_schedule_and_after() {
        let tmp = tempfile::tempdir().unwrap();
        let p = write(
            tmp.path(),
            "both",
            "command = \"true\"\nschedule = \"hourly\"\nafter = [\"a\"]\n",
        );
        let err = Job::load(&p).unwrap_err().to_string();
        assert!(err.contains("mutually exclusive"), "message was: {err}");
    }

    #[test]
    fn job_is_rejected_when_it_has_neither_schedule_nor_after() {
        let tmp = tempfile::tempdir().unwrap();
        let p = write(tmp.path(), "neither", "command = \"true\"\n");
        let err = Job::load(&p).unwrap_err().to_string();
        assert!(
            err.contains("schedule") && err.contains("after"),
            "message was: {err}"
        );
    }

    #[test]
    fn after_is_rejected_and_the_error_names_dagu_when_it_has_two_parents() {
        let tmp = tempfile::tempdir().unwrap();
        let p = write(
            tmp.path(),
            "fanin",
            "command = \"true\"\nafter = [\"a\", \"b\"]\n",
        );
        let err = Job::load(&p).unwrap_err().to_string();
        assert!(err.contains("dagu"), "message was: {err}");
    }

    #[test]
    fn after_is_rejected_when_it_is_an_empty_list() {
        let tmp = tempfile::tempdir().unwrap();
        let p = write(tmp.path(), "empty", "command = \"true\"\nafter = []\n");
        let err = Job::load(&p).unwrap_err().to_string();
        assert!(err.contains("at least one"), "message was: {err}");
    }

    #[test]
    fn job_may_not_declare_itself_as_its_own_parent() {
        let tmp = tempfile::tempdir().unwrap();
        let p = write(
            tmp.path(),
            "selfie",
            "command = \"true\"\nafter = [\"selfie\"]\n",
        );
        let err = Job::load(&p).unwrap_err().to_string();
        assert!(err.contains("itself"), "message was: {err}");
    }

    #[test]
    fn job_parses_the_parent_when_triggered_by_after() {
        let tmp = tempfile::tempdir().unwrap();
        write(
            tmp.path(),
            "a",
            "command = \"true\"\nschedule = \"hourly\"\n",
        );
        let p = write(tmp.path(), "b", "command = \"true\"\nafter = [\"a\"]\n");
        let job = Job::load(&p).unwrap();
        assert_eq!(job.after.as_deref(), Some("a"));
        assert_eq!(job.schedule_source(), None);
    }

    #[test]
    fn after_is_rejected_when_it_names_a_job_that_does_not_exist() {
        let tmp = tempfile::tempdir().unwrap();
        write(tmp.path(), "b", "command = \"true\"\nafter = [\"ghost\"]\n");
        let loaded = Job::load_all(tmp.path());
        let b = &loaded.iter().find(|(n, _)| n == "b").unwrap().1;
        let err = b.as_ref().unwrap_err().to_string();
        assert!(err.contains("ghost"), "message was: {err}");
    }

    #[test]
    fn after_is_rejected_when_it_names_a_job_whose_own_file_is_broken() {
        let tmp = tempfile::tempdir().unwrap();
        write(tmp.path(), "a", "command = = =\n");
        write(tmp.path(), "b", "command = \"true\"\nafter = [\"a\"]\n");
        let loaded = Job::load_all(tmp.path());
        assert!(loaded.iter().find(|(n, _)| n == "a").unwrap().1.is_err());
        let b = &loaded.iter().find(|(n, _)| n == "b").unwrap().1;
        assert!(
            b.is_err(),
            "a child cannot know a parent that never loaded ever succeeds"
        );
    }

    #[test]
    fn after_loads_but_warns_when_it_names_a_disabled_job() {
        let tmp = tempfile::tempdir().unwrap();
        write(
            tmp.path(),
            "a",
            "command = \"true\"\nschedule = \"hourly\"\nenabled = false\n",
        );
        write(tmp.path(), "b", "command = \"true\"\nafter = [\"a\"]\n");
        let loaded = Job::load_all(tmp.path());
        let b = &loaded.iter().find(|(n, _)| n == "b").unwrap().1;
        let b = b
            .as_ref()
            .unwrap_or_else(|e| panic!("a disabled parent is a warning, not a load error: {e}"));
        assert_eq!(b.warnings.len(), 1, "got: {:?}", b.warnings);
        assert!(
            b.warnings[0].contains("disabled") && b.warnings[0].contains("\"a\""),
            "the warning must name the disabled parent: {}",
            b.warnings[0]
        );

        let a = &loaded.iter().find(|(n, _)| n == "a").unwrap().1;
        assert!(
            a.as_ref().unwrap().warnings.is_empty(),
            "the warning belongs to the child, not the parent"
        );
    }

    #[test]
    fn job_carries_no_warnings_when_nothing_is_worth_saying() {
        let tmp = tempfile::tempdir().unwrap();
        write(
            tmp.path(),
            "a",
            "command = \"true\"\nschedule = \"hourly\"\n",
        );
        write(tmp.path(), "b", "command = \"true\"\nafter = [\"a\"]\n");
        for (name, job) in Job::load_all(tmp.path()) {
            assert!(job.unwrap().warnings.is_empty(), "job {name}");
        }
    }

    #[test]
    fn cycle_is_rejected_and_the_error_lists_it() {
        let tmp = tempfile::tempdir().unwrap();
        write(tmp.path(), "a", "command = \"true\"\nafter = [\"b\"]\n");
        write(tmp.path(), "b", "command = \"true\"\nafter = [\"a\"]\n");
        let loaded = Job::load_all(tmp.path());

        for name in ["a", "b"] {
            let entry = &loaded.iter().find(|(n, _)| n == name).unwrap().1;
            let err = entry.as_ref().unwrap_err().to_string();
            assert!(
                err.contains("cycle") && err.contains('a') && err.contains('b'),
                "job {name:?}: message was: {err}"
            );
        }
    }

    #[test]
    fn three_job_chain_loads_cleanly() {
        let tmp = tempfile::tempdir().unwrap();
        write(
            tmp.path(),
            "a",
            "command = \"true\"\nschedule = \"hourly\"\n",
        );
        write(tmp.path(), "b", "command = \"true\"\nafter = [\"a\"]\n");
        write(tmp.path(), "c", "command = \"true\"\nafter = [\"b\"]\n");
        let loaded = Job::load_all(tmp.path());
        for name in ["a", "b", "c"] {
            assert!(
                loaded.iter().find(|(n, _)| n == name).unwrap().1.is_ok(),
                "job {name:?} should have loaded"
            );
        }
    }

    #[test]
    fn chain_is_rejected_when_deeper_than_thirty_two() {
        let tmp = tempfile::tempdir().unwrap();
        write(
            tmp.path(),
            "root",
            "command = \"true\"\nschedule = \"hourly\"\n",
        );
        write(
            tmp.path(),
            "job1",
            "command = \"true\"\nafter = [\"root\"]\n",
        );
        for i in 2..=33 {
            write(
                tmp.path(),
                &format!("job{i}"),
                &format!("command = \"true\"\nafter = [\"job{}\"]\n", i - 1),
            );
        }
        let loaded = Job::load_all(tmp.path());
        let deepest = &loaded.iter().find(|(n, _)| n == "job33").unwrap().1;
        let err = deepest.as_ref().unwrap_err().to_string();
        assert!(err.contains("32"), "message was: {err}");
    }

    #[test]
    fn limits_parse_every_field() {
        let tmp = tempfile::tempdir().unwrap();
        let p = write(
            tmp.path(),
            "capped",
            "command = \"true\"\nschedule = \"hourly\"\nshell = \"/bin/sh\"\n\n[limits]\ncpu_time = \"10m\"\nprocesses = 64\nfiles = 1024\n",
        );
        let job = Job::load(&p).unwrap();
        assert_eq!(job.limits.cpu_time, Some(600));
        assert_eq!(job.limits.processes, Some(64));
        assert_eq!(job.limits.files, Some(1024));
        assert_eq!(job.limits.memory, None);
    }

    #[test]
    fn job_has_no_limits_when_no_limits_block_is_present() {
        let tmp = tempfile::tempdir().unwrap();
        let p = write(
            tmp.path(),
            "plain",
            "command = \"true\"\nschedule = \"hourly\"\n",
        );
        let job = Job::load(&p).unwrap();
        assert!(
            job.limits.is_empty(),
            "an unset limit must leave the inherited one alone, not impose a default"
        );
    }

    #[test]
    fn limit_value_is_a_load_error_not_a_silent_default_when_it_is_unparseable() {
        let tmp = tempfile::tempdir().unwrap();
        let p = write(
            tmp.path(),
            "bad",
            "command = \"true\"\nschedule = \"hourly\"\n\n[limits]\ncpu_time = \"2 gerbils\"\n",
        );
        let err = Job::load(&p).unwrap_err().to_string();
        assert!(err.contains("cpu_time"), "message was: {err}");
    }

    #[test]
    fn typo_is_rejected_and_names_the_key_when_it_appears_inside_limits() {
        let tmp = tempfile::tempdir().unwrap();
        let p = write(
            tmp.path(),
            "typo",
            "command = \"true\"\nschedule = \"hourly\"\n\n[limits]\ncpu_tiem = \"10m\"\n",
        );
        let err = format!("{:#}", Job::load(&p).unwrap_err());
        assert!(err.contains("cpu_tiem"), "message was: {err}");
    }

    #[test]
    fn cpu_time_is_refused_when_the_shell_swallows_sigxcpu() {
        let tmp = tempfile::tempdir().unwrap();
        let p = write(
            tmp.path(),
            "zshcap",
            "command = \"true\"\nschedule = \"hourly\"\nshell = \"/bin/zsh\"\n\n[limits]\ncpu_time = \"10m\"\n",
        );
        let err = Job::load(&p).unwrap_err().to_string();
        assert!(err.contains("SIGXCPU"), "message was: {err}");
        assert!(
            err.contains("/bin/sh"),
            "the refusal must name the fix, not just the problem; got: {err}"
        );
    }

    #[test]
    fn cpu_time_is_accepted_when_the_shell_enforces_it() {
        let tmp = tempfile::tempdir().unwrap();
        for shell in ["/bin/sh", "/bin/bash"] {
            let p = write(
                tmp.path(),
                "ok",
                &format!(
                    "command = \"true\"\nschedule = \"hourly\"\nshell = \"{shell}\"\n\n[limits]\ncpu_time = \"10m\"\n"
                ),
            );
            let job = Job::load(&p).unwrap();
            assert_eq!(job.limits.cpu_time, Some(600), "shell {shell}");
        }
    }

    #[test]
    fn job_may_still_set_the_limits_that_work_when_using_zsh() {
        let tmp = tempfile::tempdir().unwrap();
        let p = write(
            tmp.path(),
            "zshok",
            "command = \"true\"\nschedule = \"hourly\"\nshell = \"/bin/zsh\"\n\n[limits]\nprocesses = 64\nfiles = 512\n",
        );
        let job = Job::load(&p).unwrap();
        assert_eq!(job.limits.processes, Some(64));
        assert_eq!(job.limits.files, Some(512));
    }

    #[test]
    fn limit_is_rejected_rather_than_applied_when_it_is_zero() {
        let tmp = tempfile::tempdir().unwrap();
        for (field, value) in [("cpu_time", "\"0s\""), ("processes", "0"), ("files", "0")] {
            let p = write(
                tmp.path(),
                "zero",
                &format!(
                    "command = \"true\"\nschedule = \"hourly\"\nshell = \"/bin/sh\"\n\n[limits]\n{field} = {value}\n"
                ),
            );
            let err = Job::load(&p).unwrap_err().to_string();
            assert!(
                err.contains(field),
                "limits.{field} = 0 must be refused; message was: {err}"
            );
        }
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn memory_limit_is_refused_rather_than_silently_unenforced_when_running_on_macos() {
        let tmp = tempfile::tempdir().unwrap();
        let p = write(
            tmp.path(),
            "mem",
            "command = \"true\"\nschedule = \"hourly\"\n\n[limits]\nmemory = \"2GB\"\n",
        );
        let err = Job::load(&p).unwrap_err().to_string();
        assert!(err.contains("macOS"), "message was: {err}");
        assert!(err.contains("RLIMIT_AS"), "message was: {err}");
    }

    #[cfg(not(target_os = "macos"))]
    #[test]
    fn memory_limit_parses_when_the_kernel_supports_it() {
        let tmp = tempfile::tempdir().unwrap();
        let p = write(
            tmp.path(),
            "mem",
            "command = \"true\"\nschedule = \"hourly\"\n\n[limits]\nmemory = \"2GB\"\n",
        );
        let job = Job::load(&p).unwrap();
        assert_eq!(job.limits.memory, Some(2 * 1024 * 1024 * 1024));
    }

    #[test]
    fn chain_loads_cleanly_when_exactly_thirty_two_deep() {
        let tmp = tempfile::tempdir().unwrap();
        write(
            tmp.path(),
            "root",
            "command = \"true\"\nschedule = \"hourly\"\n",
        );
        write(
            tmp.path(),
            "job1",
            "command = \"true\"\nafter = [\"root\"]\n",
        );
        for i in 2..=32 {
            write(
                tmp.path(),
                &format!("job{i}"),
                &format!("command = \"true\"\nafter = [\"job{}\"]\n", i - 1),
            );
        }
        let loaded = Job::load_all(tmp.path());
        let deepest = &loaded.iter().find(|(n, _)| n == "job32").unwrap().1;
        assert!(
            deepest.is_ok(),
            "32 is the cap, not one past it: {:?}",
            deepest.as_ref().err()
        );
    }
}
