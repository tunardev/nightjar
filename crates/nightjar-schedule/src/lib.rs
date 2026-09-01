pub mod cron;
pub mod human;

use anyhow::{Context, Result, bail};
use jiff::{Timestamp, Zoned, tz::TimeZone};
use std::str::FromStr;

/// No valid cron field ever matches one of these words. Real cron
/// input can't be mistaken for human syntax.
const HUMAN_SCHEDULE_KEYWORDS: [&str; 5] = ["hourly", "daily", "weekly", "weekdays", "every"];

fn leading_human_keyword(input: &str) -> Option<String> {
    let first = input
        .trim()
        .to_lowercase()
        .split_whitespace()
        .next()?
        .to_string();
    HUMAN_SCHEDULE_KEYWORDS
        .contains(&first.as_str())
        .then_some(first)
}

#[derive(Debug, Clone)]
pub struct Schedule {
    inner: jiff_cron::Schedule,
    source: String,
}

impl Schedule {
    pub fn parse(input: &str) -> Result<Schedule> {
        let as_cron = match human::to_cron(input) {
            Some(c) => c,
            None if input.trim().eq_ignore_ascii_case("@reboot") => bail!(
                "invalid schedule {input:?}: @reboot runs at boot, not on a schedule, and has \
                 no equivalent here — give the job a schedule, or leave it to cron"
            ),
            None if input.trim().starts_with('@') => bail!(
                "invalid schedule {input:?}: unknown crontab shortcut; the supported ones are \
                 @hourly, @daily, @midnight, @weekly, @monthly, @yearly, and @annually"
            ),
            None => match leading_human_keyword(input) {
                Some(keyword) => bail!(
                    "invalid schedule {input:?}: looks like human schedule syntax \
                     (starts with {keyword:?}) but does not match the supported grammar — \
                     this is not a cron expression"
                ),
                None => input.to_string(),
            },
        };
        let normalized = cron::normalize_cron(&as_cron)
            .with_context(|| format!("invalid schedule {input:?}"))?;
        let inner = jiff_cron::Schedule::from_str(&normalized)
            .with_context(|| format!("invalid schedule {input:?}"))?;
        Ok(Schedule {
            inner,
            source: input.trim().to_string(),
        })
    }

    pub fn source(&self) -> &str {
        &self.source
    }

