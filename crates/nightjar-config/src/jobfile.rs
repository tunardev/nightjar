use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use nightjar_core::format::exit_reason;
use nightjar_core::guidance::no_such_job;
use nightjar_core::paths::Paths;
use toml_edit::DocumentMut;

use crate::job::Job;

fn existing_job_path(paths: &Paths, name: &str) -> Result<PathBuf> {
    let path = paths.job_file(name)?;
    if !path.exists() {
        bail!("{}", no_such_job(name, &path));
    }
    Ok(path)
}

/// # Errors
/// fails if no job file exists for `name`, or if it cannot be removed
pub fn cmd_rm(name: &str) -> Result<i32> {
    let paths = Paths::resolve()?;
    let path = existing_job_path(&paths, name)?;
    std::fs::remove_file(&path).with_context(|| format!("removing {}", path.display()))?;
    println!("removed {}", path.display());
    Ok(0)
}

/// # Errors
/// fails if no job file exists for `name`, or if it cannot be rewritten
pub fn cmd_enable(name: &str) -> Result<i32> {
    set_enabled(name, true)
}

/// # Errors
/// fails if no job file exists for `name`, or if it cannot be rewritten
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

/// # Errors
/// fails if `path` cannot be read or does not parse as toml
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

/// # Errors
/// fails if `path` cannot be read, does not parse, or cannot be replaced atomically
pub fn write_enabled(path: &Path, enabled: bool) -> Result<()> {
    let text =
        std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    let mut doc: DocumentMut = text
        .parse()
        .with_context(|| format!("parsing {}", path.display()))?;
    doc["enabled"] = toml_edit::value(enabled);

    write_job_file_atomic(path, &doc.to_string())
}

/// # Errors
/// fails if `$EDITOR` is unset, the editor exits non-zero, or the saved file no longer parses
pub fn cmd_edit(name: &str) -> Result<i32> {
    let paths = Paths::resolve()?;
    let path = existing_job_path(&paths, name)?;

    let editor = std::env::var_os("EDITOR")
        .filter(|e| !e.is_empty())
        .context("set $EDITOR to edit a job file, e.g. `export EDITOR=vim`")?;

    let original =
        std::fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
    let scratch = ScratchCopy {
        path: paths
            .config_dir
            .join(format!(".edit-{name}-{}.toml", uuid::Uuid::now_v7())),
    };
    std::fs::write(&scratch.path, &original)
        .with_context(|| format!("writing {}", scratch.path.display()))?;

    let status = editor_command(&editor, &scratch.path)
        .status()
        .with_context(|| format!("running {} on {}", editor.display(), scratch.path.display()))?;
    if !status.success() {
        bail!(
            "{} ended with {}, so {} is unchanged",
            editor.display(),
            exit_reason(&status),
            path.display()
        );
    }

    let edited = std::fs::read_to_string(&scratch.path)
        .with_context(|| format!("reading back {}", scratch.path.display()))?;
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

struct ScratchCopy {
    path: PathBuf,
}

impl Drop for ScratchCopy {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

/// # Errors
/// fails if the temporary file cannot be created beside `path` or renamed over it
pub fn write_job_file_atomic(path: &Path, contents: &str) -> Result<()> {
    let dir = path
        .parent()
        .with_context(|| format!("{} has no parent directory", path.display()))?;
    let file_name = path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("job.toml");
    let tmp_path = dir.join(format!(".{file_name}.tmp-{}", uuid::Uuid::now_v7()));

    let mode_to_preserve = mode_of(path);
    create_new_with_mode(&tmp_path, contents, mode_to_preserve)
        .with_context(|| format!("writing {}", tmp_path.display()))?;
    std::fs::rename(&tmp_path, path).with_context(|| format!("saving {}", path.display()))?;
    Ok(())
}

fn mode_of(path: &Path) -> Option<u32> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path)
        .ok()
        .map(|meta| meta.permissions().mode())
}

fn create_new_with_mode(path: &Path, contents: &str, mode: Option<u32>) -> std::io::Result<()> {
    use std::io::Write;
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};

    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    if let Some(mode) = mode {
        options.mode(mode);
    }
    options.open(path)?.write_all(contents.as_bytes())?;

    if let Some(mode) = mode {
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::ffi::OsStr;

    use super::*;

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
    fn toggling_enabled_keeps_the_job_file_as_private_as_the_user_left_it() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("private.toml");
        std::fs::write(&path, "command = \"true\"\nschedule = \"hourly\"\n").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();

        write_enabled(&path, false).unwrap();

        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(
            mode, 0o600,
            "a job file can hold [env] values; disabling it must not widen who can read it"
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
