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

use std::path::Path;
use std::sync::Arc;

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
pub use nightjar_config as config;
use nightjar_config::jobfile;
use nightjar_core::clock::SystemClock;
use nightjar_core::paths::Paths;
pub use nightjar_core::{clock, paths};
pub use nightjar_daemon as daemon;
use nightjar_daemon::Daemon;
pub use nightjar_runner as runner;
pub use nightjar_store as store;

#[derive(Parser)]
#[command(
    name = "nightjar",
    version,
    about = "cron that tells you what happened",
    disable_help_flag = true,
    disable_version_flag = true,
    disable_help_subcommand = true
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Commands,

    #[arg(
        short = 'h',
        long = "help",
        global = true,
        action = clap::ArgAction::Help,
        help = "print help for this command"
    )]
    pub help: Option<bool>,

    #[arg(
        short = 'V',
        long = "version",
        action = clap::ArgAction::Version,
        help = "print the version"
    )]
    pub version: Option<bool>,

    #[arg(skip)]
    pub host: Vec<String>,
}

#[derive(Subcommand)]
pub enum Commands {
    #[command(about = "write a new job file")]
    Add {
        #[arg(help = "what to call the job; it becomes <name>.toml")]
        name: String,
        #[arg(long = "cmd", help = "the shell command to run")]
        cmd: String,
        #[arg(
            long = "at",
            help = "when to run it, e.g. \"hourly\", \"every 15 minutes\", \"0 2 * * *\""
        )]
        at: String,
        #[arg(long, help = "kill the run after this long, e.g. 30s, 5m, 2h")]
        timeout: Option<String>,
        #[arg(
            long,
            help = "what to do about runs missed while the daemon was down: skip, once, or all"
        )]
        catchup: Option<String>,
    },
    #[command(about = "open a job file in $EDITOR")]
    Edit {
        #[arg(help = "the job to open")]
        job: String,
    },
    #[command(about = "delete a job file; its recorded runs stay")]
    Rm {
        #[arg(help = "the job to delete")]
        job: String,
    },
    #[command(about = "let a job fire on its schedule again")]
    Enable {
        #[arg(help = "the job to enable")]
        job: String,
    },
    #[command(about = "stop a job firing without deleting it")]
    Disable {
        #[arg(help = "the job to disable")]
        job: String,
    },
    #[command(about = "run a job now and wait for it")]
    Run {
        #[arg(help = "the job to run")]
        job: String,
    },
    #[command(hide = true)]
    Exec {
        #[arg(long)]
        job: String,
        #[arg(long)]
        run: String,
        #[arg(long, default_value = "manual")]
        trigger: String,
    },
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
        #[arg(
            long = "run-cmd",
            env = nightjar_runner::notify::RUN_CMD_ENV,
            hide_env_values = true
        )]
        run_cmd: Option<String>,
        #[arg(
            long,
            env = nightjar_runner::notify::WEBHOOK_ENV,
            hide_env_values = true
        )]
        webhook: Option<String>,
    },
    #[command(about = "list every job and whether it is enabled")]
    List {
        #[arg(long, help = "print one json document instead of a table")]
        json: bool,
    },
    #[command(about = "show what each job did last and when it runs next")]
    Status {
        #[arg(help = "show only this job; the default is every job")]
        job: Option<String>,
        #[arg(long, help = "print one json document instead of a table")]
        json: bool,
    },
    #[command(about = "replay what a run printed")]
    Logs {
        #[arg(help = "the job whose run you want to see")]
        job: String,
        #[arg(long, help = "a specific run id; the default is the most recent run")]
        run: Option<String>,
        #[arg(
            short = 'n',
            long = "lines",
            help = "show only the last n lines of each stream"
        )]
        lines: Option<usize>,
        #[arg(short = 'f', long, help = "keep printing while the run is still going")]
        follow: bool,
        #[arg(long, help = "print one json document instead of the captured output")]
        json: bool,
    },
    #[command(about = "run the scheduler until it is stopped")]
    Daemon {
        #[arg(long, help = "stay attached to this terminal instead of detaching")]
        foreground: bool,
    },
    #[command(about = "keep the daemon running across logout and reboot")]
    Service {
        #[command(subcommand)]
        action: ServiceCommand,
    },
    #[command(about = "turn your crontab into job files")]
    Import {
        #[arg(
            long = "from-stdin",
            help = "read the crontab from stdin instead of `crontab -l`"
        )]
        from_stdin: bool,
        #[arg(
            long,
            help = "enable the imported jobs; without this they are written disabled"
        )]
        enable: bool,
    },
    #[command(about = "check this machine over and name the fix for anything wrong")]
    Doctor {
        #[arg(long, help = "print one json document instead of a report")]
        json: bool,
    },
    #[command(about = "browse jobs, runs and output in the terminal")]
    Tui,
    #[command(about = "serve the status page on this machine")]
    Serve {
        #[arg(
            long,
            default_value = "127.0.0.1",
            help = "the loopback address to bind"
        )]
        bind: std::net::IpAddr,
        #[arg(long, default_value_t = nightjar_web::DEFAULT_PORT, help = "the port to bind")]
        port: u16,
        #[arg(
            long,
            env = "NIGHTJAR_TOKEN",
            help = "require this token on every request"
        )]
        token: Option<String>,
        #[arg(
            long,
            help = "read the token from this file instead of the command line"
        )]
        token_file: Option<std::path::PathBuf>,
    },
}

