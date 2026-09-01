use anyhow::{Context, Result, bail};
use nightjar_core::process::{own_process_group, signal_group};
use std::collections::BTreeMap;
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::Duration;
use zeroize::Zeroizing;

/// Zeroized on drop, which only protects this process's own copy — once
/// written into the child's environment, only the child's exit clears that
/// one.
pub type SecretValue = Zeroizing<String>;

/// What resolving a job's `[secrets]` hands to `runner::exec::execute`.
/// `env` and `redact` are independent views so a later stage needing "every
/// value to scrub" never has to guess which env vars came from a secret.
#[derive(Default)]
pub struct ResolvedSecrets {
    pub env: Vec<(String, SecretValue)>,
    pub redact: Vec<SecretValue>,
}

impl ResolvedSecrets {
    fn push(&mut self, var: String, value: SecretValue) {
        self.redact.push(value.clone());
        self.env.push((var, value));
    }
}

/// Long enough for a networked backend (1Password, Vault) to answer; short
/// enough that a resolver blocked on an interactive prompt — its own sign-in,
/// say — fails the run instead of wedging `nightjar exec` forever.
const RESOLVER_TIMEOUT: Duration = Duration::from_secs(30);

pub fn resolve(
    secrets: &BTreeMap<String, String>,
    resolver: Option<&str>,
) -> Result<ResolvedSecrets> {
    if secrets.is_empty() {
        return Ok(ResolvedSecrets::default());
    }
    let Some(resolver) = resolver else {
        bail!(
            "secrets are declared but no resolver is configured — set [secrets] resolver in config.toml"
        );
    };
    if !resolver.contains("{}") {
        bail!(
            "secrets.resolver {resolver:?} has no \"{{}}\" placeholder for the secret's own location"
        );
    }

    let mut out = ResolvedSecrets::default();
    for (var, location) in secrets {
        let value = resolve_one(resolver, location, RESOLVER_TIMEOUT)
            .with_context(|| format!("secret {var:?}"))?;
        out.push(var.clone(), value);
    }
    Ok(out)
}

/// Runs the resolver command for one secret. The location is quoted as
/// one shell word: `op://Personal/My Vault/password` reaches `op read`
/// intact, and a location can never end the resolver command and start
/// another.
///
/// Returns the resolver's stdout minus a single trailing newline — the same convention shell `$(...)` uses, since
/// `resolver = "op read {}"` is written and read the same way a command
/// substitution would be.
///
/// stderr is never inspected, not even on failure. It routinely contains
/// the secret itself or a prompt for it, so it must never reach an error
/// message or a log.
fn resolve_one(resolver_template: &str, location: &str, timeout: Duration) -> Result<SecretValue> {
    let command = resolver_template.replace("{}", &nightjar_core::shell::quote(location));
    let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".to_string());

    let mut cmd = Command::new(&shell);
    cmd.arg("-c")
        .arg(&command)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    own_process_group(&mut cmd);

    let child = cmd
        .spawn()
        .with_context(|| format!("starting resolver via {shell} -c"))?;
    let pid = child.id();

    // `wait_with_output` drains stdout and stderr on its own threads while
    // waiting, so a resolver that writes enough to fill a pipe cannot
    // deadlock against a reader that has not gotten to it yet. Run on a
    // helper thread so this function can still enforce `timeout`, which
    // `wait_with_output` has no way to do on its own.
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(child.wait_with_output());
    });

    match rx.recv_timeout(timeout) {
        Ok(Ok(output)) => {
            if !output.status.success() {
                // Under launchd/systemd the daemon's own PATH is often just
                // `/usr/bin:/bin`, nothing like an interactive shell's — so a
                // resolver that works by hand (`op`, `aws`, a project-local
                // script) can be entirely invisible to it. Exit 127 is the
                // shell's own signal for "command not found", never
                // something the resolver command itself chose to return, so
                // naming that case precisely (rather than a bare exit
                // status) is safe: it reports our own exit code, nothing
                // the resolver said.
                if output.status.code() == Some(127) {
                    bail!(
                        "resolver command not found (exited 127) — check that it is on \
                         the daemon's PATH, which under launchd/systemd is often just \
                         /usr/bin:/bin, not your interactive shell's"
                    );
                }
                bail!("resolver exited with {}", output.status);
            }
            let text = String::from_utf8(output.stdout)
                .map_err(|_| anyhow::anyhow!("resolver produced output that is not valid UTF-8"))?;
            Ok(Zeroizing::new(trim_trailing_newline(text)))
        }
        Ok(Err(e)) => Err(e).context("waiting for resolver"),
        Err(mpsc::RecvTimeoutError::Timeout) => {
            // The helper thread's own `wait_with_output` reaps the child once
            // this kills it; nothing here needs to join that thread.
            signal_group(pid, libc::SIGKILL);
            bail!("resolver timed out after {}s", timeout.as_secs());
        }
        Err(mpsc::RecvTimeoutError::Disconnected) => {
            bail!("resolver's helper thread ended without a result")
        }
    }
}

