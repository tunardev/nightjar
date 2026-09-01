//! Re-exports sibling crates so its own integration tests can reach them
//! through one crate name instead of several.

pub mod add;
pub mod doctor;
pub mod import;
pub mod list;
pub mod logs;
mod merged;
pub mod notify;
pub mod run;
pub mod service;
pub mod status;

pub use nightjar_config as config;
pub use nightjar_core::{clock, paths};
pub use nightjar_daemon as daemon;
pub use nightjar_runner as runner;
pub use nightjar_store as store;

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use nightjar_config::jobfile;
use nightjar_core::clock::SystemClock;
use nightjar_core::paths::Paths;
use nightjar_daemon::Daemon;
use std::path::Path;
use std::sync::Arc;

#[derive(Parser)]
#[command(
    name = "nightjar",
    version,
    about = "cron that tells you what happened"
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Commands,

    // `run_cli` populates this before clap parses the rest. `--host` must
    // be recognized no matter where it falls relative to the subcommand.
    #[arg(skip)]
    pub host: Vec<String>,
}

#[derive(Subcommand)]
pub enum Commands {
    /// Create a new job
    Add {
        name: String,
        #[arg(long = "cmd")]
        cmd: String,
        #[arg(long = "at")]
        at: String,
        #[arg(long)]
        timeout: Option<String>,
        #[arg(long)]
        catchup: Option<String>,
    },
    /// Open $EDITOR on a job's file and re-validate it on save
    Edit { job: String },
    /// Delete a job's file
    Rm { job: String },
    /// Enable a job so the daemon will schedule it
    Enable { job: String },
    /// Disable a job without deleting it
    Disable { job: String },
    /// Run a job now, then print the output it captured
    Run { job: String },
    /// Internal: execute exactly one run and record it
    #[command(hide = true)]
    Exec {
        #[arg(long)]
        job: String,
        #[arg(long)]
        run: String,
        #[arg(long, default_value = "manual")]
        trigger: String,
    },
    /// Internal: dispatch one alert through the real channels, detached from
    /// whatever spawned it
    #[command(hide = true)]
    Notify {
        #[arg(long)]
        job: String,
        #[arg(long)]
        kind: String,
        #[arg(long)]
        exit_code: Option<i32>,
        #[arg(long)]
        notify: bool,
        #[arg(long = "run-cmd")]
        run_cmd: Option<String>,
        #[arg(long)]
        webhook: Option<String>,
    },
    /// List defined jobs
    List {
        #[arg(long)]
        json: bool,
    },
    /// Show the last outcome of each job
    Status {
        job: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// Show captured output of a job's most recent run, or of one specific run
    Logs {
        job: String,
        /// Show this run instead of the most recent one
        #[arg(long)]
        run: Option<String>,
        /// Show only the last N lines of each stream
        #[arg(short = 'n', long = "lines")]
        lines: Option<usize>,
        /// Keep streaming a running job's output until it finishes
        #[arg(short = 'f', long)]
        follow: bool,
        #[arg(long)]
        json: bool,
    },
    /// Run the scheduler in the foreground
    Daemon {
        /// Accepted for compatibility; the daemon already runs in the
        /// foreground, so this flag changes nothing.
        #[arg(long)]
        foreground: bool,
    },
    /// Install, remove, or check the background service that runs the daemon
    Service {
        #[command(subcommand)]
        action: ServiceCommand,
    },
    /// Import crontab entries as new jobs, written disabled so they cannot
    /// double-run alongside cron
    Import {
        /// Read crontab lines from stdin instead of the user's real crontab
        #[arg(long = "from-stdin")]
        from_stdin: bool,
        /// Write imported jobs enabled instead of disabled
        #[arg(long)]
        enable: bool,
    },
    /// Check whether jobs can actually run, and say what to do if not
    Doctor {
        #[arg(long)]
        json: bool,
    },
    /// Browse job history and drill into failures
    Tui,
    /// Serve a read-only view of job status over HTTP
    Serve {
        /// Address to bind to; only loopback is offered — tunnel in for
        /// remote access (see the error printed on refusal)
        #[arg(long, default_value = "127.0.0.1")]
        bind: std::net::IpAddr,
        #[arg(long, default_value_t = nightjar_web::DEFAULT_PORT)]
        port: u16,
        /// Optional even on loopback; protects against another local account.
        /// On Linux, argv is world-readable via /proc/<pid>/cmdline, so
        /// prefer `NIGHTJAR_TOKEN` or --token-file over passing this directly.
        #[arg(long, env = "NIGHTJAR_TOKEN")]
        token: Option<String>,
        /// Reads the token from a file instead — the file's own permissions
        /// protect it the way neither argv nor a shell history file can.
        #[arg(long)]
        token_file: Option<std::path::PathBuf>,
    },
}

#[derive(Subcommand)]
pub enum ServiceCommand {
    /// Generate and register the unit so the daemon starts at login/boot
    Install,
    /// Unregister and remove the unit
    Uninstall,
    /// Report whether the unit is installed and whether the daemon is running
    Status,
}

/// `std::env::args()` panics on non-UTF-8 input. This turns that into an
/// ordinary error instead.
pub fn require_utf8_args(args: impl Iterator<Item = std::ffi::OsString>) -> Result<Vec<String>> {
    args.map(|a| {
        let lossy = a.to_string_lossy().into_owned();
        a.into_string()
            .map_err(|_| anyhow::anyhow!("argument {lossy:?} is not valid UTF-8"))
    })
    .collect()
}

pub fn run_cli(args: &[&str]) -> Result<i32> {
    let hosts = parse_hosts(args)?;
    let rest = strip_host_args(args)?;

    let mut cli = match Cli::try_parse_from(
        std::iter::once("nightjar".to_string()).chain(rest.iter().cloned()),
    ) {
        Ok(cli) => cli,
        Err(e) => e.exit(),
    };
    refuse_unsupported_remote_command(&hosts, &cli.command, &rest)?;
    refuse_follow_with_host(&hosts, &cli.command, &rest)?;
    cli.host = hosts;

    dispatch(cli)
}

// Also accepts `--host=value`, matching the form clap itself would use.
pub(crate) fn parse_hosts(args: &[&str]) -> Result<Vec<String>> {
    let mut hosts = Vec::new();
    let mut i = 0;
    while i < args.len() {
        if let Some(value) = args[i].strip_prefix("--host=") {
            for h in value.split(',') {
                validate_host(h)?;
                hosts.push(h.to_string());
            }
            i += 1;
        } else if args[i] == "--host" {
            let value = args.get(i + 1).context("--host requires a value")?;
            for h in value.split(',') {
                validate_host(h)?;
                hosts.push(h.to_string());
            }
            i += 2;
        } else {
            i += 1;
        }
    }
    Ok(hosts)
}

/// `remote::ssh_argv`'s `--` terminator already neutralizes a host
/// starting with `-`. This catches it earlier with a clearer error than
/// ssh's own message.
fn validate_host(host: &str) -> Result<()> {
    if host.starts_with('-') {
        bail!("invalid host {host:?}: a host name cannot start with \"-\"");
    }
    Ok(())
}

fn strip_host_args(args: &[&str]) -> Result<Vec<String>> {
    let mut rest = Vec::new();
    let mut i = 0;
    while i < args.len() {
        if args[i] == "--host" {
            args.get(i + 1).context("--host requires a value")?;
            i += 2;
        } else if args[i].starts_with("--host=") {
            i += 1;
        } else {
            rest.push(args[i].to_string());
            i += 1;
        }
    }
    Ok(rest)
}

/// Remote is read-only. Mutating commands must never run silently against
/// whichever host `ssh` happens to resolve.
fn is_read_only(command: &Commands) -> bool {
    matches!(
        command,
        Commands::Status { .. } | Commands::List { .. } | Commands::Logs { .. }
    )
}

/// `doctor` doesn't mutate anything. It's refused only because there's no
/// way to merge its output, not because it's unsafe remotely.
fn is_diagnostic_only(command: &Commands) -> bool {
    matches!(command, Commands::Doctor { .. })
}

/// Quotes each argument so a job name with a space still produces a
/// runnable hint, not a mangled one.
fn quoted_command(rest: &[String]) -> String {
    rest.iter()
        .map(|a| nightjar_remote::shell_quote(a))
        .collect::<Vec<_>>()
        .join(" ")
}

fn refuse_unsupported_remote_command(
    hosts: &[String],
    command: &Commands,
    rest: &[String],
) -> Result<()> {
    if hosts.is_empty() || is_read_only(command) {
        return Ok(());
    }
    let cmd = quoted_command(rest);
    let hint = hosts
        .iter()
        .map(|h| format!("ssh {h} nightjar {cmd}"))
        .collect::<Vec<_>>()
        .join("; ");
    if is_diagnostic_only(command) {
        bail!("{cmd} cannot be merged across hosts; run: {hint}");
    }
    bail!("remote control is not supported; run: {hint}");
}

/// Fan-out only reports a host after its `ssh` child exits. A followed
/// remote would buffer everything until `remote`'s wall-clock bound, then
/// get cut off having streamed nothing.
fn refuse_follow_with_host(hosts: &[String], command: &Commands, rest: &[String]) -> Result<()> {
    if hosts.is_empty() {
        return Ok(());
    }
    let Commands::Logs { follow: true, .. } = command else {
        return Ok(());
    };
    let cmd = quoted_command(rest);
    let hint = hosts
        .iter()
        .map(|h| format!("ssh {h} nightjar {cmd}"))
        .collect::<Vec<_>>()
        .join("; ");
    bail!("--follow cannot be used with --host; run: {hint}");
}

pub fn dispatch(cli: Cli) -> Result<i32> {
    if !cli.host.is_empty() {
        return Ok(dispatch_remote(&cli.host, &cli.command));
    }
    match cli.command {
        Commands::Add {
            name,
            cmd,
            at,
            timeout,
            catchup,
        } => add::cmd_add(&name, &cmd, &at, timeout.as_deref(), catchup.as_deref()),
        Commands::Edit { job } => jobfile::cmd_edit(&job),
        Commands::Rm { job } => jobfile::cmd_rm(&job),
        Commands::Enable { job } => jobfile::cmd_enable(&job),
        Commands::Disable { job } => jobfile::cmd_disable(&job),
        Commands::Run { job } => run::cmd_run(&job),
        Commands::Exec { job, run, trigger } => run::cmd_exec(&job, &run, &trigger),
        Commands::Notify {
            job,
            kind,
            exit_code,
            notify,
            run_cmd,
            webhook,
        } => notify::cmd_notify(
            &job,
            &kind,
            exit_code,
            &nightjar_config::OnFailure {
                notify,
                run: run_cmd,
                webhook,
            },
        ),
        Commands::List { json } => list::cmd_list(json),
        Commands::Status { job, json } => status::cmd_status(job.as_deref(), json),
        Commands::Logs {
            job,
            run,
            lines,
            follow,
            json,
        } => logs::cmd_logs(&job, run.as_deref(), lines, follow, json),
        Commands::Daemon { foreground: _ } => cmd_daemon(),
        Commands::Service { action } => match action {
            ServiceCommand::Install => service::cmd_install(),
            ServiceCommand::Uninstall => service::cmd_uninstall(),
            ServiceCommand::Status => service::cmd_status(),
        },
        Commands::Import { from_stdin, enable } => import::cmd_import(from_stdin, enable),
        Commands::Doctor { json } => doctor::cmd_doctor(json),
        Commands::Tui => nightjar_tui::cmd_tui(),
        Commands::Serve {
            bind,
            port,
            token,
            token_file,
        } => cmd_serve(bind, port, token, token_file.as_deref()),
    }
}

fn cmd_serve(
    bind: std::net::IpAddr,
    port: u16,
    token: Option<String>,
    token_file: Option<&Path>,
) -> Result<i32> {
    let token = resolve_token(token, token_file)?;
    let paths = Paths::resolve()?;
    nightjar_web::serve(std::net::SocketAddr::new(bind, port), token, &paths)?;
    Ok(0)
}

fn resolve_token(token: Option<String>, token_file: Option<&Path>) -> Result<Option<String>> {
    match token_file {
        Some(path) => {
            let contents = std::fs::read_to_string(path)
                .with_context(|| format!("reading --token-file {}", path.display()))?;
            Ok(Some(contents.trim_end().to_string()))
        }
        None => Ok(token),
    }
}

fn dispatch_remote(hosts: &[String], command: &Commands) -> i32 {
    let (args, local_json) = remote_invocation(command);
    let results = nightjar_remote::fan_out(hosts, &args);
    match command {
        Commands::Status { .. } => status::cmd_status_remote(results, local_json),
        Commands::List { .. } => list::cmd_list_remote(results, local_json),
        Commands::Logs { .. } => logs::cmd_logs_remote(results, local_json),
        _ => unreachable!("run_cli only lets --host through for read-only commands"),
    }
}

/// Rebuilt from the parsed `Commands`, not `run_cli`'s raw argv. That argv
/// is quoted back to the user verbatim in refusal messages and must stay
/// unmutated.
fn remote_invocation(command: &Commands) -> (Vec<String>, bool) {
    match command {
        Commands::Status { job, json } => {
            let mut args = vec!["status".to_string()];
            if let Some(j) = job {
                args.push(j.clone());
            }
            args.push("--json".to_string());
            (args, *json)
        }
        Commands::List { json } => (vec!["list".to_string(), "--json".to_string()], *json),
        Commands::Logs {
            job,
            run,
            lines,
            follow: _,
            json,
        } => {
            let mut args = vec!["logs".to_string(), job.clone()];
            if let Some(r) = run {
                args.push("--run".to_string());
                args.push(r.clone());
            }
            if let Some(n) = lines {
                args.push("--lines".to_string());
                args.push(n.to_string());
            }
            args.push("--json".to_string());
            (args, *json)
        }
        _ => unreachable!("run_cli only lets --host through for read-only commands"),
    }
}

fn cmd_daemon() -> Result<i32> {
    nightjar_daemon::install_stop_handlers();
    let mut daemon = Daemon::new(Paths::resolve()?, Arc::new(SystemClock))?;
    daemon.run()?;
    Ok(0)
}

/// Returns raw bytes, not a String — invalid UTF-8 would make
/// `read_to_string` discard the whole file.
pub(crate) fn read_captured(path: &Path) -> Result<Option<Vec<u8>>> {
    match std::fs::read(path) {
        Ok(bytes) => Ok(Some(bytes)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
    }
}

#[cfg(test)]
mod serve_token_tests {
    use super::*;

    #[test]
    fn resolve_token_falls_through_to_input_token_when_no_file_is_given() {
        assert_eq!(
            resolve_token(Some("abc".to_string()), None).unwrap(),
            Some("abc".to_string())
        );
        assert_eq!(resolve_token(None, None).unwrap(), None);
    }

    #[test]
    fn token_file_wins_over_token_and_has_trailing_newline_trimmed() {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(tmp.path(), "file-token-value\n").unwrap();
        assert_eq!(
            resolve_token(Some("ignored".to_string()), Some(tmp.path())).unwrap(),
            Some("file-token-value".to_string())
        );
    }

    #[test]
    fn token_file_is_reported_error_not_panic_when_it_is_unreadable() {
        let err = resolve_token(None, Some(Path::new("/does/not/exist/at/all"))).unwrap_err();
        assert!(err.to_string().contains("--token-file"), "got: {err}");
    }

    #[test]
    fn token_file_parses_as_serve_flag() {
        let cli = Cli::try_parse_from(["nightjar", "serve", "--token-file", "/tmp/x"]).unwrap();
        match cli.command {
            Commands::Serve { token_file, .. } => {
                assert_eq!(token_file, Some(std::path::PathBuf::from("/tmp/x")));
            }
            _ => panic!("expected Serve"),
        }
    }
}

#[cfg(test)]
mod host_tests {
    use super::*;

    #[test]
    fn host_accepts_comma_list_and_repeated_flags() {
        assert_eq!(
            parse_hosts(&["--host", "a,b", "--host", "c"]).unwrap(),
            ["a", "b", "c"]
        );
    }

    #[test]
    fn host_is_rejected_before_it_ever_reaches_ssh_when_it_begins_with_a_dash() {
        let err = parse_hosts(&["--host", "-oProxyCommand=x"]).unwrap_err();
        assert!(err.to_string().contains("cannot start with"), "got: {err}");

        let err = run_cli(&["--host", "-oProxyCommand=x", "status"]).unwrap_err();
        assert!(err.to_string().contains("cannot start with"), "got: {err}");
    }

    #[test]
    fn host_is_still_caught_when_it_is_dash_prefixed_among_others_in_a_comma_list() {
        assert!(parse_hosts(&["--host", "web1,-oBad=1"]).is_err());
    }

    #[test]
    fn host_is_refused_and_names_ssh_command_when_paired_with_mutating_subcommand() {
        let err = run_cli(&["--host", "web1", "run", "backup"]).unwrap_err();
        assert!(
            err.to_string().contains("ssh web1 nightjar run backup"),
            "got: {err}"
        );
    }

    #[test]
    fn refusal_hint_quotes_argument_so_it_can_be_run_verbatim_when_argument_contains_a_space() {
        let err = run_cli(&["--host", "web1", "run", "my backup"]).unwrap_err();
        assert!(
            err.to_string()
                .contains("ssh web1 nightjar run 'my backup'"),
            "got: {err}"
        );
    }

    #[test]
    fn subcommand_is_not_refused_when_it_is_read_only() {
        let command = Commands::Status {
            job: None,
            json: false,
        };
        assert!(refuse_unsupported_remote_command(&["web1".to_string()], &command, &[]).is_ok());
    }

    #[test]
    fn cli_behaves_exactly_as_before_when_no_host_flag_is_given() {
        assert!(parse_hosts(&["run", "backup"]).unwrap().is_empty());
        assert_eq!(
            strip_host_args(&["run", "backup"]).unwrap(),
            ["run", "backup"]
        );

        let command = Commands::Run {
            job: "backup".to_string(),
        };
        assert!(
            refuse_unsupported_remote_command(&[], &command, &["run".into(), "backup".into()])
                .is_ok()
        );
    }

    #[test]
    fn host_is_still_recognized_when_it_appears_after_subcommand() {
        assert_eq!(
            parse_hosts(&["run", "backup", "--host", "web1"]).unwrap(),
            ["web1"]
        );
        assert_eq!(
            strip_host_args(&["run", "backup", "--host", "web1"]).unwrap(),
            ["run", "backup"]
        );
    }

    #[test]
    fn host_flag_is_error_not_silently_dropped_when_dangling_with_no_value() {
        assert!(parse_hosts(&["status", "--host"]).is_err());
        assert!(strip_host_args(&["status", "--host"]).is_err());

        let err = run_cli(&["status", "--host"]).unwrap_err();
        assert!(err.to_string().contains("--host"), "got: {err}");
    }

    #[test]
    fn subcommand_names_real_reason_not_control_when_it_is_diagnostic_only() {
        let err = run_cli(&["--host", "web1", "doctor"]).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("cannot be merged across hosts"), "got: {msg}");
        assert!(msg.contains("ssh web1 nightjar doctor"), "got: {msg}");
        assert!(
            !msg.contains("remote control is not supported"),
            "got: {msg}"
        );
    }

    #[test]
    fn host_is_refused_and_names_ssh_command_when_paired_with_logs_follow() {
        let err = run_cli(&["--host", "web1", "logs", "backup", "--follow"]).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("--follow"), "got: {msg}");
        assert!(
            msg.contains("ssh web1 nightjar logs backup --follow"),
            "got: {msg}"
        );
    }

    #[test]
    fn host_is_not_refused_when_paired_with_logs_without_follow() {
        let command = Commands::Logs {
            job: "backup".to_string(),
            run: None,
            lines: None,
            follow: false,
            json: false,
        };
        assert!(refuse_follow_with_host(&["web1".to_string()], &command, &[]).is_ok());
    }

    #[test]
    fn argument_is_reported_as_error_not_panic_when_it_is_not_valid_utf8() {
        use std::os::unix::ffi::OsStringExt;

        let bad = std::ffi::OsString::from_vec(vec![0x66, 0x6f, 0xff, 0x6f]);
        let err = require_utf8_args(vec![bad].into_iter()).unwrap_err();

        assert!(err.to_string().contains("not valid UTF-8"), "got: {err}");
    }

    #[test]
    fn argument_passes_through_unchanged_when_it_is_valid_utf8() {
        let args = vec![
            std::ffi::OsString::from("status"),
            std::ffi::OsString::from("--json"),
        ];
        assert_eq!(
            require_utf8_args(args.into_iter()).unwrap(),
            ["status", "--json"]
        );
    }
}
