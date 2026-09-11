#[must_use]
pub fn to_cron(input: &str) -> Option<String> {
    let lower = input.trim().to_lowercase();
    let words: Vec<&str> = lower.split_whitespace().collect();

    match words.as_slice() {
        ["hourly" | "@hourly"] => Some("0 * * * *".to_string()),
        ["daily" | "@daily" | "@midnight"] => Some("0 0 * * *".to_string()),
        ["@weekly"] => Some("0 0 * * 0".to_string()),
        ["@monthly"] => Some("0 0 1 * *".to_string()),
        ["@yearly" | "@annually"] => Some("0 0 1 1 *".to_string()),
        ["daily", "at", time] => {
            let at = TimeOfDay::parse(time)?;
            Some(format!("{} {} * * *", at.minute, at.hour))
        }
        ["weekdays", "at", time] => {
            let at = TimeOfDay::parse(time)?;
            Some(format!("{} {} * * 1-5", at.minute, at.hour))
        }
        ["weekly", day, "at", time] => {
            let day = cron_day_of_week(day)?;
            let at = TimeOfDay::parse(time)?;
            Some(format!("{} {} * * {day}", at.minute, at.hour))
        }
        ["every", count, unit] => every(count, unit),
        _ => None,
    }
}

fn every(count: &str, unit: &str) -> Option<String> {
    let interval: u32 = count.parse().ok()?;
    if interval == 0 {
        return None;
    }
    match unit {
        "minute" | "minutes" if interval == 60 => Some("0 * * * *".to_string()),
        "minute" | "minutes" if interval <= 59 => Some(format!("*/{interval} * * * *")),
        "hour" | "hours" if interval == 24 => Some("0 0 * * *".to_string()),
        "hour" | "hours" if interval <= 23 => Some(format!("0 */{interval} * * *")),
        _ => None,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Meridiem {
    Am,
    Pm,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct TimeOfDay {
    hour: u32,
    minute: u32,
}

impl TimeOfDay {
    fn parse(text: &str) -> Option<Self> {
        let (clock_face, meridiem) = match (text.strip_suffix("am"), text.strip_suffix("pm")) {
            (Some(rest), _) => (rest, Some(Meridiem::Am)),
            (_, Some(rest)) => (rest, Some(Meridiem::Pm)),
            (None, None) => (text, None),
        };

        let (hours, minutes) = clock_face.split_once(':').unwrap_or((clock_face, "0"));

        let stated_hour: u32 = hours.parse().ok()?;
        let minute: u32 = minutes.parse().ok()?;
        if minute > 59 {
            return None;
        }

        let hour = match meridiem {
            Some(meridiem) => twenty_four_hour(stated_hour, meridiem)?,
            None if stated_hour <= 23 => stated_hour,
            None => return None,
        };
        Some(Self { hour, minute })
    }
}

fn twenty_four_hour(stated_hour: u32, meridiem: Meridiem) -> Option<u32> {
    if !(1..=12).contains(&stated_hour) {
        return None;
    }
    Some(match (stated_hour, meridiem) {
        (12, Meridiem::Am) => 0,
        (12, Meridiem::Pm) => 12,
        (hour, Meridiem::Am) => hour,
        (hour, Meridiem::Pm) => hour + 12,
    })
}

fn cron_day_of_week(name: &str) -> Option<u32> {
    Some(match name {
        "sun" | "sunday" => 0,
        "mon" | "monday" => 1,
        "tue" | "tues" | "tuesday" => 2,
        "wed" | "wednesday" => 3,
        "thu" | "thurs" | "thursday" => 4,
        "fri" | "friday" => 5,
        "sat" | "saturday" => 6,
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn to_cron_returns_the_expected_cron_when_input_is_a_bare_keyword() {
        assert_eq!(to_cron("hourly").as_deref(), Some("0 * * * *"));
        assert_eq!(to_cron("daily").as_deref(), Some("0 0 * * *"));
    }

    #[test]
    fn crontab_at_shortcuts_lower_to_the_same_cron_as_cron_itself() {
        assert_eq!(to_cron("@hourly").as_deref(), Some("0 * * * *"));
        assert_eq!(to_cron("@daily").as_deref(), Some("0 0 * * *"));
        assert_eq!(to_cron("@midnight").as_deref(), Some("0 0 * * *"));
        assert_eq!(to_cron("@weekly").as_deref(), Some("0 0 * * 0"));
        assert_eq!(to_cron("@monthly").as_deref(), Some("0 0 1 * *"));
        assert_eq!(to_cron("@yearly").as_deref(), Some("0 0 1 1 *"));
        assert_eq!(to_cron("@annually").as_deref(), Some("0 0 1 1 *"));
        assert_eq!(to_cron("@Daily").as_deref(), Some("0 0 * * *"));
    }

    #[test]
    fn unknown_at_shortcut_is_not_guessed() {
        assert_eq!(to_cron("@reboot"), None);
        assert_eq!(to_cron("@fortnightly"), None);
    }

    #[test]
    fn to_cron_returns_the_expected_cron_when_input_is_daily_at_a_specific_time() {
        assert_eq!(to_cron("daily at 2am").as_deref(), Some("0 2 * * *"));
        assert_eq!(to_cron("daily at 2pm").as_deref(), Some("0 14 * * *"));
        assert_eq!(to_cron("daily at 14:30").as_deref(), Some("30 14 * * *"));
        assert_eq!(to_cron("daily at 2:30am").as_deref(), Some("30 2 * * *"));
        assert_eq!(to_cron("daily at 12am").as_deref(), Some("0 0 * * *"));
        assert_eq!(to_cron("daily at 12pm").as_deref(), Some("0 12 * * *"));
    }

    #[test]
    fn to_cron_returns_the_expected_cron_when_input_is_weekdays_or_weekly() {
        assert_eq!(to_cron("weekdays at 9am").as_deref(), Some("0 9 * * 1-5"));
        assert_eq!(to_cron("weekly sun at 3am").as_deref(), Some("0 3 * * 0"));
        assert_eq!(to_cron("weekly mon at 3am").as_deref(), Some("0 3 * * 1"));
    }

    #[test]
    fn to_cron_returns_the_expected_cron_when_input_is_every_n_units() {
        assert_eq!(to_cron("every 15 minutes").as_deref(), Some("*/15 * * * *"));
        assert_eq!(to_cron("every 1 minute").as_deref(), Some("*/1 * * * *"));
        assert_eq!(to_cron("every 6 hours").as_deref(), Some("0 */6 * * *"));
    }

    #[test]
    fn every_60_minutes_and_every_24_hours_mean_hourly_and_daily() {
        assert_eq!(to_cron("every 60 minutes").as_deref(), Some("0 * * * *"));
        assert_eq!(to_cron("every 24 hours").as_deref(), Some("0 0 * * *"));
        assert_eq!(
            to_cron("every 60 minutes").as_deref(),
            to_cron("hourly").as_deref()
        );
        assert_eq!(
            to_cron("every 24 hours").as_deref(),
            to_cron("daily").as_deref()
        );
    }

    #[test]
    fn every_61_minutes_and_every_25_hours_are_still_rejected() {
        assert_eq!(to_cron("every 61 minutes"), None);
        assert_eq!(to_cron("every 25 hours"), None);
    }

    #[test]
    fn case_and_spacing_are_forgiving() {
        assert_eq!(to_cron("Daily At 2AM").as_deref(), Some("0 2 * * *"));
        assert_eq!(
            to_cron("  every   15   minutes ").as_deref(),
            Some("*/15 * * * *")
        );
    }

    #[test]
    fn non_human_input_returns_none_so_cron_parsing_can_take_over() {
        assert_eq!(to_cron("0 2 * * *"), None);
        assert_eq!(to_cron("*/15 * * * *"), None);
        assert_eq!(to_cron(""), None);
    }

    #[test]
    fn twelve_am_is_midnight_and_twelve_pm_is_noon() {
        assert_eq!(twenty_four_hour(12, Meridiem::Am), Some(0));
        assert_eq!(twenty_four_hour(12, Meridiem::Pm), Some(12));
        assert_eq!(twenty_four_hour(1, Meridiem::Am), Some(1));
        assert_eq!(twenty_four_hour(1, Meridiem::Pm), Some(13));
        assert_eq!(twenty_four_hour(0, Meridiem::Am), None);
        assert_eq!(twenty_four_hour(13, Meridiem::Pm), None);
    }

    #[test]
    fn time_of_day_keeps_hour_and_minute_apart() {
        assert_eq!(
            TimeOfDay::parse("2:30pm"),
            Some(TimeOfDay {
                hour: 14,
                minute: 30
            })
        );
        assert_eq!(
            TimeOfDay::parse("7"),
            Some(TimeOfDay { hour: 7, minute: 0 })
        );
        assert_eq!(TimeOfDay::parse("24"), None);
        assert_eq!(TimeOfDay::parse("1:60"), None);
    }

    #[test]
    fn malformed_human_input_returns_none_rather_than_guessing() {
        assert_eq!(to_cron("daily at 25am"), None);
        assert_eq!(to_cron("every 0 minutes"), None);
        assert_eq!(to_cron("every 90 minutes"), None);
        assert_eq!(to_cron("weekly funday at 3am"), None);
    }
}
