use jiff::Timestamp;

/// A `toml` error spans multiple lines with a caret. Keep just the
/// first line — a table row holds only one line.
pub fn error_summary(e: &anyhow::Error) -> String {
    format!("{e:#}")
        .lines()
        .next()
        .unwrap_or_default()
        .trim_end()
        .to_string()
}

pub fn json_string(s: &str) -> String {
    use std::fmt::Write as _;
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => {
                let _ = write!(out, "\\u{:04x}", c as u32);
            }
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

pub fn relative_time(then: Timestamp, now: Timestamp) -> String {
    let secs = (now.as_second() - then.as_second()).max(0);
    match secs {
        0 => "just now".to_string(),
        s if s < 60 => format!("{s}s ago"),
        s if s < 3600 => format!("{}m ago", s / 60),
        s if s < 86_400 => format!("{}h ago", s / 3600),
        s => format!("{}d ago", s / 86_400),
    }
}

pub fn relative_future(then: Timestamp, now: Timestamp) -> String {
    let secs = (then.as_second() - now.as_second()).max(0);
    match secs {
        s if s < 60 => format!("in {s}s"),
        s if s < 3600 => format!("in {}m", s / 60),
        s if s < 86_400 => format!("in {}h", s / 3600),
        s => format!("in {}d", s / 86_400),
    }
}

/// Only abbreviates "every N minutes/hours". Everything else passes
/// through unchanged, so `list` stays grep-able against the job file.
pub fn abbreviate_schedule(source: &str) -> String {
    let trimmed = source.trim();
    let lower = trimmed.to_lowercase();
    let words: Vec<&str> = lower.split_whitespace().collect();
    match words.as_slice() {
        ["every", n, unit] if n.chars().all(|c| c.is_ascii_digit()) => match *unit {
            "minute" | "minutes" => format!("every {n}m"),
            "hour" | "hours" => format!("every {n}h"),
            _ => trimmed.to_string(),
        },
        _ => trimmed.to_string(),
    }
}

// Bounded by a job's own lifetime, far below f64's 2^53 limit. The
// precision loss here is unreachable in practice.
#[allow(clippy::cast_precision_loss, clippy::cast_possible_truncation)]
pub fn duration_human(ms: i64) -> String {
    // A backwards clock step can make this negative. "-0.5s" is not a duration.
    let secs = ms.max(0) as f64 / 1000.0;
    if secs < 60.0 {
        format!("{secs:.1}s")
    } else if secs < 3600.0 {
        format!("{}m{}s", (secs / 60.0) as i64, (secs % 60.0) as i64)
    } else {
        format!(
            "{}h{}m",
            (secs / 3600.0) as i64,
            ((secs % 3600.0) / 60.0) as i64
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use jiff::Span;

    fn ts(s: &str) -> Timestamp {
        s.parse().unwrap()
    }

    #[test]
    fn abbreviate_schedule_shortens_every_n_minutes_and_hours() {
        assert_eq!(abbreviate_schedule("every 15 minutes"), "every 15m");
        assert_eq!(abbreviate_schedule("every 1 minute"), "every 1m");
        assert_eq!(abbreviate_schedule("every 6 hours"), "every 6h");
        assert_eq!(abbreviate_schedule("Every 6 Hours"), "every 6h");
    }

    #[test]
    fn abbreviate_schedule_leaves_every_other_form_verbatim() {
        assert_eq!(abbreviate_schedule("hourly"), "hourly");
        assert_eq!(abbreviate_schedule("daily at 2am"), "daily at 2am");
        assert_eq!(
            abbreviate_schedule("weekly sun at 3am"),
            "weekly sun at 3am"
        );
        assert_eq!(abbreviate_schedule("0 2 * * *"), "0 2 * * *");
    }

    #[test]
    fn relative_time_renders_past_intervals() {
        let now = ts("2026-08-23T12:00:00Z");
        assert_eq!(relative_time(now - Span::new().seconds(5), now), "5s ago");
        assert_eq!(relative_time(now - Span::new().minutes(8), now), "8m ago");
        assert_eq!(relative_time(now - Span::new().hours(2), now), "2h ago");
        assert_eq!(relative_time(now - Span::new().hours(72), now), "3d ago");
    }

    #[test]
    fn relative_time_returns_just_now_when_then_equals_now() {
        let now = ts("2026-08-23T12:00:00Z");
        assert_eq!(relative_time(now, now), "just now");
    }

    #[test]
    fn relative_future_renders_upcoming_intervals() {
        let now = ts("2026-08-23T12:00:00Z");
        assert_eq!(
            relative_future(now + Span::new().seconds(30), now),
            "in 30s"
        );
        assert_eq!(relative_future(now + Span::new().minutes(7), now), "in 7m");
        assert_eq!(relative_future(now + Span::new().hours(22), now), "in 22h");
        assert_eq!(relative_future(now + Span::new().hours(96), now), "in 4d");
    }

    #[test]
    fn relative_future_clamps_to_zero_when_time_is_in_the_past() {
        let now = ts("2026-08-23T12:00:00Z");
        assert_eq!(relative_future(now - Span::new().hours(1), now), "in 0s");
    }

    #[test]
    fn duration_human_scales_units() {
        assert_eq!(duration_human(340), "0.3s");
        assert_eq!(duration_human(12_400), "12.4s");
        assert_eq!(duration_human(90_000), "1m30s");
        assert_eq!(duration_human(3_600_000), "1h0m");
        assert_eq!(
            duration_human(-500),
            "0.0s",
            "a clock step backwards is not a negative duration"
        );
    }
}
