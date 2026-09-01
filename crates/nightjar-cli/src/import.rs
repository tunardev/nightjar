use anyhow::{Context, Result, bail};
use nightjar_config::jobfile::write_job_file_atomic;
use nightjar_core::paths::{Paths, validate_job_name};
use nightjar_schedule::Schedule;
use std::io::Read;
use std::path::{Path, PathBuf};
use toml_edit::{DocumentMut, value};

pub fn cmd_import(from_stdin: bool, enable: bool) -> Result<i32> {
    let source = read_crontab(from_stdin)?;
    let paths = Paths::resolve()?;
    paths.ensure_dirs()?;

    let report = import_crontab(&paths, &source, enable);
    let skipped = report.skipped.len();
    print_report(&report, enable);
    Ok(i32::from(skipped != 0))
}

fn read_crontab(from_stdin: bool) -> Result<String> {
    if from_stdin {
        let mut buf = String::new();
        std::io::stdin()
            .read_to_string(&mut buf)
            .context("reading crontab from stdin")?;
        return Ok(buf);
    }

    let output = std::process::Command::new("crontab")
        .arg("-l")
        .output()
        .context("running `crontab -l` (use --from-stdin to import from a file instead)")?;
    if !output.status.success() {
        bail!(
            "`crontab -l` exited with {}: {}",
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

struct Imported {
    name: String,
    line: String,
}

struct ImportReport {
    written: Vec<Imported>,
    skipped: Vec<(String, String)>,
    /// `SHELL=`, `PATH=`, `MAILTO=` and friends. A command depending on
    /// cron's `PATH` behaves differently under nightjar.
    environment: Vec<String>,
}

fn import_crontab(paths: &Paths, source: &str, enable: bool) -> ImportReport {
    let mut written = Vec::new();
    let mut skipped = Vec::new();
    let mut environment = Vec::new();

    for raw_line in source.lines() {
        let line = raw_line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if is_env_assignment(line) {
            environment.push(line.to_string());
            continue;
        }
        match import_line(paths, line, enable) {
            Ok(name) => written.push(Imported {
                name,
                line: line.to_string(),
            }),
            Err(e) => skipped.push((line.to_string(), format!("{e:#}"))),
        }
    }

    ImportReport {
        written,
        skipped,
        environment,
    }
}

fn import_line(paths: &Paths, line: &str, enable: bool) -> Result<String> {
    // A bare CR mid-line survives `lines()` and `trim()`. It only surfaces
    // when `list` tries to parse the job file as TOML.
    if let Some(c) = line.chars().find(|c| c.is_control()) {
        bail!(
            "crontab line contains the control character {c:?}, which no job file can hold: \
             {line:?}"
        );
    }
    let tokens: Vec<&str> = line.split_whitespace().collect();
    if tokens.len() < 6 {
        bail!("not a crontab line (need 5 schedule fields and a command): {line:?}");
    }
    let schedule_expr = tokens[0..5].join(" ");
    let command = tokens[5..].join(" ");

    // Reuses `Schedule::parse` instead of a second parser. POSIX and
    // Quartz disagree on whether Sunday is 0 or 1.
    Schedule::parse(&schedule_expr).with_context(|| format!("in crontab line {line:?}"))?;

    let base = sanitize_job_name(basename_of(&command));
    let (name, path) = unique_job_path(paths, &base)?;

    write_job_file_atomic(
        &path,
        &build_import_toml(&command, &schedule_expr, enable, line),
    )?;
    Ok(name)
}

fn basename_of(command: &str) -> &str {
    let first = command.split_whitespace().next().unwrap_or("job");
    Path::new(first)
        .file_name()
        .and_then(|f| f.to_str())
        .unwrap_or("job")
}

/// Everything outside `[A-Za-z0-9_-]` collapses to a single `-`. This also
/// keeps `..` from ever reaching `validate_job_name`.
fn sanitize_job_name(raw: &str) -> String {
    let mut out = String::new();
    let mut last_was_dash = false;
    for c in raw.chars() {
        if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
            out.push(c);
            last_was_dash = c == '-';
        } else if !last_was_dash {
            out.push('-');
            last_was_dash = true;
        }
    }
    let trimmed = out.trim_matches('-');
    if trimmed.is_empty() {
        "job".to_string()
    } else {
        trimmed.to_string()
    }
}

fn unique_job_path(paths: &Paths, base: &str) -> Result<(String, PathBuf)> {
    let mut n = 1u32;
    loop {
        let candidate = if n == 1 {
            base.to_string()
        } else {
            format!("{base}-{n}")
        };
        validate_job_name(&candidate)?;
        let path = paths.jobs_dir.join(format!("{candidate}.toml"));
        if !path.exists() {
            return Ok((candidate, path));
        }
        n += 1;
    }
}

fn is_env_assignment(line: &str) -> bool {
    let Some((name, _)) = line.split_once('=') else {
        return false;
    };
    let mut chars = name.chars();
    match chars.next() {
        Some(c) if c.is_ascii_alphabetic() || c == '_' => {}
        _ => return false,
    }
    chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// Always writes `enabled` explicitly. Omitting it defaults to enabled
/// elsewhere. Import must never default to enabled silently.
fn build_import_toml(command: &str, schedule: &str, enabled: bool, source_line: &str) -> String {
    let mut doc = DocumentMut::new();
    doc["command"] = value(command);
    doc["schedule"] = value(schedule);
    doc["enabled"] = value(enabled);
    format!("# imported from crontab: {source_line}\n{doc}")
}

fn print_report(report: &ImportReport, enable: bool) {
    if report.written.is_empty() {
        println!("no crontab lines needed importing");
    } else if enable {
        println!(
            "imported {} job(s) from your crontab, written enabled:",
            report.written.len()
        );
        for j in &report.written {
            println!("  {} <- {}", j.name, j.line);
        }
        println!(
            "these run under nightjar now — remove or comment them out of your crontab, or they will run twice"
        );
    } else {
        println!(
            "imported {} job(s) from your crontab, written disabled so they do not run twice while cron still has them:",
            report.written.len()
        );
        for j in &report.written {
            println!("  {} <- {}", j.name, j.line);
        }
        println!(
            "review them, then remove them from cron and run `nightjar enable <job>` \
             (or re-run import with --enable) when you're ready"
        );
    }

    if !report.environment.is_empty() {
        println!(
            "\n{} crontab environment line(s) were not imported — nightjar sets these per \
             job, not per file:",
            report.environment.len()
        );
        for line in &report.environment {
            println!("  {line}");
        }
        println!(
            "a job that relied on one of these needs it as `env`, `shell` or `on_failure` in \
             its own file"
        );
    }

    for (line, reason) in &report.skipped {
        eprintln!("import: could not import {line:?}: {reason}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitize_collapses_unsafe_runs_and_strips_leading_trailing_dashes() {
        assert_eq!(sanitize_job_name("backup.sh"), "backup-sh");
        assert_eq!(sanitize_job_name("/usr/bin/x"), "usr-bin-x");
        assert_eq!(sanitize_job_name("already-safe_123"), "already-safe_123");
        assert_eq!(sanitize_job_name("///"), "job");
        assert_eq!(sanitize_job_name(""), "job");
    }

    #[test]
    fn basename_of_takes_first_token_and_strips_directory() {
        assert_eq!(basename_of("/usr/local/bin/backup.sh --full"), "backup.sh");
        assert_eq!(basename_of("true"), "true");
    }

    #[test]
    fn env_assignment_lines_are_recognised_and_ordinary_commands_are_not() {
        assert!(is_env_assignment("SHELL=/bin/bash"));
        assert!(is_env_assignment("MAILTO=me@example.com"));
        assert!(!is_env_assignment("0 2 * * * true"));
        assert!(!is_env_assignment("0 2 * * * FOO=bar ./script.sh"));
    }

    #[test]
    fn unique_job_path_appends_numeric_suffix_when_path_collides() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = Paths::for_root(tmp.path());
        paths.ensure_dirs().unwrap();

        let (first, _) = unique_job_path(&paths, "backup").unwrap();
        std::fs::write(paths.jobs_dir.join(format!("{first}.toml")), "").unwrap();

        let (second, _) = unique_job_path(&paths, "backup").unwrap();
        assert_ne!(first, second);
        assert_eq!(second, "backup-2");
    }

    #[test]
    fn imported_jobs_default_to_disabled() {
        let toml = build_import_toml("true", "0 2 * * *", false, "0 2 * * * true");
        assert!(toml.contains("enabled = false"));
        assert!(!toml.contains("enabled = true"));
    }

    #[test]
    fn enable_flag_writes_enabled_true() {
        let toml = build_import_toml("true", "0 2 * * *", true, "0 2 * * * true");
        assert!(toml.contains("enabled = true"));
    }

    #[test]
    fn source_line_is_preserved_as_leading_comment() {
        let toml = build_import_toml("true", "0 2 * * *", false, "0 2 * * * true # nightly");
        assert!(toml.starts_with("# imported from crontab: 0 2 * * * true # nightly\n"));
    }
}
