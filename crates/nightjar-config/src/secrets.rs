use std::collections::BTreeMap;
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use nightjar_core::format::exit_reason;
use nightjar_core::process::{own_process_group, signal_group};
use zeroize::Zeroizing;

pub type SecretValue = Zeroizing<String>;

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

const RESOLVER_TIMEOUT: Duration = Duration::from_secs(30);

/// # Errors
/// fails if the resolver is missing or unusable, or if it cannot resolve a secret
pub fn resolve(
    secrets: &BTreeMap<String, String>,
    resolver: Option<&str>,
) -> Result<ResolvedSecrets> {
    if secrets.is_empty() {
        return Ok(ResolvedSecrets::default());
    }
    let Some(resolver) = resolver else {
        bail!(
            "secrets are declared but no resolver is configured; set [secrets] resolver in config.toml"
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

fn resolve_one(resolver_template: &str, location: &str, timeout: Duration) -> Result<SecretValue> {
    let command = resolver_template.replace("{}", &nightjar_core::shell::quote(location));
    let shell = nightjar_core::shell::default_shell();

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

    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(child.wait_with_output());
    });

    match rx.recv_timeout(timeout) {
        Ok(Ok(output)) => {
            if !output.status.success() {
                if output.status.code() == Some(127) {
                    bail!(
                        "resolver command not found (exited 127); check that it is on \
                         the daemon's PATH, which under launchd/systemd is often just \
                         /usr/bin:/bin, not your interactive shell's"
                    );
                }
                bail!("resolver ended with {}", exit_reason(&output.status));
            }
            let text = String::from_utf8(output.stdout)
                .map_err(|_| anyhow::anyhow!("resolver produced output that is not valid UTF-8"))?;
            Ok(Zeroizing::new(trim_trailing_newline(text)))
        }
        Ok(Err(e)) => Err(e).context("waiting for resolver"),
        Err(mpsc::RecvTimeoutError::Timeout) => {
            signal_group(pid, libc::SIGKILL);
            bail!("resolver timed out after {}s", timeout.as_secs());
        }
        Err(mpsc::RecvTimeoutError::Disconnected) => {
            bail!("the resolver produced neither output nor an error; this is a bug in nightjar")
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
