use nightjar_remote::{HostOutcome, HostResult};
use serde_json::Value;

/// Bump only when a field's meaning changes in a way older code would
/// misread. Adding a field needs no bump — a `Value` lookup by key
/// already treats a missing key as `None`.
pub(crate) const SCHEMA_VERSION: u64 = 1;

/// `HostOutcome::Success` only means ssh got an exit code back. The
/// payload can still be a login banner or MOTD printed ahead of the
/// real JSON, so parsing must never panic on what it finds.
pub(crate) enum HostPayload {
    Ok(Value),
    Unreachable,
    MissingBinary,
    Malformed,
}

pub(crate) struct HostView {
    pub host: String,
    pub payload: HostPayload,
    /// 0 for `Unreachable`/`MissingBinary`, since neither ran a real
    /// command. Kept separate from `payload` because a non-zero exit can
    /// still parse into a normal `HostPayload::Ok`, and `any_problem`
    /// needs both facts.
    pub remote_exit_code: i32,
}

fn classify(outcome: HostOutcome) -> (HostPayload, i32) {
    match outcome {
        HostOutcome::Unreachable => (HostPayload::Unreachable, 0),
        HostOutcome::MissingBinary => (HostPayload::MissingBinary, 0),
        HostOutcome::Success(text, exit_code) => {
            let payload = match serde_json::from_str::<Value>(&text) {
                Ok(v) if v.is_object() => HostPayload::Ok(v),
                _ => HostPayload::Malformed,
            };
            (payload, exit_code)
        }
    }
}

pub(crate) fn skew_warning(host: &str, value: &Value) -> Option<String> {
    match value.get("schema").and_then(Value::as_u64) {
        Some(s) if s < SCHEMA_VERSION => Some(format!(
            "warning: {host} is running an older nightjar (schema {s}, local is \
             {SCHEMA_VERSION}); rendering what it can"
        )),
        Some(s) if s > SCHEMA_VERSION => Some(format!(
            "warning: {host} is running a newer nightjar (schema {s}, local is \
             {SCHEMA_VERSION}); fields nightjar does not recognize are ignored"
        )),
        Some(_) => None,
        None => Some(format!(
            "warning: {host}'s response has no schema field; treating it as older than \
             local schema {SCHEMA_VERSION}"
        )),
    }
}

/// Warnings print to stderr, never stdout, so `--host ... --json | jq`
/// stays clean even against a fleet running mixed versions.
pub(crate) fn collect(results: Vec<HostResult>) -> Vec<HostView> {
    results
        .into_iter()
        .map(|r| {
            let (payload, remote_exit_code) = classify(r.outcome);
            if let HostPayload::Ok(v) = &payload
                && let Some(warning) = skew_warning(&r.host, v)
            {
                eprintln!("{warning}");
            }
            HostView {
                host: r.host,
                payload,
                remote_exit_code,
            }
        })
        .collect()
}

/// Distinct wording per cause, matching how `remote::HostOutcome`
/// already keeps `Unreachable` and `MissingBinary` apart.
pub(crate) fn problem_label(payload: &HostPayload) -> Option<&'static str> {
    match payload {
        HostPayload::Ok(_) => None,
        HostPayload::Unreachable => Some("unreachable"),
        HostPayload::MissingBinary => Some("no nightjar on remote PATH"),
        HostPayload::Malformed => Some("response was not valid JSON"),
    }
}

/// Matches what `ssh <host> nightjar <cmd>` run by hand would exit:
/// unreachable, missing binary, unparseable output, or a nonzero remote
/// exit all count. A schema mismatch alone does not — it only warns.
pub(crate) fn any_problem(views: &[HostView]) -> bool {
    views
        .iter()
        .any(|v| problem_label(&v.payload).is_some() || v.remote_exit_code != 0)
}

/// Spreads the host's fields in rather than nesting them under a
/// `"data"` key. This keeps a single-host `jq` filter (`.jobs`,
/// `.daemon`) working one level down from `.hosts[]`, unchanged.
fn host_entry_json(view: &HostView) -> Value {
    match &view.payload {
        HostPayload::Ok(v) => {
            // `classify` only builds `Ok` from an object, so this is
            // always `Some`. The fallback avoids a panic if that ever
            // stops holding.
            let mut map = v.as_object().cloned().unwrap_or_default();
            map.insert("host".to_string(), Value::String(view.host.clone()));
            map.insert("ok".to_string(), Value::Bool(true));
            Value::Object(map)
        }
        other => {
            let label = problem_label(other).unwrap_or("unknown error");
            serde_json::json!({ "host": view.host, "ok": false, "error": label })
        }
    }
}