#[derive(Subcommand)]
pub enum ServiceCommand {
    #[command(about = "register the daemon with launchd or systemd")]
    Install,
    #[command(about = "unregister the daemon and remove its unit file")]
    Uninstall,
    #[command(about = "say whether a unit is installed and whether the daemon is up")]
    Status,
}

/// # Errors
/// fails if any argument is not valid utf-8
pub fn require_utf8_args(args: impl Iterator<Item = std::ffi::OsString>) -> Result<Vec<String>> {
    args.map(|a| {
        let lossy = a.to_string_lossy().into_owned();
        a.into_string()
            .map_err(|_| anyhow::anyhow!("argument {lossy:?} is not valid UTF-8"))
    })
    .collect()
}

/// # Errors
/// fails if the arguments do not parse, or if the command itself fails
pub fn run_cli(args: &[&str]) -> Result<i32> {
    let (hosts, rest) = split_host_args(args)?;

    let mut cli = match Cli::try_parse_from(
        std::iter::once("nightjar".to_string()).chain(rest.iter().cloned()),
    ) {
        Ok(cli) => cli,
        Err(e) => e.exit(),
    };
    refuse_unsupported_remote_command(&hosts, &cli.command, &rest)?;
    refuse_follow_with_host(&hosts, &cli.command, &rest)?;
    refuse_follow_with_json(&cli.command)?;
    cli.host = hosts;

    dispatch(cli)
}

pub(crate) fn split_host_args(args: &[&str]) -> Result<(Vec<String>, Vec<String>)> {
    let mut hosts = Vec::new();
    let mut rest = Vec::new();
    let mut i = 0;
    while i < args.len() {
        let value = if let Some(value) = args[i].strip_prefix("--host=") {
            i += 1;
            value
        } else if args[i] == "--host" {
            let value = args.get(i + 1).context("--host requires a value")?;
            i += 2;
            value
        } else {
            rest.push(args[i].to_string());
            i += 1;
            continue;
        };
        for host in value.split(',') {
            validate_host(host)?;
            hosts.push(host.to_string());
        }
    }
    Ok((hosts, rest))
}

fn validate_host(host: &str) -> Result<()> {
    if host.is_empty() {
        bail!("invalid host: a host name cannot be empty");
    }
    if host.starts_with('-') {
        bail!("invalid host {host:?}: a host name cannot start with \"-\"");
    }
    if host.chars().any(char::is_whitespace) {
        bail!("invalid host {host:?}: a host name cannot contain whitespace");
    }
    Ok(())
}

const fn is_read_only(command: &Commands) -> bool {
    matches!(
        command,
        Commands::Status { .. } | Commands::List { .. } | Commands::Logs { .. }
    )
}

const fn is_diagnostic_only(command: &Commands) -> bool {
    matches!(command, Commands::Doctor { .. })
}

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

/// # Errors
/// fails if the command itself fails; the exit code it returns is not an error
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

