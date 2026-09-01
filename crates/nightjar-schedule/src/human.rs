/// Returns `None` for invalid human syntax, including near-misses
/// like `"daily at 25am"`. This lets the caller give one clear
/// error, not a confusing cron failure.
pub fn to_cron(input: &str) -> Option<String> {
    let lower = input.trim().to_lowercase();
    let words: Vec<&str> = lower.split_whitespace().collect();

    match words.as_slice() {
        // The crontab(5) shortcuts, spelled the way `crontab -l` prints
        // them, so `nightjar import` and a hand-written job file agree.
        ["hourly" | "@hourly"] => Some("0 * * * *".to_string()),
        ["daily" | "@daily" | "@midnight"] => Some("0 0 * * *".to_string()),
        ["@weekly"] => Some("0 0 * * 0".to_string()),
        ["@monthly"] => Some("0 0 1 * *".to_string()),
        ["@yearly" | "@annually"] => Some("0 0 1 1 *".to_string()),
        ["daily", "at", t] => {
            let (h, m) = parse_time(t)?;
            Some(format!("{m} {h} * * *"))
        }
        ["weekdays", "at", t] => {
            let (h, m) = parse_time(t)?;
            Some(format!("{m} {h} * * 1-5"))
        }
        ["weekly", dow, "at", t] => {
            let d = parse_dow(dow)?;
            let (h, m) = parse_time(t)?;
            Some(format!("{m} {h} * * {d}"))
        }
        ["every", n, unit] => {
            let n: u32 = n.parse().ok()?;
            if n == 0 {
                return None;
            }
            match *unit {
                // 60 minutes / 24 hours mean hourly/daily, not a step
                // equal to the field's full range. The step forms below
                // can't say that.
                "minute" | "minutes" if n == 60 => Some("0 * * * *".to_string()),
                "minute" | "minutes" if n <= 59 => Some(format!("*/{n} * * * *")),
                "hour" | "hours" if n == 24 => Some("0 0 * * *".to_string()),
                "hour" | "hours" if n <= 23 => Some(format!("0 */{n} * * *")),
                _ => None,
            }
        }
        _ => None,
    }
}

fn parse_time(t: &str) -> Option<(u32, u32)> {
    let (body, is_pm) = if let Some(b) = t.strip_suffix("am") {
        (b, Some(false))
    } else if let Some(b) = t.strip_suffix("pm") {
        (b, Some(true))
    } else {
        (t, None)
    };

    let (h_str, m_str) = match body.split_once(':') {
        Some((h, m)) => (h, m),
        None => (body, "0"),
    };

    let mut hour: u32 = h_str.parse().ok()?;
    let minute: u32 = m_str.parse().ok()?;
    if minute > 59 {
        return None;
    }

    match is_pm {
        Some(is_pm) => {
            if !(1..=12).contains(&hour) {
                return None;
            }
            hour = match (hour, is_pm) {
                (12, false) => 0,
                (12, true) => 12,
                (h, false) => h,
                (h, true) => h + 12,
            };
        }
        None => {
            if hour > 23 {
                return None;
            }
        }
    }

    Some((hour, minute))
}

fn parse_dow(d: &str) -> Option<u32> {
    Some(match d {
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
    fn malformed_human_input_returns_none_rather_than_guessing() {
        assert_eq!(to_cron("daily at 25am"), None);
        assert_eq!(to_cron("every 0 minutes"), None);
        assert_eq!(to_cron("every 90 minutes"), None);
        assert_eq!(to_cron("weekly funday at 3am"), None);
    }
}
