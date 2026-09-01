use anyhow::{Context, Result, bail};
use nightjar_core::paths::Paths;

use crate::job::Job;
use std::path::{Path, PathBuf};
use toml_edit::DocumentMut;

fn existing_job_path(paths: &Paths, name: &str) -> Result<PathBuf> {
    let path = paths.job_file(name)?;
    if !path.exists() {
        bail!("no such job: {name} (expected {})", path.display());
    }
    Ok(path)
}

pub fn cmd_rm(name: &str) -> Result<i32> {
    let paths = Paths::resolve()?;
    let path = existing_job_path(&paths, name)?;
    std::fs::remove_file(&path).with_context(|| format!("removing {}", path.display()))?;
    println!("removed {}", path.display());
    Ok(0)
}

pub fn cmd_enable(name: &str) -> Result<i32> {
    set_enabled(name, true)
}

pub fn cmd_disable(name: &str) -> Result<i32> {
    set_enabled(name, false)
}

fn set_enabled(name: &str, enabled: bool) -> Result<i32> {
    let paths = Paths::resolve()?;
    let path = existing_job_path(&paths, name)?;
    write_enabled(&path, enabled)?;
    println!("{name}: {}", if enabled { "enabled" } else { "disabled" });
    Ok(0)
}

/// Reads `enabled` at TOML-syntax level only, never `Job::load`'s full
/// schema check: a job whose schedule `Job::load` rejects is exactly the
/// job a user most wants to disable, and `nightjar disable` must still
/// work on it. Absent key defaults to `true`, matching
/// `config::job::default_true`.
pub fn read_enabled(path: &Path) -> Result<bool> {
    let text =
        std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    let doc: DocumentMut = text
        .parse()
        .with_context(|| format!("parsing {}", path.display()))?;
    Ok(doc
        .get("enabled")
        .and_then(toml_edit::Item::as_bool)
        .unwrap_or(true))
}

/// Edits `enabled` with `toml_edit` rather than loading the `Job` and
/// re-serializing it. A serde round-trip would drop the user's comments and
/// formatting. Shared by `nightjar enable`/`disable` and the TUI's `d` key,
/// so the toggle has exactly one implementation.
pub fn write_enabled(path: &Path, enabled: bool) -> Result<()> {
    let text =
        std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    let mut doc: DocumentMut = text
        .parse()
        .with_context(|| format!("parsing {}", path.display()))?;
    doc["enabled"] = toml_edit::value(enabled);

    write_job_file_atomic(path, &doc.to_string())
}

/// Re-validates after the editor exits but never reverts: a bad edit stays
/// on disk, fixable by running `edit` again.
///
/// `$EDITOR` is handed a scratch copy, never the live path: `nano`, `sed -i`
/// and several GUI editors truncate in place, and the daemon polling the
/// jobs directory could read a job mid-save.
pub fn cmd_edit(name: &str) -> Result<i32> {
    let paths = Paths::resolve()?;
    let path = existing_job_path(&paths, name)?;

    let editor = std::env::var_os("EDITOR")
        .filter(|e| !e.is_empty())
        .context("set $EDITOR to edit a job file, e.g. `export EDITOR=vim`")?;

    let original =
        std::fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
    // In the config directory, not the jobs directory or `/tmp`: the only
    // place that is both the user's own and not scanned by the daemon.
    let scratch = Scratch(
        paths
            .config_dir
            .join(format!(".edit-{name}-{}.toml", uuid::Uuid::now_v7())),
    );
    std::fs::write(&scratch.0, &original)
        .with_context(|| format!("writing {}", scratch.0.display()))?;

    let status = editor_command(&editor, &scratch.0)
        .status()
        .with_context(|| format!("running {editor:?} on {}", scratch.0.display()))?;
    if !status.success() {
        bail!("{editor:?} exited with {status}");
    }

    let edited = std::fs::read_to_string(&scratch.0)
        .with_context(|| format!("reading back {}", scratch.0.display()))?;
    if edited == original {
        println!("{}: unchanged", path.display());
        return Ok(0);
    }
    write_job_file_atomic(&path, &edited)?;

    Job::load(&path).with_context(|| {
        format!(
            "{} was saved but does not parse; fix it and run `nightjar edit {name}` again",
            path.display()
        )
    })?;
    println!("saved {}", path.display());
    Ok(0)
}

