use anyhow::{Result, bail};
use std::collections::BTreeSet;

/// 6-field input is assumed seconds-first (`sec min hour dom mon dow`).
/// A different field order is silently reinterpreted.
pub fn normalize_cron(expr: &str) -> Result<String> {
    let fields: Vec<&str> = expr.split_whitespace().collect();
    let mut fields: Vec<String> = match fields.len() {
        5 => std::iter::once("0")
            .chain(fields)
            .map(str::to_string)
            .collect(),
        6 => fields.iter().copied().map(str::to_string).collect(),
        n => bail!("cron expression must have 5 or 6 fields, found {n}: {expr:?}"),
    };

    // Day-of-week is the last field in both accepted shapes.
    let dow = fields.last_mut().expect("six fields by construction");
    *dow = posix_dow_to_quartz(dow)?;

    Ok(fields.join(" "))
}

/// POSIX numbers Sunday `0`-`6`, `7` as an alias. `jiff-cron` uses
/// Quartz `1`-`7`. Names already agree and pass through untouched.
fn posix_dow_to_quartz(field: &str) -> Result<String> {
    let terms: Vec<String> = field
        .split(',')
        .map(|term| translate_term(term, field))
        .collect::<Result<_>>()?;
    Ok(terms.join(","))
}

/// A lone step counts days, not a single day. It stays as written.
fn translate_term(term: &str, field: &str) -> Result<String> {
    let (base, step) = match term.split_once('/') {
        Some((base, step)) => (base, Some(step)),
        None => (term, None),
    };

    if let Some((start, end)) = numeric_range(base) {
        return expand_range(start, end, step, base, field);
    }

    match step {
        Some(step) => Ok(format!("{}/{step}", translate_base(base, field)?)),
        None => translate_base(base, field),
    }
}

/// A named range is already the same seven days in Quartz order.
fn numeric_range(base: &str) -> Option<(u32, u32)> {
    let (start, end) = base.split_once('-')?;
    Some((start.parse().ok()?, end.parse().ok()?))
}

/// POSIX spells Sunday both `0` and `7`. Renumbering only the
/// endpoints would turn `0-7` into Quartz `1-1`, not every day.
///
/// `jiff-cron` steps only one contiguous range. This list is
/// expanded and not contiguous. The step is applied here instead.
fn expand_range(
    start: u32,
    end: u32,
    step: Option<&str>,
    base: &str,
    field: &str,
) -> Result<String> {
    let mut days = posix_range_days(start, end, field)?;

    if let Some(step) = step {
        let Some(n) = step.parse::<usize>().ok().filter(|n| *n > 0) else {
            bail!("day-of-week step {step:?} in {field:?} must be a positive number");
        };
        days = days.into_iter().step_by(n).collect();
    }

    let quartz: BTreeSet<u32> = days.into_iter().map(quartz_day).collect();
    if quartz.is_empty() {
        bail!("day-of-week range {base:?} in {field:?} selects no days");
    }
    Ok(as_list(&quartz))
}

/// A range whose start is after its end wraps: `6-1` covers Sat,
/// Sun, Mon.
fn posix_range_days(start: u32, end: u32, field: &str) -> Result<Vec<u32>> {
    for n in [start, end] {
        if n > 7 {
            bail!("day-of-week {n} in {field:?} is out of range; crontab allows 0-7");
        }
    }
    if start <= end {
        return Ok((start..=end).collect());
    }
    // 7 and 0 are the same day. A range starting on 7 doesn't really
    // wrap once written the other way.
    let start = if start == 7 { 0 } else { start };
    if start <= end {
        return Ok((start..=end).collect());
    }
    Ok((start..=6).chain(0..=end).collect())
}

fn quartz_day(posix: u32) -> u32 {
    // POSIX spells Sunday both 0 and 7. Quartz has only 1 for it.
    if posix == 7 { 1 } else { posix + 1 }
}

fn as_list(days: &BTreeSet<u32>) -> String {
    let mut parts: Vec<String> = Vec::new();
    let mut days = days.iter().copied().peekable();
    while let Some(start) = days.next() {
        let mut end = start;
        while days.peek().is_some_and(|&n| n == end + 1) {
            end = days.next().expect("just peeked");
        }
        if end > start {
            parts.push(format!("{start}-{end}"));
        } else {
            parts.push(start.to_string());
        }
    }
    parts.join(",")
}

fn translate_base(base: &str, field: &str) -> Result<String> {
    if base == "*" || base == "?" {
        return Ok(base.to_string());
    }
    match base.split_once('-') {
        // Not a numeric range, or `translate_term` would have expanded it.
        Some((start, end)) => Ok(format!(
            "{}-{}",
            translate_point(start, field)?,
            translate_point(end, field)?
        )),
        None => translate_point(base, field),
    }
}