/// Uses `Value`, not string formatting like other renderers here. These
/// fields come from other hosts, already escaped — concatenating them by
/// hand risks double-escaping or invalid JSON from an unexpected byte.
pub(crate) fn merged_json(views: &[HostView]) -> String {
    let hosts: Vec<Value> = views.iter().map(host_entry_json).collect();
    serde_json::json!({ "schema": SCHEMA_VERSION, "hosts": hosts }).to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn host_result(host: &str, outcome: HostOutcome) -> HostResult {
        HostResult {
            host: host.to_string(),
            outcome,
        }
    }

    #[test]
    fn host_classifies_as_ok_with_no_problem_and_no_warning_when_it_is_healthy() {
        let outcome = HostOutcome::Success(
            format!(r#"{{"schema":{SCHEMA_VERSION},"jobs":[]}}"#).to_string(),
            0,
        );
        let views = collect(vec![host_result("web1", outcome)]);
        assert!(!any_problem(&views));
        assert!(matches!(views[0].payload, HostPayload::Ok(_)));
    }

    #[test]
    fn host_is_problem_even_though_it_parses_when_its_remote_command_exits_nonzero() {
        let outcome = HostOutcome::Success(r#"{"schema":1,"jobs":[]}"#.to_string(), 1);
        let views = collect(vec![host_result("web1", outcome)]);

        assert!(
            any_problem(&views),
            "a nonzero remote exit must flip the merged exit code"
        );
        assert!(
            matches!(views[0].payload, HostPayload::Ok(_)),
            "the payload still parsed and still renders a normal row"
        );
    }

    #[test]
    fn host_is_problem_with_precise_label_when_it_is_unreachable() {
        let views = collect(vec![host_result("web1", HostOutcome::Unreachable)]);
        assert!(any_problem(&views));
        assert_eq!(problem_label(&views[0].payload), Some("unreachable"));
    }

    #[test]
    fn remote_warns_and_renders_what_it_can_when_reporting_an_older_schema() {
        let older = serde_json::json!({"schema": SCHEMA_VERSION - 1, "jobs": []});
        let warning = skew_warning("web1", &older).expect("an older schema must warn");
        assert!(warning.contains("older"), "got: {warning}");

        let views = vec![HostView {
            host: "web1".to_string(),
            payload: HostPayload::Ok(older),
            remote_exit_code: 0,
        }];
        assert!(!any_problem(&views));
        let merged = merged_json(&views);
        let parsed: Value = serde_json::from_str(&merged).unwrap();
        assert_eq!(parsed["hosts"][0]["ok"], Value::Bool(true));
    }

    #[test]
    fn remote_warns_and_still_renders_when_reporting_no_schema_field_at_all() {
        let schemaless = serde_json::json!({"jobs": [{"job": "backup"}]});
        let warning = skew_warning("web1", &schemaless).expect("a missing schema field must warn");
        assert!(warning.contains("no schema field"), "got: {warning}");

        let views = vec![HostView {
            host: "web1".to_string(),
            payload: HostPayload::Ok(schemaless),
            remote_exit_code: 0,
        }];
        assert!(!any_problem(&views));
        let merged = merged_json(&views);
        let parsed: Value = serde_json::from_str(&merged).unwrap();
        assert_eq!(parsed["hosts"][0]["jobs"][0]["job"], "backup");
    }

    #[test]
    fn remote_warns_and_does_not_misreport_when_reporting_a_newer_schema() {
        let newer = serde_json::json!({
            "schema": SCHEMA_VERSION + 1,
            "jobs": [{"job": "backup", "status": "success"}],
            "a_field_from_the_future": {"anything": "at all"},
        });
        let warning = skew_warning("web2", &newer).expect("a newer schema must warn");
        assert!(warning.contains("newer"), "got: {warning}");

        let views = vec![HostView {
            host: "web2".to_string(),
            payload: HostPayload::Ok(newer),
            remote_exit_code: 0,
        }];
        let merged = merged_json(&views);
        let parsed: Value = serde_json::from_str(&merged).unwrap();
        assert_eq!(parsed["hosts"][0]["jobs"][0]["job"], "backup");
        assert!(parsed["hosts"][0]["a_field_from_the_future"].is_object());
    }

    #[test]
    fn remote_produces_problem_row_not_panic_when_response_is_malformed_json() {
        let banner_then_json = "Welcome to Ubuntu 24.04.2 LTS\nLast login: Tue Aug 25 09:14:02 2026\n\
             {\"schema\":1,\"jobs\":[]}\n";
        let views = collect(vec![host_result(
            "web3",
            HostOutcome::Success(banner_then_json.to_string(), 0),
        )]);

        assert!(
            any_problem(&views),
            "a banner ahead of the JSON must not be read as a clean success"
        );
        assert_eq!(
            problem_label(&views[0].payload),
            Some("response was not valid JSON")
        );

        let merged = merged_json(&views);
        let parsed: Value =
            serde_json::from_str(&merged).expect("merged output must still be valid JSON");
        assert_eq!(parsed["hosts"][0]["host"], "web3");
        assert_eq!(parsed["hosts"][0]["ok"], Value::Bool(false));
    }

    #[test]
    fn classifier_does_not_panic_when_payload_is_adversarial() {
        let payloads = [
            String::new(),
            "{".to_string(),
            "}".to_string(),
            "null".to_string(),
            "\"just a string\"".to_string(),
            "[".repeat(10_000),
            "\u{0}\u{0}\u{0}".to_string(),
            "{\"schema\":".to_string(),
        ];
        for payload in payloads {
            let views = collect(vec![host_result("web4", HostOutcome::Success(payload, 0))]);
            assert!(any_problem(&views));
            let merged = merged_json(&views);
            assert!(serde_json::from_str::<Value>(&merged).is_ok());
        }
    }

    #[test]
    fn merged_output_is_valid_json() {
        let views = collect(vec![
            host_result(
                "web1",
                HostOutcome::Success(r#"{"schema":1,"jobs":[{"job":"backup"}]}"#.to_string(), 0),
            ),
            host_result("web2", HostOutcome::Unreachable),
            host_result("web3", HostOutcome::MissingBinary),
            host_result(
                "web4",
                HostOutcome::Success("garbage before the payload {}".to_string(), 0),
            ),
        ]);

        let merged = merged_json(&views);
        let parsed: Value = serde_json::from_str(&merged).expect("must parse as JSON");
        let hosts = parsed["hosts"].as_array().expect("hosts must be an array");
        assert_eq!(hosts.len(), 4);
        assert_eq!(parsed["schema"], SCHEMA_VERSION);
        for host in hosts {
            assert!(host.get("host").is_some());
            assert!(host.get("ok").is_some());
        }
    }
}
