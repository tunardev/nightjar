fn survives_a_shell_unquoted(token: &str) -> bool {
    !token.is_empty()
        && token.bytes().all(|byte| {
            byte.is_ascii_alphanumeric()
                || matches!(
                    byte,
                    b'-' | b'_' | b'.' | b'/' | b':' | b'@' | b'%' | b'+' | b','
                )
        })
}

#[must_use]
pub fn quote(token: &str) -> String {
    if survives_a_shell_unquoted(token) {
        token.to_string()
    } else {
        format!("'{}'", token.replace('\'', r"'\''"))
    }
}

#[must_use]
pub fn default_shell() -> String {
    std::env::var("SHELL")
        .ok()
        .filter(|shell| !shell.is_empty())
        .unwrap_or_else(|| "/bin/sh".to_string())
}

#[cfg(test)]
mod tests {
    use super::quote;

    #[test]
    fn safe_tokens_pass_through_unquoted() {
        assert_eq!(quote("status"), "status");
        assert_eq!(quote("op://vault/db/password"), "op://vault/db/password");
        assert_eq!(quote("--json"), "--json");
    }

    #[test]
    fn anything_else_is_single_quoted_with_embedded_quotes_escaped() {
        assert_eq!(quote("my backup"), "'my backup'");
        assert_eq!(quote("x; touch /tmp/pwned"), "'x; touch /tmp/pwned'");
        assert_eq!(quote("o'brien"), "'o'\\''brien'");
        assert_eq!(quote(""), "''");
    }
}
