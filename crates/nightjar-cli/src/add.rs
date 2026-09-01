use anyhow::{Result, bail};
use nightjar_core::paths::{Paths, validate_job_name};
use nightjar_schedule::Schedule;
use toml_edit::{DocumentMut, value};

pub fn cmd_add(
    name: &str,
    cmd: &str,
    at: &str,
    timeout: Option<&str>,
    catchup: Option<&str>,
) -> Result<i32> {
    validate_job_name(name)?;
    Schedule::parse(at)?;

    let paths = Paths::resolve()?;
    paths.ensure_dirs()?;
    let path = paths.job_file(name)?;
    if path.exists() {
        bail!(
            "job {name:?} already exists at {} — use `nightjar edit {name}` to change it",
            path.display()
        );
    }

    nightjar_config::jobfile::write_job_file_atomic(
        &path,
        &validated_job_toml(name, cmd, at, timeout, catchup)?,
    )?;

    println!("wrote {}", path.display());
    println!(
        "run `nightjar daemon` (or `nightjar service install` to survive reboot) for {name} to actually fire"
    );
    Ok(0)
}

/// The file `add` is about to write, checked the way the daemon will
/// check it. Anything `Job::load` would refuse is refused here first, so
/// `add` never leaves behind a job that shows up as `invalid`.
fn validated_job_toml(
    name: &str,
    cmd: &str,
    at: &str,
    timeout: Option<&str>,
    catchup: Option<&str>,
) -> Result<String> {
    let toml = build_job_toml(cmd, at, timeout, catchup);
    nightjar_config::Job::from_toml_str(name, &toml)?;
    Ok(toml)
}

fn build_job_toml(cmd: &str, at: &str, timeout: Option<&str>, catchup: Option<&str>) -> String {
    let mut doc = DocumentMut::new();
    doc["command"] = value(cmd);
    doc["schedule"] = value(at);
    if let Some(t) = timeout {
        doc["timeout"] = value(t);
    }
    if let Some(c) = catchup {
        doc["catchup"] = value(c);
    }
    doc.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_job_toml_produces_only_command_and_schedule_when_call_is_minimal() {
        let toml = build_job_toml("echo hi", "hourly", None, None);
        assert_eq!(toml, "command = \"echo hi\"\nschedule = \"hourly\"\n");
    }

    #[test]
    fn add_refuses_a_timeout_the_job_file_could_not_load_with() {
        let err = validated_job_toml("j", "true", "hourly", Some("5 minutes"), None)
            .unwrap_err()
            .to_string();
        assert!(err.contains("timeout"), "got: {err}");
    }

    #[test]
    fn add_refuses_a_catchup_value_outside_none_once_all() {
        let err = validated_job_toml("j", "true", "hourly", None, Some("sometimes")).unwrap_err();
        assert!(format!("{err:#}").contains("catchup"), "got: {err:#}");
    }

    #[test]
    fn add_accepts_a_job_the_daemon_would_load() {
        let toml = validated_job_toml("j", "true", "hourly", Some("30m"), Some("all")).unwrap();
        assert!(toml.contains("catchup = \"all\""));
    }

    #[test]
    fn build_job_toml_includes_optional_flags_when_they_are_given() {
        let toml = build_job_toml("echo hi", "hourly", Some("30m"), Some("once"));
        assert!(toml.contains("timeout = \"30m\""));
        assert!(toml.contains("catchup = \"once\""));
    }
}
