use jiff::Timestamp;

#[must_use]
pub fn error_summary(error: &anyhow::Error) -> String {
    format!("{error:#}")
        .lines()
        .next()
        .unwrap_or_default()
        .trim_end()
        .to_string()
}

pub fn log_line(args: std::fmt::Arguments<'_>) {
    eprintln!("{} nightjar: {args}", log_timestamp(&jiff::Zoned::now()));
}

fn log_timestamp(at: &jiff::Zoned) -> String {
    at.strftime("%Y-%m-%dT%H:%M:%S%:z").to_string()
}

#[macro_export]
macro_rules! log {
    ($($arg:tt)*) => {
        $crate::format::log_line(format_args!($($arg)*))
    };
}

#[must_use]
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

#[must_use]
pub fn relative_future(then: Timestamp, now: Timestamp) -> String {
    let secs = (then.as_second() - now.as_second()).max(0);
    match secs {
        s if s < 60 => format!("in {s}s"),
        s if s < 3600 => format!("in {}m", s / 60),
        s if s < 86_400 => format!("in {}h", s / 3600),
        s => format!("in {}d", s / 86_400),
    }
}

#[must_use]
pub fn exit_reason(status: &std::process::ExitStatus) -> String {
    status
        .code()
        .map_or_else(|| "a signal".to_string(), |code| format!("exit {code}"))
}

#[must_use]
pub fn quantity(count: usize, singular: &str, plural: &str) -> String {
    if count == 1 {
        format!("{count} {singular}")
    } else {
        format!("{count} {plural}")
    }
}

#[must_use]
pub fn abbreviate_schedule(source: &str) -> String {
    let trimmed = source.trim();
    let lower = trimmed.to_lowercase();
    let words: Vec<&str> = lower.split_whitespace().collect();
    match words.as_slice() {
        ["every", count, unit] if count.chars().all(|c| c.is_ascii_digit()) => match *unit {
            "minute" | "minutes" => format!("every {count}m"),
            "hour" | "hours" => format!("every {count}h"),
            _ => trimmed.to_string(),
        },
        _ => trimmed.to_string(),
    }
}

#[allow(clippy::cast_precision_loss, clippy::cast_possible_truncation)]
#[must_use]
pub fn duration_human(duration_ms: i64) -> String {
    let secs = duration_ms.max(0) as f64 / 1000.0;
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
    use jiff::Span;

    use super::*;

    fn ts(s: &str) -> Timestamp {
        s.parse().unwrap()
    }

    #[test]
    fn log_timestamp_is_rfc3339_with_a_local_offset() {
        let at = ts("2026-08-23T12:34:56Z").to_zoned(jiff::tz::TimeZone::get("Asia/Baku").unwrap());
        assert_eq!(log_timestamp(&at), "2026-08-23T16:34:56+04:00");
    }

    #[test]
    fn quantity_picks_the_form_that_matches_the_count() {
        assert_eq!(quantity(0, "job", "jobs"), "0 jobs");
        assert_eq!(quantity(1, "job", "jobs"), "1 job");
        assert_eq!(quantity(2, "job", "jobs"), "2 jobs");
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
