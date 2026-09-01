/// True for a token that round-trips through a POSIX shell unquoted.
/// This keeps the common case readable and copy-pasteable.
fn is_shell_safe(s: &str) -> bool {
    !s.is_empty()
        && s.bytes().all(|b| {
            b.is_ascii_alphanumeric()
                || matches!(
                    b,
                    b'-' | b'_' | b'.' | b'/' | b':' | b'@' | b'%' | b'+' | b','
                )
        })
}

/// Quotes `s` as one word for a POSIX shell. An embedded `'` is escaped
/// as `'\''`, since nothing can be escaped while single-quoted.
pub fn quote(s: &str) -> String {
    if is_shell_safe(s) {
        s.to_string()
    } else {
        format!("'{}'", s.replace('\'', r"'\''"))
    }
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
