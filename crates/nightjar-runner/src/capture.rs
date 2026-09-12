use std::fs::File;
use std::io::{self, Read, Write};

use nightjar_config::redact::Redactor;
use nightjar_config::secrets::SecretValue;

pub const TRUNCATION_MARKER: &str = "\n[nightjar: output truncated at cap]\n";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Capping {
    EverythingWritten,
    SomethingDropped,
}

fn write_capped(dst: &mut File, data: &[u8], cap: u64, written: &mut u64) -> io::Result<Capping> {
    if data.is_empty() {
        return Ok(Capping::EverythingWritten);
    }
    let room = cap.saturating_sub(*written);
    let take = usize::try_from(room).unwrap_or(usize::MAX).min(data.len());
    if take > 0 {
        dst.write_all(&data[..take])?;
        *written += take as u64;
    }
    Ok(if take == data.len() {
        Capping::EverythingWritten
    } else {
        Capping::SomethingDropped
    })
}

/// # Errors
/// fails on a read error other than `Interrupted`, or on any write to the capture file
pub fn pump(
    mut src: impl Read,
    mut dst: File,
    cap: u64,
    secrets: &[SecretValue],
) -> io::Result<u64> {
    let mut buf = [0u8; 8192];
    let mut seen: u64 = 0;
    let mut written: u64 = 0;
    let mut dropped_anything = false;
    let mut marker_on_disk = false;
    let mut redactor = Redactor::new(secrets);
    let redacting = !redactor.is_noop();

    loop {
        let bytes_read = match src.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        };
        seen = seen.saturating_add(bytes_read as u64);

        let capping = if written >= cap {
            Capping::SomethingDropped
        } else if redacting {
            let ready = redactor.feed(&buf[..bytes_read]);
            write_capped(&mut dst, &ready, cap, &mut written)?
        } else {
            write_capped(&mut dst, &buf[..bytes_read], cap, &mut written)?
        };
        dropped_anything |= capping == Capping::SomethingDropped;

        if dropped_anything && !marker_on_disk {
            dst.write_all(TRUNCATION_MARKER.as_bytes())?;
            marker_on_disk = true;
        }
    }

    if redacting {
        let tail = redactor.finish();
        dropped_anything |=
            write_capped(&mut dst, &tail, cap, &mut written)? == Capping::SomethingDropped;
    }
    if dropped_anything && !marker_on_disk {
        dst.write_all(TRUNCATION_MARKER.as_bytes())?;
    }

    dst.flush()?;
    Ok(seen)
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;

    use super::*;

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
    fn output_is_not_marked_truncated_when_it_is_exactly_the_cap() {
        let exactly_the_cap = "z".repeat(64);
        let (seen, written) = pump_to_string(&exactly_the_cap, 64);

        assert_eq!(seen, 64);
        assert_eq!(
            written, exactly_the_cap,
            "every byte reached disk, so nothing was truncated"
        );
    }

    #[test]
    fn output_is_not_marked_truncated_when_there_was_none_and_the_cap_is_zero() {
        let (seen, written) = pump_to_string("", 0);

        assert_eq!(seen, 0);
        assert_eq!(written, "", "a job that printed nothing lost nothing");
    }

    #[test]
    fn output_is_marked_truncated_when_the_cap_is_zero_and_the_job_printed_something() {
        let (seen, written) = pump_to_string("dropped entirely\n", 0);

        assert_eq!(seen, 17);
        assert_eq!(written, TRUNCATION_MARKER);
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
    fn output_is_marked_truncated_when_the_cap_lands_on_a_tail_the_redactor_was_still_holding() {
        let cap = 60;
        let mut input = "z".repeat(usize::try_from(cap).unwrap()).into_bytes();
        input.extend_from_slice(b"hunte");

        let (seen, written) = pump_to_bytes(&input, cap, &[secret("hunter2")]);
        let text = String::from_utf8(written).unwrap();

        assert_eq!(seen, 65);
        assert!(
            text.ends_with(TRUNCATION_MARKER),
            "the held-back tail never reached disk: {text:?}"
        );
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