    /// Next firing strictly after `after`, or `None` if none remain.
    pub fn next_after(&self, after: Timestamp, tz: &TimeZone) -> Result<Option<Timestamp>> {
        let zoned: Zoned = after.to_zoned(tz.clone());
        Ok(self.inner.after(zoned).next().map(|z| z.timestamp()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn utc() -> TimeZone {
        TimeZone::get("UTC").unwrap()
    }

    const SUNDAY_NOON: &str = "2026-08-23T12:00:00Z";

    fn firing_after(expr: &str, after: &str) -> String {
        let s = Schedule::parse(expr).unwrap_or_else(|e| panic!("{expr:?} did not parse: {e:#}"));
        let after: Timestamp = after.parse().unwrap();
        s.next_after(after, &utc())
            .unwrap()
            .unwrap_or_else(|| panic!("{expr:?} has no next firing"))
            .to_zoned(utc())
            .strftime("%Y-%m-%d %H:%M %A")
            .to_string()
    }

    fn assert_fires(expr: &str, expected: &str) {
        assert_eq!(firing_after(expr, SUNDAY_NOON), expected, "for {expr:?}");
    }

    #[test]
    fn numeric_day_of_week_uses_posix_crontab_numbering() {
        assert_fires("0 3 * * 1", "2026-08-24 03:00 Monday");
        assert_fires("0 3 * * 6", "2026-08-29 03:00 Saturday");
        assert_fires("0 3 * * 0", "2026-08-30 03:00 Sunday");
        assert_fires("0 3 * * 7", "2026-08-30 03:00 Sunday");
    }

    #[test]
    fn numeric_and_named_days_of_week_agree() {
        for (numeric, named) in [
            ("0 3 * * 0", "0 3 * * SUN"),
            ("0 3 * * 1", "0 3 * * MON"),
            ("0 3 * * 5", "0 3 * * fri"),
            ("0 3 * * 6", "0 3 * * SAT"),
            ("0 3 * * 1-5", "0 3 * * MON-FRI"),
        ] {
            let after: Timestamp = SUNDAY_NOON.parse().unwrap();
            let a = Schedule::parse(numeric).unwrap().next_after(after, &utc());
            let b = Schedule::parse(named).unwrap().next_after(after, &utc());
            assert_eq!(
                a.unwrap(),
                b.unwrap(),
                "{numeric:?} and {named:?} must be the same instant"
            );
        }
    }

    #[test]
    fn day_of_week_ranges_lists_and_steps_are_translated() {
        assert_fires("0 3 * * 1-5", "2026-08-24 03:00 Monday");
        assert_fires("0 3 * * 0-6", "2026-08-24 03:00 Monday");
        assert_fires("0 3 * * 0,6", "2026-08-29 03:00 Saturday");
        assert_fires("0 3 * * 2,4", "2026-08-25 03:00 Tuesday");
        assert_fires("0 3 * * */3", "2026-08-26 03:00 Wednesday");
        assert_fires("0 3 * * 1-5/2", "2026-08-24 03:00 Monday");
        assert_fires("0 0 3 * * 6", "2026-08-29 03:00 Saturday");
    }

    fn firings(expr: &str, count: usize) -> Vec<String> {
        let s = Schedule::parse(expr).unwrap_or_else(|e| panic!("{expr:?} did not parse: {e:#}"));
        let mut at: Timestamp = SUNDAY_NOON.parse().unwrap();
        (0..count)
            .map(|_| {
                at = s.next_after(at, &utc()).unwrap().unwrap();
                at.to_zoned(utc()).strftime("%Y-%m-%d %H:%M %A").to_string()
            })
            .collect()
    }

    fn weekdays_of(expr: &str, count: usize) -> Vec<String> {
        firings(expr, count)
            .iter()
            .map(|f| f.rsplit(' ').next().unwrap().to_string())
            .collect()
    }

    #[test]
    fn range_spanning_the_whole_week_fires_daily_not_weekly() {
        assert_eq!(
            firings("0 3 * * 0-7", 3),
            [
                "2026-08-24 03:00 Monday",
                "2026-08-25 03:00 Tuesday",
                "2026-08-26 03:00 Wednesday",
            ]
        );
    }

    #[test]
    fn every_spelling_of_the_whole_week_reaches_the_same_schedule() {
        let daily = firings("0 3 * * *", 8);
        for spelling in ["0 3 * * 0-7", "0 3 * * 1-7", "0 3 * * 0-6"] {
            assert_eq!(firings(spelling, 8), daily, "for {spelling:?}");
        }
    }

    #[test]
    fn range_ending_on_sunday_as_seven_keeps_every_day_in_it() {
        assert_eq!(
            weekdays_of("0 3 * * 5-7", 6),
            [
                "Friday", "Saturday", "Sunday", "Friday", "Saturday", "Sunday"
            ]
        );
    }

    #[test]
    fn range_that_wraps_past_saturday_lands_on_both_of_its_days() {
        assert_eq!(
            weekdays_of("0 3 * * 6-0", 4),
            ["Saturday", "Sunday", "Saturday", "Sunday"]
        );
    }

    #[test]
    fn step_over_a_whole_week_range_and_a_list_containing_one_both_work() {
        assert_eq!(
            weekdays_of("0 3 * * 0-7/2", 8),
            [
                "Tuesday", "Thursday", "Saturday", "Sunday", "Tuesday", "Thursday", "Saturday",
                "Sunday"
            ]
        );
        assert_eq!(firings("0 3 * * MON,0-7", 8), firings("0 3 * * *", 8));
    }

    #[test]
    fn human_weekly_lands_on_the_named_day() {
        assert_fires("weekly sun at 3am", "2026-08-30 03:00 Sunday");
        assert_fires("weekly mon at 3am", "2026-08-24 03:00 Monday");
        assert_fires("weekly sat at 3am", "2026-08-29 03:00 Saturday");
    }

    #[test]
    fn weekdays_never_lands_on_a_weekend_and_does_land_on_friday() {
        let s = Schedule::parse("weekdays at 9am").unwrap();
        let mut at: Timestamp = SUNDAY_NOON.parse().unwrap();
        let mut weekdays = Vec::new();
        for _ in 0..10 {
            at = s.next_after(at, &utc()).unwrap().unwrap();
            weekdays.push(at.to_zoned(utc()).strftime("%A").to_string());
        }

        assert!(
            !weekdays.iter().any(|d| d == "Saturday" || d == "Sunday"),
            "weekdays fired on a weekend: {weekdays:?}"
        );
        assert!(
            weekdays.iter().any(|d| d == "Friday"),
            "weekdays never reached a Friday: {weekdays:?}"
        );
    }

    #[test]
    fn both_syntaxes_reach_the_same_schedule() {
        assert_fires("daily at 2am", "2026-08-24 02:00 Monday");
        assert_fires("0 2 * * *", "2026-08-24 02:00 Monday");
        assert_fires("weekly mon at 3am", "2026-08-24 03:00 Monday");
        assert_fires("0 3 * * 1", "2026-08-24 03:00 Monday");
        assert_fires("0 3 * * MON", "2026-08-24 03:00 Monday");
    }

    #[test]
    fn every_60_minutes_and_every_24_hours_are_accepted_and_fire_like_hourly_and_daily() {
        assert_fires("every 60 minutes", "2026-08-23 13:00 Sunday");
        assert_fires("hourly", "2026-08-23 13:00 Sunday");
        assert_fires("every 24 hours", "2026-08-24 00:00 Monday");
        assert_fires("daily", "2026-08-24 00:00 Monday");
    }

    #[test]
    fn source_is_preserved_for_display() {
        assert_eq!(
            Schedule::parse("daily at 2am").unwrap().source(),
            "daily at 2am"
        );
    }

    #[test]
    fn bad_schedule_names_the_input_in_its_error() {
        let err = Schedule::parse("every other tuesday")
            .unwrap_err()
            .to_string();
        assert!(err.contains("every other tuesday"), "message was: {err}");
    }

    #[test]
    fn near_miss_human_syntax_fails_with_a_clear_message_not_a_cron_error() {
        for bad in [
            "weekly mon at 3am extra",
            "daily at 25am",
            "every 0 minutes",
        ] {
            let err = Schedule::parse(bad).unwrap_err().to_string();
            assert!(err.contains(bad), "message did not name the input: {err}");
            assert!(
                !err.contains("Minutes") && !err.contains("does not support using names"),
                "leaked a jiff-cron-flavoured message for {bad:?}: {err}"
            );
        }
    }

    #[test]
    fn crontab_shortcut_fires_when_cron_would() {
        assert_fires("@daily", "2026-08-24 00:00 Monday");
        assert_fires("@hourly", "2026-08-23 13:00 Sunday");
        assert_fires("@weekly", "2026-08-30 00:00 Sunday");
        assert_fires("@monthly", "2026-09-01 00:00 Tuesday");
        assert_fires("@yearly", "2027-01-01 00:00 Friday");
    }

    #[test]
    fn reboot_and_unknown_shortcuts_are_refused_with_a_message_that_names_them() {
        let err = Schedule::parse("@reboot").unwrap_err().to_string();
        assert!(
            err.contains("@reboot") && err.contains("boot"),
            "got: {err}"
        );

        let err = Schedule::parse("@fortnightly").unwrap_err().to_string();
        assert!(
            err.contains("@fortnightly") && err.contains("@daily"),
            "got: {err}"
        );
    }

    #[test]
    fn real_cron_expression_still_parses_despite_the_keyword_check() {
        assert!(Schedule::parse("0 2 * * *").is_ok());
    }

    #[test]
    fn six_field_schedule_is_interpreted_seconds_first() {
        let s = Schedule::parse("30 0 2 * * *").unwrap();
        let tz = TimeZone::get("UTC").unwrap();
        let before: Timestamp = "2026-06-01T00:00:00Z".parse().unwrap();

        let next = s.next_after(before, &tz).unwrap().unwrap();

        let expected: Timestamp = "2026-06-01T02:00:30Z".parse().unwrap();
        let minutes_first_interpretation: Timestamp = "2026-06-01T00:30:00Z".parse().unwrap();
        assert_eq!(next, expected);
        assert_ne!(next, minutes_first_interpretation);
    }

    fn ny() -> TimeZone {
        TimeZone::get("America/New_York").unwrap()
    }

    fn local(ts: Timestamp) -> String {
        ts.to_zoned(ny())
            .strftime("%Y-%m-%d %H:%M:%S %z")
            .to_string()
    }

    #[test]
    fn spring_forward_runs_once_at_the_next_valid_instant() {
        let s = Schedule::parse("0 2 * * *").unwrap();
        let before: Timestamp = "2026-03-07T12:00:00Z".parse().unwrap();

        let first = s.next_after(before, &ny()).unwrap().unwrap();
        assert_eq!(
            local(first),
            "2026-03-08 03:00:00 -0400",
            "the 8th must not be skipped"
        );

        let second = s.next_after(first, &ny()).unwrap().unwrap();
        assert_eq!(local(second), "2026-03-09 02:00:00 -0400");
    }

    #[test]
    fn fall_back_runs_once_not_twice() {
        let s = Schedule::parse("0 1 * * *").unwrap();
        let before: Timestamp = "2026-10-31T12:00:00Z".parse().unwrap();

        let first = s.next_after(before, &ny()).unwrap().unwrap();
        let second = s.next_after(first, &ny()).unwrap().unwrap();

        assert!(
            local(first).starts_with("2026-11-01 01:00:00"),
            "got {}",
            local(first)
        );
        assert!(
            second.to_zoned(ny()).date().to_string() == "2026-11-02",
            "1am on the 1st fired twice; second occurrence was {}",
            local(second)
        );
    }

    #[test]
    fn schedule_outside_a_dst_transition_is_unaffected() {
        let s = Schedule::parse("0 2 * * *").unwrap();
        let before: Timestamp = "2026-06-01T12:00:00Z".parse().unwrap();
        let next = s.next_after(before, &ny()).unwrap().unwrap();
        assert_eq!(local(next), "2026-06-02 02:00:00 -0400");
    }
}
