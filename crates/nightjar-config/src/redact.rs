use crate::secrets::SecretValue;
use zeroize::Zeroizing;

/// Free of newlines so a redaction never turns one line of output into two,
/// unlike `capture::TRUNCATION_MARKER`, which is meant to stand alone.
pub const MARKER: &[u8] = b"[nightjar:redacted]";

/// Streams bytes through a set of secret values, replacing every exact
/// occurrence with `MARKER`. A match can straddle any boundary the caller
/// splits its input on: `feed` holds back whatever suffix might still grow
/// into a needle.
pub struct Redactor {
    needles: Vec<Zeroizing<Vec<u8>>>,
    max_len: usize,
    pending: Zeroizing<Vec<u8>>,
}

impl Redactor {
    pub fn new(secrets: &[SecretValue]) -> Self {
        let mut needles: Vec<Zeroizing<Vec<u8>>> = secrets
            .iter()
            .map(|s| Zeroizing::new(s.as_str().as_bytes().to_vec()))
            // Empty or all-whitespace is a misconfiguration, not a real
            // secret — matching it would redact every byte of output.
            .filter(|b| !b.is_empty() && !b.iter().all(u8::is_ascii_whitespace))
            .collect();
        needles.sort_by(|a, b| a.as_slice().cmp(b.as_slice()));
        needles.dedup_by(|a, b| a.as_slice() == b.as_slice());
        let max_len = needles.iter().map(|n| n.len()).max().unwrap_or(0);
        Self {
            needles,
            max_len,
            pending: Zeroizing::new(Vec::new()),
        }
    }

    pub fn is_noop(&self) -> bool {
        self.needles.is_empty()
    }

    /// Returns the prefix confirmed safe to write; a remainder of at most
    /// `max_len - 1` bytes stays held until a later `feed` or `finish`.
    pub fn feed(&mut self, chunk: &[u8]) -> Vec<u8> {
        if self.is_noop() {
            return chunk.to_vec();
        }
        self.pending.extend_from_slice(chunk);
        self.drain(false)
    }

    /// Call once the source is exhausted; whatever is still held back never
    /// completed a match, so it is returned as ordinary bytes.
    pub fn finish(mut self) -> Vec<u8> {
        if self.is_noop() {
            return Vec::new();
        }
        self.drain(true)
    }

    fn drain(&mut self, at_eof: bool) -> Vec<u8> {
        let mut out = Vec::new();
        let mut cursor = 0;
        while let Some((start, len)) = self.find_earliest_match(cursor) {
            out.extend_from_slice(&self.pending[cursor..start]);
            out.extend_from_slice(MARKER);
            cursor = start + len;
        }

        let hold = if at_eof {
            0
        } else {
            self.longest_suffix_matching_a_needle_prefix(cursor)
        };
        let flush_to = self.pending.len() - hold;
        out.extend_from_slice(&self.pending[cursor..flush_to]);
        self.pending.drain(..flush_to);
        out
    }

    /// The earliest, and among ties the longest, full needle occurrence at
    /// or after `from` — longest-at-a-tie so a secret that is a byte-prefix
    /// of another still consumes the whole longer one.
    fn find_earliest_match(&self, from: usize) -> Option<(usize, usize)> {
        let hay = &self.pending[from..];
        let mut best: Option<(usize, usize)> = None;
        for needle in &self.needles {
            if needle.len() > hay.len() {
                continue;
            }
            let Some(pos) = hay
                .windows(needle.len())
                .position(|w| w == needle.as_slice())
            else {
                continue;
            };
            let start = from + pos;
            let better = match best {
                None => true,
                Some((bp, blen)) => start < bp || (start == bp && needle.len() > blen),
            };
            if better {
                best = Some((start, needle.len()));
            }
        }
        best
    }

    /// The longest suffix of `pending[from..]` that is also a prefix of some
    /// needle. Capped at `max_len - 1`: anything longer would already have
    /// been a complete match, caught above.
    fn longest_suffix_matching_a_needle_prefix(&self, from: usize) -> usize {
        let tail = &self.pending[from..];
        let upper = tail.len().min(self.max_len.saturating_sub(1));
        for len in (1..=upper).rev() {
            let suffix = &tail[tail.len() - len..];
            if self
                .needles
                .iter()
                .any(|n| n.len() >= len && &n[..len] == suffix)
            {
                return len;
            }
        }
        0
    }
}