fn remote_invocation(command: &Commands) -> (Vec<String>, bool) {
    match command {
        Commands::Status { job, json } => {
            let mut args = vec!["status".to_string()];
            if let Some(job_name) = job {
                args.push(job_name.clone());
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
            if let Some(run_id) = run {
                args.push("--run".to_string());
                args.push(run_id.clone());
            }
            if let Some(max_lines) = lines {
                args.push("--lines".to_string());
                args.push(max_lines.to_string());
            }
            args.push("--json".to_string());
            (args, *json)
        }
        _ => unreachable!("run_cli only lets --host through for read-only commands"),
    }
}

fn refuse_follow_with_json(command: &Commands) -> Result<()> {
    let Commands::Logs {
        job,
        follow: true,
        json: true,
        ..
    } = command
    else {
        return Ok(());
    };
    let quoted = nightjar_remote::shell_quote(job);
    bail!(
        "--follow cannot be used with --json; run `nightjar logs {quoted} --follow` to \
         stream, or `nightjar logs {quoted} --json` for one document"
    );
}

fn cmd_daemon() -> Result<i32> {
    nightjar_daemon::install_stop_handlers();
    let mut daemon = Daemon::new(Paths::resolve()?, Arc::new(SystemClock))?;
    daemon.run()?;
    Ok(0)
}

pub(crate) fn read_captured(path: &Path) -> Result<Option<Vec<u8>>> {
    match std::fs::read(path) {
        Ok(bytes) => Ok(Some(bytes)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
    }
}

#[cfg(test)]
mod tests {
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

    #[test]
    fn notify_reads_its_channels_from_the_environment_when_argv_omits_them() {
        let cli = Cli::try_parse_from(["nightjar", "notify", "--job=j", "--kind=failed"]).unwrap();
        let Commands::Notify {
            run_cmd, webhook, ..
        } = cli.command
        else {
            panic!("expected Notify");
        };
        assert!(
            run_cmd.is_none() || std::env::var_os(nightjar_runner::notify::RUN_CMD_ENV).is_some()
        );
        assert!(
            webhook.is_none() || std::env::var_os(nightjar_runner::notify::WEBHOOK_ENV).is_some()
        );
    }

    fn hosts(args: &[&str]) -> Vec<String> {
        split_host_args(args).unwrap().0
    }

    fn rest(args: &[&str]) -> Vec<String> {
        split_host_args(args).unwrap().1
    }

    #[test]
    fn host_accepts_comma_list_and_repeated_flags() {
        assert_eq!(hosts(&["--host", "a,b", "--host", "c"]), ["a", "b", "c"]);
        assert_eq!(hosts(&["--host=a,b", "--host=c"]), ["a", "b", "c"]);
    }

    #[test]
    fn host_is_rejected_before_it_ever_reaches_ssh_when_it_begins_with_a_dash() {
        let err = split_host_args(&["--host", "-oProxyCommand=x"]).unwrap_err();
        assert!(err.to_string().contains("cannot start with"), "got: {err}");

        let err = run_cli(&["--host", "-oProxyCommand=x", "status"]).unwrap_err();
        assert!(err.to_string().contains("cannot start with"), "got: {err}");
    }

    #[test]
    fn host_is_still_caught_when_it_is_dash_prefixed_among_others_in_a_comma_list() {
        assert!(split_host_args(&["--host", "web1,-oBad=1"]).is_err());
    }

    #[test]
    fn host_is_rejected_when_it_is_empty() {
        for args in [
            &["--host", "", "status"][..],
            &["--host=", "status"][..],
            &["--host", "a,,b", "status"][..],
            &["--host", "a,", "status"][..],
        ] {
            let err = split_host_args(args).unwrap_err();
            assert!(err.to_string().contains("empty"), "{args:?}: {err}");
        }
    }

    #[test]
    fn host_is_rejected_when_it_contains_whitespace() {
        let err = split_host_args(&["--host", "web 1"]).unwrap_err();
        assert!(err.to_string().contains("whitespace"), "got: {err}");
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
        assert!(hosts(&["run", "backup"]).is_empty());
        assert_eq!(rest(&["run", "backup"]), ["run", "backup"]);

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
        assert_eq!(hosts(&["run", "backup", "--host", "web1"]), ["web1"]);
        assert_eq!(
            rest(&["run", "backup", "--host", "web1"]),
            ["run", "backup"]
        );
        assert_eq!(rest(&["run", "--host=web1", "backup"]), ["run", "backup"]);
    }

    #[test]
    fn host_flag_is_error_not_silently_dropped_when_dangling_with_no_value() {
        assert!(split_host_args(&["status", "--host"]).is_err());

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
    fn follow_is_refused_and_names_both_alternatives_when_paired_with_json() {
        let err = run_cli(&["logs", "backup", "--follow", "--json"]).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("--follow cannot be used with --json"),
            "got: {msg}"
        );
        assert!(msg.contains("nightjar logs backup --follow"), "got: {msg}");
        assert!(msg.contains("nightjar logs backup --json"), "got: {msg}");
    }

    #[test]
    fn follow_refusal_quotes_job_so_hint_runs_verbatim_when_name_contains_a_space() {
        let err = run_cli(&["logs", "my backup", "--follow", "--json"]).unwrap_err();
        assert!(
            err.to_string()
                .contains("nightjar logs 'my backup' --follow"),
            "got: {err}"
        );
    }

    #[test]
    fn follow_is_not_refused_when_json_is_not_also_asked_for() {
        let command = Commands::Logs {
            job: "backup".to_string(),
            run: None,
            lines: None,
            follow: true,
            json: false,
        };
        assert!(refuse_follow_with_json(&command).is_ok());
    }

    #[test]
    fn json_is_not_refused_when_follow_is_not_also_asked_for() {
        let command = Commands::Logs {
            job: "backup".to_string(),
            run: None,
            lines: None,
            follow: false,
            json: true,
        };
        assert!(refuse_follow_with_json(&command).is_ok());
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
