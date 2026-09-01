use nightjar_config::redact::Redactor;
use nightjar_config::secrets::SecretValue;
use std::fs::File;
use std::io::{self, Read, Write};

pub const TRUNCATION_MARKER: &str = "\n[nightjar: output truncated at cap]\n";

fn write_capped(dst: &mut File, data: &[u8], cap: u64, written: &mut u64) -> io::Result<()> {
    if data.is_empty() || *written >= cap {
        return Ok(());
    }
    let room = cap - *written;
    let take = usize::try_from(room).unwrap_or(usize::MAX).min(data.len());
    dst.write_all(&data[..take])?;
    *written += take as u64;
    Ok(())
}

/// Copy `src` to `dst`. Redact every occurrence of `secrets` while copying.
/// Write at most `cap` bytes, then one truncation marker if the input was
/// longer. Return the total number of bytes read, not written, so callers
/// can report the true output size even when the file on disk is shorter.
///
/// Redaction happens here, before any byte reaches `dst`. A secret never
/// touches disk unredacted, even for a moment. `cap` bounds bytes written
/// after redaction. It does not bound bytes read.
pub fn pump(
    mut src: impl Read,
    mut dst: File,
    cap: u64,
    secrets: &[SecretValue],
) -> io::Result<u64> {
    let mut buf = [0u8; 8192];
    let mut seen: u64 = 0;
    let mut written: u64 = 0;
    let mut marked = false;
    let mut redactor = Redactor::new(secrets);
    // `Redactor::feed` still allocates a `Vec<u8>` copy per chunk even with
    // no secrets. Checking once here skips that when nothing will match.
    let redacting = !redactor.is_noop();

    loop {
        let n = match src.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        };
        seen = seen.saturating_add(n as u64);

        if written < cap {
            if redacting {
                let ready = redactor.feed(&buf[..n]);
                write_capped(&mut dst, &ready, cap, &mut written)?;
            } else {
                write_capped(&mut dst, &buf[..n], cap, &mut written)?;
            }
        }

        if written >= cap && !marked {
            dst.write_all(TRUNCATION_MARKER.as_bytes())?;
            marked = true;
        }
    }

    // `src` is exhausted now, so no match will ever complete. Flush whatever
    // the redactor held back.
    if redacting && written < cap {
        let tail = redactor.finish();
        write_capped(&mut dst, &tail, cap, &mut written)?;
    }
    if written >= cap && !marked {
        dst.write_all(TRUNCATION_MARKER.as_bytes())?;
    }

    dst.flush()?;
    Ok(seen)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    fn pump_to_bytes(input: &[u8], cap: u64, secrets: &[SecretValue]) -> (u64, Vec<u8>) {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        let path = tmp.path().to_path_buf();
        let seen = pump(
            Cursor::new(input.to_vec()),
            tmp.reopen().unwrap(),
            cap,
            secrets,
        )
        .unwrap();
        (seen, std::fs::read(&path).unwrap())
    }

    fn pump_to_string(input: &str, cap: u64) -> (u64, String) {
        let (seen, bytes) = pump_to_bytes(input.as_bytes(), cap, &[]);
        (seen, String::from_utf8(bytes).unwrap())
    }

    fn secret(s: &str) -> SecretValue {
        zeroize::Zeroizing::new(s.to_string())
    }

    #[test]
    fn output_is_written_verbatim_when_short() {
        let (seen, written) = pump_to_string("hello world\n", 1024);
        assert_eq!(seen, 12);
        assert_eq!(written, "hello world\n");
    }

    #[test]
    fn output_is_truncated_and_marked_when_over_cap() {
        let big = "x".repeat(500);
        let (seen, written) = pump_to_string(&big, 100);

        assert_eq!(seen, 500, "must report true size, not written size");
        assert!(written.starts_with(&"x".repeat(100)));
        assert!(written.ends_with(TRUNCATION_MARKER));
        assert!(written.len() < 500);
    }

    #[test]
    fn output_writes_nothing_and_reports_zero_when_empty() {
        let (seen, written) = pump_to_string("", 1024);
        assert_eq!(seen, 0);
        assert_eq!(written, "");
    }

    #[test]
    fn marker_is_written_exactly_once_when_input_is_huge() {
        let big = "y".repeat(100_000);
        let (_, written) = pump_to_string(&big, 50);
        assert_eq!(written.matches(TRUNCATION_MARKER).count(), 1);
    }

    #[test]
    fn secret_is_still_redacted_when_split_across_a_chunk_boundary() {
        let secret_text = "hunter2-canary";
        let prefix = "a".repeat(8192 - 4);
        let mut input = prefix.clone().into_bytes();
        input.extend_from_slice(secret_text.as_bytes());
        input.extend_from_slice(b"-tail");

        let (_, written) = pump_to_bytes(&input, u64::from(u32::MAX), &[secret(secret_text)]);
        let text = String::from_utf8(written).unwrap();

        assert!(
            !text.contains(secret_text),
            "secret leaked across the chunk boundary"
        );
        assert!(text.starts_with(&prefix));
        assert!(text.ends_with("-tail"));
        assert_eq!(text.matches("[nightjar:redacted]").count(), 1);
    }

    #[test]
    fn secret_is_still_gone_from_the_truncated_output_when_it_ends_before_the_cap() {
        let mut input = "x".repeat(50).into_bytes();
        input.extend_from_slice(b"hunter2");
        input.extend_from_slice(&"y".repeat(50).into_bytes());

        let (_, written) = pump_to_bytes(&input, 60, &[secret("hunter2")]);
        let text = String::from_utf8(written).unwrap();
        assert!(!text.contains("hunter2"), "got: {text:?}");
        assert!(text.ends_with(TRUNCATION_MARKER));
    }
}