/// One-shot redaction for text bound for a notification channel. A lossy
/// UTF-8 decode is an acceptable trade here, unlike for capture files, which
/// are replayed byte-for-byte.
pub fn redact_text(secrets: &[SecretValue], text: &str) -> String {
    let mut r = Redactor::new(secrets);
    let mut out = r.feed(text.as_bytes());
    out.extend(r.finish());
    String::from_utf8_lossy(&out).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn secret(s: &str) -> SecretValue {
        zeroize::Zeroizing::new(s.to_string())
    }

    fn redact_all(secrets: &[SecretValue], input: &[u8]) -> Vec<u8> {
        let mut r = Redactor::new(secrets);
        let mut out = r.feed(input);
        out.extend(r.finish());
        out
    }

    #[test]
    fn secret_is_replaced_with_the_marker_when_it_is_within_a_single_chunk() {
        let secrets = [secret("hunter2")];
        let out = redact_all(&secrets, b"password=hunter2 end");
        assert_eq!(out, [b"password=".as_slice(), MARKER, b" end"].concat());
    }

    #[test]
    fn secret_is_still_redacted_when_split_across_a_chunk_boundary() {
        let secrets = [secret("hunter2")];
        let mut r = Redactor::new(&secrets);
        let mut out = r.feed(b"password=hunt");
        out.extend(r.feed(b"er2 end"));
        out.extend(r.finish());
        assert_eq!(out, [b"password=".as_slice(), MARKER, b" end"].concat());
    }

    #[test]
    fn secret_is_still_redacted_when_split_across_many_chunks() {
        let long_secret = "x".repeat(20_000);
        let secrets = [secret(&long_secret)];
        let mut r = Redactor::new(&secrets);
        let mut out = Vec::new();
        out.extend(r.feed(b"before-"));
        for chunk in long_secret.as_bytes().chunks(4096) {
            out.extend(r.feed(chunk));
        }
        out.extend(r.feed(b"-after"));
        out.extend(r.finish());
        assert_eq!(out, [b"before-".as_slice(), MARKER, b"-after"].concat());
    }

    #[test]
    fn redaction_does_not_corrupt_surrounding_output() {
        let secrets = [secret("s3cr3t")];
        let out = redact_all(&secrets, b"line one\nkey=s3cr3t\nline three\n");
        let text = String::from_utf8(out).unwrap();
        assert!(text.starts_with("line one\nkey="), "got: {text:?}");
        assert!(text.ends_with("\nline three\n"), "got: {text:?}");
        assert!(!text.contains("s3cr3t"), "got: {text:?}");
    }

    #[test]
    fn secret_does_not_redact_the_whole_stream_when_it_is_empty_or_whitespace() {
        let secrets = [secret(""), secret("\n"), secret("   ")];
        let input = b"totally ordinary output\nwith newlines\n";
        assert_eq!(redact_all(&secrets, input), input);
    }

    #[test]
    fn redaction_is_byte_exact_and_not_utf8_dependent() {
        let secrets = [secret("hunter2")];
        let mut input = b"before-".to_vec();
        input.extend_from_slice(b"hunter2");
        input.push(0xFF);
        input.extend_from_slice(b"-after");

        let out = redact_all(&secrets, &input);

        let mut expected = b"before-".to_vec();
        expected.extend_from_slice(MARKER);
        expected.push(0xFF);
        expected.extend_from_slice(b"-after");
        assert_eq!(out, expected);
    }

    #[test]
    fn secret_is_redacted_every_time_when_it_appears_more_than_once() {
        let secrets = [secret("dup")];
        let out = redact_all(&secrets, b"dup and dup again");
        assert_eq!(out.windows(3).filter(|w| *w == b"dup").count(), 0);
        assert_eq!(
            out.windows(MARKER.len()).filter(|w| *w == MARKER).count(),
            2
        );
    }

    #[test]
    fn multiple_distinct_secrets_are_all_redacted() {
        let secrets = [secret("alpha"), secret("beta")];
        let out = redact_all(&secrets, b"alpha and beta together");
        let text = String::from_utf8(out).unwrap();
        assert!(
            !text.contains("alpha") && !text.contains("beta"),
            "got: {text:?}"
        );
    }

    #[test]
    fn both_secrets_are_still_cleared_when_one_is_a_byte_prefix_of_the_other() {
        let secrets = [secret("ab"), secret("abc")];
        let out = redact_all(&secrets, b"xabcx");
        let text = String::from_utf8(out).unwrap();
        assert!(
            !text.contains("abc") && !text.contains("ab"),
            "got: {text:?}"
        );
    }

    #[test]
    fn overlapping_but_not_nested_needles_leave_no_raw_secret_bytes_behind() {
        let secrets = [secret("ab"), secret("bcd")];
        let out = redact_all(&secrets, b"xx-abcd-yy");
        let text = String::from_utf8(out).unwrap();

        assert!(text.starts_with("xx-"), "got: {text:?}");
        assert!(text.ends_with("-yy"), "got: {text:?}");
        assert!(!text.contains("ab"), "got: {text:?}");
        assert!(!text.contains("bcd"), "got: {text:?}");
    }

    #[test]
    fn stream_is_left_untouched_when_there_are_no_secrets() {
        let out = redact_all(&[], b"nothing to hide here");
        assert_eq!(out, b"nothing to hide here");
    }

    #[test]
    fn redact_text_scrubs_a_secret_from_a_string() {
        let secrets = [secret("tok3n")];
        let s = redact_text(&secrets, "Authorization: tok3n");
        assert!(!s.contains("tok3n"), "got: {s:?}");
        assert!(s.contains("Authorization:"), "got: {s:?}");
    }
}