fn trim_trailing_newline(mut s: String) -> String {
    if s.ends_with('\n') {
        s.pop();
        if s.ends_with('\r') {
            s.pop();
        }
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    fn map(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    #[test]
    fn resolve_yields_nothing_and_never_needs_a_resolver_when_there_are_no_secrets() {
        let out = resolve(&BTreeMap::new(), None).unwrap();
        assert!(out.env.is_empty());
        assert!(out.redact.is_empty());
    }

    #[test]
    fn secret_resolves_via_the_configured_command() {
        let secrets = map(&[("PGPASSWORD", "hunter2")]);
        let out = resolve(&secrets, Some("echo {}")).unwrap();

        assert_eq!(out.env.len(), 1);
        assert_eq!(out.env[0].0, "PGPASSWORD");
        assert_eq!(out.env[0].1.as_str(), "hunter2");
        assert_eq!(out.redact.len(), 1);
        assert_eq!(out.redact[0].as_str(), "hunter2");
    }

    #[test]
    fn location_reaches_the_resolver_as_one_word_when_it_contains_spaces() {
        let secrets = map(&[("PW", "op://Personal/My Vault/password")]);
        let out = resolve(&secrets, Some("printf %s {}")).unwrap();
        assert_eq!(out.env[0].1.as_str(), "op://Personal/My Vault/password");
    }

    #[test]
    fn location_cannot_end_the_resolver_command_and_run_its_own() {
        let dir = tempfile::tempdir().unwrap();
        let marker = dir.path().join("pwned");
        let location = format!("x; touch {}", marker.display());
        let secrets = map(&[("PW", location.as_str())]);

        let out = resolve(&secrets, Some("printf %s {}")).unwrap();

        assert_eq!(out.env[0].1.as_str(), location);
        assert!(
            !marker.exists(),
            "the location must be data, never a second command"
        );
    }

    #[test]
    fn trailing_newline_is_trimmed_like_shell_command_substitution() {
        assert_eq!(trim_trailing_newline("value\n".to_string()), "value");
        assert_eq!(trim_trailing_newline("value\r\n".to_string()), "value");
        assert_eq!(trim_trailing_newline("value".to_string()), "value");
        assert_eq!(
            trim_trailing_newline("value\n\n".to_string()),
            "value\n",
            "only one trailing newline is stripped, as a shell would"
        );
    }

    #[test]
    fn secret_resolution_fails_when_declared_but_no_resolver_is_configured() {
        let secrets = map(&[("PGPASSWORD", "op://vault/db/password")]);
        let err = resolve(&secrets, None).map(|_| ()).unwrap_err().to_string();
        assert!(err.contains("resolver"), "got: {err}");
    }

    #[test]
    fn resolver_template_is_rejected_when_it_is_missing_the_placeholder() {
        let secrets = map(&[("PGPASSWORD", "op://vault/db/password")]);
        let err = resolve(&secrets, Some("op read"))
            .map(|_| ())
            .unwrap_err()
            .to_string();
        assert!(err.contains("{}"), "got: {err}");
    }

    #[test]
    fn resolver_names_the_secret_but_not_the_stderr_when_it_fails() {
        let secrets = map(&[("PGPASSWORD", "whatever")]);
        let err = resolve(&secrets, Some("echo leaked-stderr-text >&2; exit 7 #{}"))
            .map(|_| ())
            .unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("PGPASSWORD"), "got: {msg}");
        assert!(
            !msg.contains("leaked-stderr-text"),
            "resolver stderr must never reach the error message; got: {msg}"
        );
    }

    #[test]
    fn resolver_command_is_reported_precisely_not_as_a_bare_exit_status_when_it_is_not_found() {
        let err = resolve_one(
            "nightjar-test-nonexistent-resolver-binary {}",
            "x",
            Duration::from_secs(5),
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("not found"), "got: {err}");
        assert!(err.to_lowercase().contains("path"), "got: {err}");
    }

    #[test]
    fn resolver_times_out_rather_than_hanging_forever_when_it_never_exits() {
        let started = std::time::Instant::now();
        let err = resolve_one("sleep 30; echo {}", "x", Duration::from_millis(200))
            .unwrap_err()
            .to_string();
        assert!(err.contains("timed out"), "got: {err}");
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "must not wait out the sleep; took {:?}",
            started.elapsed()
        );
    }

    #[test]
    fn second_secret_is_not_resolved_when_an_earlier_one_fails() {
        let secrets = map(&[("A_FAILS", "x"), ("B_NEVER_RUN", "y")]);
        let err = resolve(&secrets, Some("exit 1 #{}"))
            .map(|_| ())
            .unwrap_err()
            .to_string();
        assert!(err.contains("A_FAILS"), "got: {err}");
    }

    #[test]
    fn secret_values_are_zeroized_when_the_child_has_been_spawned() {
        let mut value: SecretValue = Zeroizing::new("supersecretvalue123".to_string());
        assert_eq!(value.as_str(), "supersecretvalue123");

        zeroize::Zeroize::zeroize(&mut *value);

        assert!(
            value.is_empty() || value.bytes().all(|b| b == 0),
            "secret bytes must not survive in memory once zeroized; got {:?}",
            value.as_str()
        );
    }
}