/// `EDITOR="code --wait"` and `EDITOR="vim -u NONE"` are ordinary
/// settings, and git, crontab, and sudoedit all honour the arguments. A
/// value with whitespace goes through `sh -c` with the file as `$1`;
/// a bare program is run directly, so a non-UTF-8 path still works.
fn editor_command(editor: &std::ffi::OsStr, file: &Path) -> std::process::Command {
    match editor.to_str() {
        Some(text) if text.chars().any(char::is_whitespace) => {
            let mut cmd = std::process::Command::new("/bin/sh");
            cmd.arg("-c")
                .arg(format!("{text} \"$1\""))
                .arg("nightjar-edit")
                .arg(file);
            cmd
        }
        _ => {
            let mut cmd = std::process::Command::new(editor);
            cmd.arg(file);
            cmd
        }
    }
}

struct Scratch(PathBuf);

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

/// Writes to a sibling temp file then `rename`s it in — atomic on the same
/// filesystem, unlike a direct write the polling daemon could see truncated.
pub fn write_job_file_atomic(path: &Path, contents: &str) -> Result<()> {
    let dir = path
        .parent()
        .with_context(|| format!("{} has no parent directory", path.display()))?;
    let file_name = path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("job.toml");
    let tmp_path = dir.join(format!(".{file_name}.tmp-{}", uuid::Uuid::now_v7()));

    std::fs::write(&tmp_path, contents)
        .with_context(|| format!("writing {}", tmp_path.display()))?;
    std::fs::rename(&tmp_path, path).with_context(|| format!("saving {}", path.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::OsStr;

    fn argv(cmd: &std::process::Command) -> Vec<String> {
        std::iter::once(cmd.get_program())
            .chain(cmd.get_args())
            .map(|a| a.to_string_lossy().into_owned())
            .collect()
    }

    #[test]
    fn editor_runs_directly_with_the_file_when_it_is_a_bare_program() {
        let cmd = editor_command(OsStr::new("vim"), Path::new("/tmp/j.toml"));
        assert_eq!(argv(&cmd), ["vim", "/tmp/j.toml"]);
    }

    #[test]
    fn editor_runs_through_the_shell_with_the_file_as_dollar_one_when_it_has_arguments() {
        let cmd = editor_command(OsStr::new("code --wait"), Path::new("/tmp/j.toml"));
        assert_eq!(
            argv(&cmd),
            [
                "/bin/sh",
                "-c",
                "code --wait \"$1\"",
                "nightjar-edit",
                "/tmp/j.toml"
            ]
        );
    }

    #[test]
    fn editor_with_arguments_really_receives_the_file_last() {
        let tmp = tempfile::tempdir().unwrap();
        let file = tmp.path().join("j.toml");
        let marker = tmp.path().join("seen");
        std::fs::write(&file, "x").unwrap();
        // Inner `sh -c` sees `--` as `$0`, so the file lands in `$1`.
        let editor = format!("sh -c 'printf %s \"$1\" > {}' --", marker.display());

        let status = editor_command(OsStr::new(&editor), &file).status().unwrap();

        assert!(status.success());
        assert_eq!(
            std::fs::read_to_string(&marker).unwrap(),
            file.display().to_string(),
            "the file path must reach the editor after its own arguments"
        );
    }

    #[test]
    fn existing_job_path_names_the_missing_job() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = Paths::for_root(tmp.path());
        std::fs::create_dir_all(&paths.jobs_dir).unwrap();

        let err = existing_job_path(&paths, "ghost").unwrap_err();
        assert!(err.to_string().contains("ghost"));
    }

    #[test]
    fn read_enabled_reflects_the_value_when_it_is_explicitly_set() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("j.toml");
        std::fs::write(
            &path,
            "command = \"true\"\nschedule = \"hourly\"\nenabled = false\n",
        )
        .unwrap();
        assert!(!read_enabled(&path).unwrap());
    }

    #[test]
    fn read_enabled_defaults_to_true_when_absent() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("j.toml");
        std::fs::write(&path, "command = \"true\"\nschedule = \"hourly\"\n").unwrap();
        assert!(read_enabled(&path).unwrap());
    }

    #[test]
    fn read_enabled_succeeds_when_the_file_is_schema_invalid_but_syntactically_fine() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("badsched.toml");
        std::fs::write(
            &path,
            "command = \"true\"\nschedule = \"@nonsense\"\nenabled = true\n",
        )
        .unwrap();

        assert!(
            crate::job::Job::load(&path).is_err(),
            "precondition: Job::load must reject this file"
        );
        assert!(
            read_enabled(&path).unwrap(),
            "read_enabled must not require what Job::load requires"
        );
    }

    #[test]
    fn read_enabled_rejects_the_file_when_it_has_a_toml_syntax_error() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("broken.toml");
        std::fs::write(&path, "command = = =\n").unwrap();
        assert!(read_enabled(&path).is_err());
    }
}