fn translate_point(point: &str, field: &str) -> Result<String> {
    match point.parse::<u32>() {
        Ok(n) if n <= 7 => Ok(quartz_day(n).to_string()),
        Ok(n) => bail!("day-of-week {n} in {field:?} is out of range; crontab allows 0-7"),
        Err(_) => Ok(point.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn five_field_gains_a_seconds_column() {
        assert_eq!(normalize_cron("0 2 * * *").unwrap(), "0 0 2 * * *");
        assert_eq!(normalize_cron("*/15 * * * *").unwrap(), "0 */15 * * * *");
    }

    #[test]
    fn six_field_is_passed_through_unchanged() {
        assert_eq!(normalize_cron("30 0 2 * * *").unwrap(), "30 0 2 * * *");
    }

    #[test]
    fn extra_whitespace_is_tolerated() {
        assert_eq!(normalize_cron("  0   2  * * *  ").unwrap(), "0 0 2 * * *");
    }

    #[test]
    fn wrong_field_count_is_rejected_with_the_count_in_the_message() {
        let err = normalize_cron("0 2 * *").unwrap_err().to_string();
        assert!(err.contains('4'), "message was: {err}");
        assert!(normalize_cron("").is_err());
        assert!(normalize_cron("0 2 * * * * *").is_err());
    }

    fn dow(field: &str) -> String {
        posix_dow_to_quartz(field).unwrap()
    }

    #[test]
    fn wildcards_are_left_alone() {
        assert_eq!(dow("*"), "*");
        assert_eq!(dow("?"), "?");
    }

    #[test]
    fn single_numbers_shift_by_one_with_seven_folding_onto_sunday() {
        assert_eq!(dow("0"), "1");
        assert_eq!(dow("1"), "2");
        assert_eq!(dow("5"), "6");
        assert_eq!(dow("6"), "7");
        assert_eq!(dow("7"), "1");
    }

    #[test]
    fn range_that_stays_contiguous_is_still_emitted_as_a_range() {
        assert_eq!(dow("1-5"), "2-6");
        assert_eq!(dow("2-3"), "3-4");
        assert_eq!(dow("3-3"), "4");
    }

    #[test]
    fn lists_translate_every_element_including_nested_ranges_and_steps() {
        assert_eq!(dow("0,6"), "1,7");
        assert_eq!(dow("1,3,5"), "2,4,6");
        assert_eq!(dow("0,2-4,6"), "1,3-5,7");
        assert_eq!(dow("1-5/2,0"), "2,4,6,1");
    }

    #[test]
    fn steps_on_a_non_range_base_keep_the_interval_as_written() {
        assert_eq!(dow("*/2"), "*/2");
        assert_eq!(dow("1/2"), "2/2");
        assert_eq!(dow("0/6"), "1/6");
    }

    #[test]
    fn steps_on_a_range_are_applied_during_expansion() {
        assert_eq!(dow("1-5/2"), "2,4,6");
        assert_eq!(dow("0-7/2"), "1,3,5,7");
        assert_eq!(dow("0-6/3"), "1,4,7");
        assert_eq!(dow("5-7/2"), "1,6");
        let err = posix_dow_to_quartz("1-5/0").unwrap_err().to_string();
        assert!(err.contains("positive"), "message was: {err}");
    }

    #[test]
    fn names_are_never_renumbered() {
        assert_eq!(dow("SUN"), "SUN");
        assert_eq!(dow("mon"), "mon");
        assert_eq!(dow("MON-FRI"), "MON-FRI");
        assert_eq!(dow("Mon,Wed,Fri"), "Mon,Wed,Fri");
        assert_eq!(dow("MON-FRI/2"), "MON-FRI/2");
    }

    #[test]
    fn range_wrapping_past_the_end_of_the_week_keeps_the_days_on_both_sides() {
        assert_eq!(dow("6-1"), "1-2,7");
        assert_eq!(dow("5-2"), "1-3,6-7");
        assert_eq!(dow("7-1"), "1-2");
    }

    #[test]
    fn range_endpoint_outside_the_legal_days_is_rejected_naming_the_value() {
        for bad in ["1-9", "8-2"] {
            let err = posix_dow_to_quartz(bad).unwrap_err().to_string();
            assert!(err.contains("0-7"), "message for {bad:?} was: {err}");
        }
    }

    #[test]
    fn ranges_are_expanded_so_both_spellings_of_sunday_survive() {
        assert_eq!(dow("0-7"), "1-7");
        assert_eq!(dow("1-7"), "1-7");
        assert_eq!(dow("0-6"), "1-7");
        assert_eq!(dow("5-7"), "1,6-7");
        assert_eq!(dow("6-0"), "1,7");
        assert_eq!(dow("1-5"), "2-6");
    }

    #[test]
    fn out_of_range_day_is_rejected_rather_than_shifted_into_a_valid_one() {
        let err = posix_dow_to_quartz("8").unwrap_err().to_string();
        assert!(err.contains('8'), "message was: {err}");
        assert!(err.contains("0-7"), "message was: {err}");
    }

    #[test]
    fn only_the_day_of_week_field_is_renumbered() {
        assert_eq!(normalize_cron("1 1 1 1 1").unwrap(), "0 1 1 1 1 2");
        assert_eq!(normalize_cron("1 1 1 1 1 1").unwrap(), "1 1 1 1 1 2");
    }
}
