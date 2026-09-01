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
        &build_job_toml(cmd, at, timeout, catchup),
    )?;

    println!("wrote {}", path.display());
    println!(
        "run `nightjar daemon` (or `nightjar service install` to survive reboot) for {name} to actually fire"
    );
    Ok(0)
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
    fn build_job_toml_includes_optional_flags_when_they_are_given() {
        let toml = build_job_toml("echo hi", "hourly", Some("30m"), Some("once"));
        assert!(toml.contains("timeout = \"30m\""));
        assert!(toml.contains("catchup = \"once\""));
    }
}
