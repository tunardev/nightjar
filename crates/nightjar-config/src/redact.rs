use zeroize::Zeroizing;

use crate::secrets::SecretValue;

pub const MARKER: &[u8] = b"[nightjar:redacted]";

#[derive(Clone, Copy)]
enum Feed {
    MoreMayFollow,
    EndOfStream,
}

#[derive(Clone, Copy)]
struct Occurrence {
    start: usize,
    len: usize,
}

impl Occurrence {
    const fn end(self) -> usize {
        self.start + self.len
    }
}

pub struct Redactor {
    needles: Vec<Zeroizing<Vec<u8>>>,
    max_len: usize,
    pending: Zeroizing<Vec<u8>>,
}

impl Redactor {
    #[must_use]
    pub fn new(secrets: &[SecretValue]) -> Self {
        let mut needles: Vec<Zeroizing<Vec<u8>>> = secrets
            .iter()
            .map(|s| Zeroizing::new(s.as_str().as_bytes().to_vec()))
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

    #[must_use]
    pub const fn is_noop(&self) -> bool {
        self.needles.is_empty()
    }

    pub fn feed(&mut self, chunk: &[u8]) -> Vec<u8> {
        if self.is_noop() {
            return chunk.to_vec();
        }
        self.pending.extend_from_slice(chunk);
        self.drain(Feed::MoreMayFollow)
    }

    #[must_use]
    pub fn finish(mut self) -> Vec<u8> {
        if self.is_noop() {
            return Vec::new();
        }
        self.drain(Feed::EndOfStream)
    }

    fn drain(&mut self, feed: Feed) -> Vec<u8> {
        let mut out = Vec::new();
        let mut cursor = 0;
        while let Some(found) = self.find_earliest_match(cursor) {
            if matches!(feed, Feed::MoreMayFollow) && self.may_still_grow(found) {
                break;
            }
            out.extend_from_slice(&self.pending[cursor..found.start]);
            out.extend_from_slice(MARKER);
            cursor = found.end();
        }

        let hold = match feed {
            Feed::EndOfStream => 0,
            Feed::MoreMayFollow => self.longest_suffix_matching_a_needle_prefix(cursor),
        };
        let flush_to = self.pending.len() - hold;
        out.extend_from_slice(&self.pending[cursor..flush_to]);
        self.pending.drain(..flush_to);
        out
    }

    fn may_still_grow(&self, found: Occurrence) -> bool {
        if found.end() != self.pending.len() {
            return false;
        }
        let so_far = &self.pending[found.start..];
        self.needles
            .iter()
            .any(|needle| needle.len() > so_far.len() && needle.starts_with(so_far))
    }

    fn find_earliest_match(&self, from: usize) -> Option<Occurrence> {
        let haystack = &self.pending[from..];
        let mut earliest: Option<Occurrence> = None;
        for needle in &self.needles {
            if needle.len() > haystack.len() {
                continue;
            }
            let Some(offset) = haystack
                .windows(needle.len())
                .position(|window| window == needle.as_slice())
            else {
                continue;
            };
            let candidate = Occurrence {
                start: from + offset,
                len: needle.len(),
            };
            let better = earliest.is_none_or(|best| {
                candidate.start < best.start
                    || (candidate.start == best.start && candidate.len > best.len)
            });
            if better {
                earliest = Some(candidate);
            }
        }
        earliest
    }

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

#[must_use]
pub fn redact_text(secrets: &[SecretValue], text: &str) -> String {
    let mut redactor = Redactor::new(secrets);
    let mut out = redactor.feed(text.as_bytes());
    out.extend(redactor.finish());
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
    fn longer_secret_still_wins_when_the_shorter_one_completes_at_a_chunk_boundary() {
        let secrets = [secret("hunter"), secret("hunter2")];
        let mut r = Redactor::new(&secrets);
        let mut out = r.feed(b"pw=hunter");
        out.extend(r.feed(b"2 ok"));
        out.extend(r.finish());

        assert_eq!(
            out,
            [b"pw=".as_slice(), MARKER, b" ok"].concat(),
            "chunking must not leak the tail of the longer secret"
        );
    }

    #[test]
    fn every_chunking_of_the_same_input_redacts_identically() {
        let secrets = [
            secret("ab"),
            secret("abc"),
            secret("bcd"),
            secret("hunter2"),
        ];
        let input = b"x-ab-abc-abcd-hunter2-bcd-a";
        let whole = redact_all(&secrets, input);

        for split in 0..=input.len() {
            let mut r = Redactor::new(&secrets);
            let mut out = r.feed(&input[..split]);
            out.extend(r.feed(&input[split..]));
            out.extend(r.finish());
            assert_eq!(
                out, whole,
                "splitting after {split} bytes changed the result"
            );
        }
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
