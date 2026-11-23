//! Capture-line parser (task 080).
//!
//! One typed line becomes a note plus optional group, priority and deadline
//! chips. Everything here is local, deterministic and pure: the parser takes
//! the current instant and zone from the caller and never calls the network,
//! the model, or the system clock.
//!
//! Conservative rules, in order:
//!
//! * a group is only a whole token that starts with # followed by a name;
//!   # inside a word (C#) is text;
//! * importance is only a whole token: !, !important or !high;
//! * a deadline must sit at the end of the line (after chips are removed),
//!   optionally introduced by at, by, on or due; "the 4pm thing" is text;
//! * anything ambiguous (invalid dates, 25:00, unknown words) is left alone.

use jiff::civil::{Date, Weekday};
use jiff::tz::TimeZone;
use jiff::{Span as DurationSpan, Timestamp};
use serde::{Deserialize, Serialize};

use crate::time::TimeSpec;

/// A group name mentioned by the line. Unknown names are proposed, not
/// created; known is true only when it matched the caller's group list.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CaptureGroup {
    pub name: String,
    pub known: bool,
}

/// A half-open byte range in the original input that was consumed as a chip.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Span {
    pub start: usize,
    pub end: usize,
}

impl Span {
    fn of(token: &Token<'_>) -> Self {
        Self {
            start: token.start,
            end: token.end,
        }
    }
}

/// Everything a capture line produced, ready for the composer to apply.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ParsedCapture {
    /// Remaining note text with consumed chips removed and whitespace cleaned.
    pub text: String,
    /// Last group chip, if any.
    pub group: Option<CaptureGroup>,
    /// An importance chip ("!" or "!important") was present.
    pub important: bool,
    /// Deadline, when a date and/or time phrase was consumed.
    pub due: Option<TimeSpec>,
    /// Byte ranges of the original input that were consumed as chips.
    pub consumed: Vec<Span>,
}

/// Parses one capture line.
///
/// now_utc_ms is the current instant; tz is the board zone used for "today"
/// and for the produced TimeSpec. known_groups supplies the existing group
/// names so the caller can distinguish a proposal from an existing group.
/// An unknown zone still yields text, group and priority chips, but no
/// deadline: a deadline that cannot be resolved is left in the text.
pub fn parse_capture(
    input: &str,
    now_utc_ms: i64,
    tz: &str,
    known_groups: &[String],
) -> ParsedCapture {
    let tokens = tokenize(input);
    if tokens.is_empty() {
        return ParsedCapture::default();
    }

    let mut consumed = vec![false; tokens.len()];
    let mut group: Option<CaptureGroup> = None;
    let mut important = false;

    for (index, token) in tokens.iter().enumerate() {
        if let Some(name) = group_name(token.text) {
            group = Some(CaptureGroup {
                known: false,
                name: name.to_owned(),
            });
            consumed[index] = true;
        } else if is_importance(token.text) {
            important = true;
            consumed[index] = true;
        }
    }

    if let Some(found) = &mut group
        && let Some(canonical) = known_groups
            .iter()
            .find(|known| known.eq_ignore_ascii_case(&found.name))
    {
        found.name = canonical.clone();
        found.known = true;
    }

    let remaining: Vec<usize> = (0..tokens.len())
        .filter(|index| !consumed[*index])
        .collect();
    let mut due = None;
    if let Some(today) = local_today(now_utc_ms, tz)
        && let Some((used, spec)) = parse_due_suffix(&tokens, &remaining, today, now_utc_ms, tz)
    {
        for index in remaining.iter().rev().take(used) {
            consumed[*index] = true;
        }
        due = Some(spec);
    }

    let mut spans = Vec::new();
    let mut parts: Vec<&str> = Vec::new();
    for (index, token) in tokens.iter().enumerate() {
        if consumed[index] {
            spans.push(Span::of(token));
        } else {
            parts.push(token.text);
        }
    }
    spans.sort_by_key(|span| span.start);

    ParsedCapture {
        text: parts.join(" "),
        group,
        important,
        due,
        consumed: spans,
    }
}

/// A whitespace-delimited token with its byte range in the input.
struct Token<'a> {
    text: &'a str,
    start: usize,
    end: usize,
}

fn tokenize(input: &str) -> Vec<Token<'_>> {
    let mut tokens = Vec::new();
    let mut start: Option<usize> = None;
    for (index, ch) in input.char_indices() {
        if ch.is_whitespace() {
            if let Some(from) = start.take() {
                tokens.push(Token {
                    text: &input[from..index],
                    start: from,
                    end: index,
                });
            }
        } else if start.is_none() {
            start = Some(index);
        }
    }
    if let Some(from) = start {
        tokens.push(Token {
            text: &input[from..],
            start: from,
            end: input.len(),
        });
    }
    tokens
}

/// Group name inside a #token, if the token is a well-formed group chip.
fn group_name(token: &str) -> Option<&str> {
    let name = token.strip_prefix('#')?;
    if name.is_empty()
        || !name
            .chars()
            .all(|ch| ch.is_alphanumeric() || ch == '_' || ch == '-')
    {
        return None;
    }
    Some(name)
}

/// A whole "!" token, or the long forms "!important" / "!high".
fn is_importance(token: &str) -> bool {
    matches!(
        token.to_ascii_lowercase().as_str(),
        "!" | "!important" | "!high"
    )
}

/// Today's civil date in the given zone, or None when the zone is invalid.
fn local_today(now_utc_ms: i64, tz: &str) -> Option<Date> {
    let zone = TimeZone::get(tz).ok()?;
    let now = Timestamp::from_millisecond(now_utc_ms).ok()?;
    Some(zone.to_datetime(now).date())
}

/// Parses the deadline at the end of the non-chip tokens, returning how many
/// of those tokens it used (from the end) and the deadline itself.
fn parse_due_suffix(
    tokens: &[Token<'_>],
    remaining: &[usize],
    today: Date,
    now_utc_ms: i64,
    tz: &str,
) -> Option<(usize, TimeSpec)> {
    if remaining.is_empty() {
        return None;
    }

    let time = parse_time_suffix(tokens, remaining);
    let date_part_end = match time {
        Some((used, _, _)) => remaining.len().checked_sub(used)?,
        None => remaining.len(),
    };

    if let Some((time_used, hour, minute)) = time {
        if let Some((date_used, choice)) =
            parse_date_suffix(tokens, &remaining[..date_part_end], today)
        {
            let used = date_used + time_used;
            let spec = build_due(choice, Some((hour, minute)), today, now_utc_ms, tz, false)?;
            return Some((used, spec));
        }
        // A time with no date attaches to today, or tomorrow when it has
        // already passed. This is the only form that rolls forward.
        let spec = build_due(
            DateChoice::Today,
            Some((hour, minute)),
            today,
            now_utc_ms,
            tz,
            true,
        )?;
        return Some((time_used, spec));
    }

    let (date_used, choice) = parse_date_suffix(tokens, remaining, today)?;
    let spec = build_due(choice, None, today, now_utc_ms, tz, false)?;
    Some((date_used, spec))
}

/// Parses a time at the end of the token list: at 4pm, 4:30pm, 4 pm, 09:30,
/// noon, midnight. Returns (tokens used, hour, minute).
fn parse_time_suffix(tokens: &[Token<'_>], remaining: &[usize]) -> Option<(usize, u8, u8)> {
    let count = remaining.len();
    if count == 0 {
        return None;
    }

    let last = tokens[remaining[count - 1]].text;

    // "4 pm" / "4:30 pm": the meridiem is its own token.
    if let Some(pm) = meridiem_word(last)
        && count >= 2
        && let Some(hour) = parse_bare_hour(tokens[remaining[count - 2]].text)
    {
        let hour = apply_meridiem(hour, pm)?;
        let mut used = 2;
        if count >= 3 && is_time_prefix(tokens[remaining[count - 3]].text) {
            used = 3;
        }
        return Some((used, hour, 0));
    }

    let clock = parse_clock_token(last)?;
    let mut used = 1;
    if count >= 2 && is_time_prefix(tokens[remaining[count - 2]].text) {
        used = 2;
    }
    Some((used, clock.hour, clock.minute))
}

fn is_time_prefix(token: &str) -> bool {
    token.eq_ignore_ascii_case("at")
}

#[derive(Clone, Copy)]
struct Clock {
    hour: u8,
    minute: u8,
}

fn meridiem_word(token: &str) -> Option<bool> {
    // true = pm
    if token.eq_ignore_ascii_case("pm") {
        Some(true)
    } else if token.eq_ignore_ascii_case("am") {
        Some(false)
    } else {
        None
    }
}

/// 12-hour clock applied to a bare hour: 12am is midnight, 12pm is noon.
fn apply_meridiem(hour: u8, pm: bool) -> Option<u8> {
    match (hour, pm) {
        (12, false) => Some(0),
        (12, true) => Some(12),
        (1..=11, false) => Some(hour),
        (1..=11, true) => Some(hour + 12),
        _ => None,
    }
}

/// Parses one clock token: noon, midnight, 4pm, 4:30pm, 09:30. Bare numbers
/// are not times unless a meridiem follows in a separate token.
fn parse_clock_token(token: &str) -> Option<Clock> {
    let lower = token.to_ascii_lowercase();
    match lower.as_str() {
        "noon" => {
            return Some(Clock {
                hour: 12,
                minute: 0,
            });
        }
        "midnight" => {
            return Some(Clock { hour: 0, minute: 0 });
        }
        _ => {}
    }

    if let Some(stripped) = lower
        .strip_suffix("pm")
        .or_else(|| lower.strip_suffix("am"))
    {
        let pm = lower.ends_with("pm");
        let (hour, minute) = parse_hour_minute(stripped)?;
        let hour = apply_meridiem(hour, pm)?;
        return Some(Clock { hour, minute });
    }

    if lower.contains(':') {
        let (hour, minute) = parse_hour_minute(&lower)?;
        if hour > 23 {
            return None;
        }
        return Some(Clock { hour, minute });
    }

    None
}

/// Parses a bare hour 1..=12 used with a separate am/pm token.
fn parse_bare_hour(text: &str) -> Option<u8> {
    if text.is_empty() || text.len() > 2 || !text.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    let hour: u8 = text.parse().ok()?;
    (1..=12).contains(&hour).then_some(hour)
}

/// Parses H or H:MM into (hour, minute), without a meridiem.
fn parse_hour_minute(text: &str) -> Option<(u8, u8)> {
    let (hour, minute) = match text.split_once(':') {
        Some((hour, minute)) => (hour, minute),
        None => (text, "0"),
    };
    if hour.is_empty() || hour.len() > 2 || !hour.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    let minute = if minute.is_empty() { "0" } else { minute };
    if minute.len() > 2 || !minute.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    let hour: u8 = hour.parse().ok()?;
    let minute: u8 = minute.parse().ok()?;
    if minute > 59 {
        return None;
    }
    Some((hour, minute))
}

/// What a date phrase means before it is turned into a concrete date.
#[derive(Clone, Copy, Debug)]
enum DateChoice {
    Today,
    Tomorrow,
    Tonight,
    /// The next occurrence of this weekday, rolling forward a week when the
    /// requested time has already passed today.
    Weekday(Weekday),
    /// The weekday strictly after today (the "next mon" form).
    NextWeekday(Weekday),
    Exact(Date),
    PlusDays(i64),
}

/// Parses a date at the end of the token list, returning how many tokens it
/// used (including an optional by/on/due prefix).
fn parse_date_suffix(
    tokens: &[Token<'_>],
    remaining: &[usize],
    today: Date,
) -> Option<(usize, DateChoice)> {
    let (used, choice) = match_date_suffix(tokens, remaining, today)?;
    let prefix_index = remaining.len().checked_sub(used + 1);
    if let Some(index) = prefix_index
        && is_date_prefix(tokens[remaining[index]].text)
    {
        return Some((used + 1, choice));
    }
    Some((used, choice))
}

fn is_date_prefix(token: &str) -> bool {
    token.eq_ignore_ascii_case("by")
        || token.eq_ignore_ascii_case("on")
        || token.eq_ignore_ascii_case("due")
}

fn match_date_suffix(
    tokens: &[Token<'_>],
    remaining: &[usize],
    today: Date,
) -> Option<(usize, DateChoice)> {
    let count = remaining.len();
    if count == 0 {
        return None;
    }

    // in N days / in N weeks
    if count >= 3 {
        let unit = tokens[remaining[count - 1]].text;
        let number = tokens[remaining[count - 2]].text;
        let prefix = tokens[remaining[count - 3]].text;
        if prefix.eq_ignore_ascii_case("in")
            && number.bytes().all(|byte| byte.is_ascii_digit())
            && let Ok(amount) = number.parse::<i64>()
            && amount > 0
            && amount <= 3650
        {
            if unit.eq_ignore_ascii_case("day") || unit.eq_ignore_ascii_case("days") {
                return Some((3, DateChoice::PlusDays(amount)));
            }
            if unit.eq_ignore_ascii_case("week") || unit.eq_ignore_ascii_case("weeks") {
                return Some((3, DateChoice::PlusDays(amount.checked_mul(7)?)));
            }
        }
    }

    // next monday
    if count >= 2
        && tokens[remaining[count - 2]]
            .text
            .eq_ignore_ascii_case("next")
        && let Some(weekday) = parse_weekday(tokens[remaining[count - 1]].text)
    {
        return Some((2, DateChoice::NextWeekday(weekday)));
    }

    let last = tokens[remaining[count - 1]].text;
    if last.eq_ignore_ascii_case("today") {
        return Some((1, DateChoice::Today));
    }
    if last.eq_ignore_ascii_case("tomorrow") {
        return Some((1, DateChoice::Tomorrow));
    }
    if last.eq_ignore_ascii_case("tonight") {
        return Some((1, DateChoice::Tonight));
    }
    if let Some(weekday) = parse_weekday(last) {
        return Some((1, DateChoice::Weekday(weekday)));
    }
    if let Some(date) = parse_iso_date(last).or_else(|| parse_day_month(last, today)) {
        return Some((1, DateChoice::Exact(date)));
    }
    None
}

fn parse_weekday(token: &str) -> Option<Weekday> {
    match token.to_ascii_lowercase().as_str() {
        "mon" | "monday" => Some(Weekday::Monday),
        "tue" | "tues" | "tuesday" => Some(Weekday::Tuesday),
        "wed" | "weds" | "wednesday" => Some(Weekday::Wednesday),
        "thu" | "thur" | "thurs" | "thursday" => Some(Weekday::Thursday),
        "fri" | "friday" => Some(Weekday::Friday),
        "sat" | "saturday" => Some(Weekday::Saturday),
        "sun" | "sunday" => Some(Weekday::Sunday),
        _ => None,
    }
}

fn parse_iso_date(token: &str) -> Option<Date> {
    let mut parts = token.split('-');
    let year = parts.next()?;
    let month = parts.next()?;
    let day = parts.next()?;
    if parts.next().is_some() || year.len() != 4 || month.len() > 2 || day.len() > 2 {
        return None;
    }
    let year: i16 = year.parse().ok()?;
    let month: i8 = month.parse().ok()?;
    let day: i8 = day.parse().ok()?;
    if !(1970..=2100).contains(&year) || !(1..=12).contains(&month) || !(1..=31).contains(&day) {
        return None;
    }
    Date::new(year, month, day).ok()
}

fn parse_day_month(token: &str, today: Date) -> Option<Date> {
    let (day, month) = token.split_once('/')?;
    if day.is_empty() || month.is_empty() || day.len() > 2 || month.len() > 2 {
        return None;
    }
    let day: i8 = day.parse().ok()?;
    let month: i8 = month.parse().ok()?;
    if !(1..=31).contains(&day) || !(1..=12).contains(&month) {
        return None;
    }
    // D/M carries no year; use the board zone's current year.
    Date::new(today.year(), month, day).ok()
}

/// Builds the concrete TimeSpec for a choice and optional clock.
fn build_due(
    choice: DateChoice,
    time: Option<(u8, u8)>,
    today: Date,
    now_utc_ms: i64,
    tz: &str,
    roll_time_only: bool,
) -> Option<TimeSpec> {
    let (hour, minute) = time.unwrap_or(match choice {
        DateChoice::Tonight => (20, 0),
        _ => (9, 0),
    });

    match choice {
        DateChoice::Today
        | DateChoice::Tomorrow
        | DateChoice::Tonight
        | DateChoice::PlusDays(_) => {
            let date = match choice {
                DateChoice::Tomorrow => add_days(today, 1)?,
                DateChoice::PlusDays(days) => add_days(today, days)?,
                _ => today,
            };
            let (spec, utc_ms) = resolve_on(date, hour, minute, tz)?;
            if roll_time_only && utc_ms <= now_utc_ms {
                let tomorrow = add_days(date, 1)?;
                return resolve_on(tomorrow, hour, minute, tz).map(|(spec, _)| spec);
            }
            Some(spec)
        }
        DateChoice::Exact(date) => resolve_on(date, hour, minute, tz).map(|(spec, _)| spec),
        DateChoice::Weekday(weekday) => {
            let delta = weekday_delta(today.weekday(), weekday);
            let candidate = add_days(today, delta)?;
            let (spec, utc_ms) = resolve_on(candidate, hour, minute, tz)?;
            if utc_ms > now_utc_ms {
                return Some(spec);
            }
            let next = add_days(candidate, 7)?;
            resolve_on(next, hour, minute, tz).map(|(spec, _)| spec)
        }
        DateChoice::NextWeekday(weekday) => {
            let delta = weekday_delta(today.weekday(), weekday);
            let delta = if delta == 0 { 7 } else { delta };
            let date = add_days(today, delta)?;
            resolve_on(date, hour, minute, tz).map(|(spec, _)| spec)
        }
    }
}

fn resolve_on(date: Date, hour: u8, minute: u8, tz: &str) -> Option<(TimeSpec, i64)> {
    let spec = TimeSpec {
        local: format!("{date}T{hour:02}:{minute:02}:00"),
        tz: tz.to_owned(),
    };
    let resolved = spec.resolve().ok()?;
    Some((spec, resolved.utc_ms))
}

fn add_days(date: Date, days: i64) -> Option<Date> {
    date.checked_add(DurationSpan::new().days(days)).ok()
}

fn weekday_delta(from: Weekday, to: Weekday) -> i64 {
    (weekday_index(to) - weekday_index(from)).rem_euclid(7)
}

fn weekday_index(weekday: Weekday) -> i64 {
    match weekday {
        Weekday::Monday => 0,
        Weekday::Tuesday => 1,
        Weekday::Wednesday => 2,
        Weekday::Thursday => 3,
        Weekday::Friday => 4,
        Weekday::Saturday => 5,
        Weekday::Sunday => 6,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const TZ: &str = "Europe/London";

    fn now_at(local: &str) -> i64 {
        TimeSpec {
            local: local.to_owned(),
            tz: TZ.to_owned(),
        }
        .resolve()
        .expect("test instant resolves")
        .utc_ms
    }

    fn parse(input: &str) -> ParsedCapture {
        // Sunday 2026-11-22 08:00 in London (GMT).
        parse_capture(input, now_at("2026-11-22T08:00:00"), TZ, &[])
    }

    fn due_local(parsed: &ParsedCapture) -> &str {
        parsed.due.as_ref().expect("due time").local.as_str()
    }

    #[test]
    fn empty_input_is_empty() {
        let parsed = parse("");
        assert_eq!(parsed, ParsedCapture::default());
        assert_eq!(parse("   ").text, "");
    }

    #[test]
    fn plain_text_is_untouched_and_has_no_chips() {
        let parsed = parse("buy milk, eggs and bread!");
        assert_eq!(parsed.text, "buy milk, eggs and bread!");
        assert!(parsed.group.is_none());
        assert!(!parsed.important);
        assert!(parsed.due.is_none());
        assert!(parsed.consumed.is_empty());
    }

    #[test]
    fn groups_are_whole_hash_tokens_and_last_wins() {
        let parsed = parse("#home buy milk #errands");
        assert_eq!(parsed.text, "buy milk");
        let group = parsed.group.expect("group");
        assert_eq!(group.name, "errands");
        assert!(!group.known);

        let known = vec!["Home".to_owned()];
        let parsed = parse_capture("buy milk #home", now_at("2026-11-22T08:00:00"), TZ, &known);
        let group = parsed.group.expect("group");
        assert_eq!(group.name, "Home");
        assert!(group.known);
    }

    #[test]
    fn a_hash_inside_a_word_is_not_a_group() {
        for input in ["learn C#", "C#7 notes", "mail#work", "#"] {
            let parsed = parse(input);
            assert!(parsed.group.is_none(), "input: {input}");
            assert_eq!(parsed.text, input);
        }
    }

    #[test]
    fn importance_is_a_whole_bang_token() {
        for input in [
            "! write it",
            "write it !",
            "!important write it",
            "!HIGH write it",
        ] {
            let parsed = parse(input);
            assert_eq!(parsed.text, "write it", "input: {input}");
            assert!(parsed.important, "input: {input}");
        }
        assert!(!parse("!urgent write it").important);
        assert!(!parse("!low write it").important);
        assert_eq!(parse("write!").text, "write!");
        assert!(!parse("write!").important);
    }

    #[test]
    fn date_chips_at_the_end() {
        let cases = [
            ("buy milk today", "buy milk", "2026-11-22T09:00:00"),
            ("buy milk tomorrow", "buy milk", "2026-11-23T09:00:00"),
            ("buy milk tonight", "buy milk", "2026-11-22T20:00:00"),
            ("pay in 3 days", "pay", "2026-11-25T09:00:00"),
            ("pay in 2 weeks", "pay", "2026-12-06T09:00:00"),
            ("ship 2026-12-01", "ship", "2026-12-01T09:00:00"),
            ("ship 3/4", "ship", "2026-04-03T09:00:00"),
            ("meet fri", "meet", "2026-11-27T09:00:00"),
            ("meet next mon", "meet", "2026-11-23T09:00:00"),
            ("pay rent by friday", "pay rent", "2026-11-27T09:00:00"),
            ("call on monday", "call", "2026-11-23T09:00:00"),
        ];
        for (input, text, due) in cases {
            let parsed = parse(input);
            assert_eq!(parsed.text, text, "input: {input}");
            assert_eq!(due_local(&parsed), due, "input: {input}");
        }
    }

    #[test]
    fn time_chips_and_the_time_only_rollover() {
        let parsed = parse("call at 4pm");
        assert_eq!(parsed.text, "call");
        assert_eq!(due_local(&parsed), "2026-11-22T16:00:00");

        let parsed = parse("call 4:30pm");
        assert_eq!(parsed.text, "call");
        assert_eq!(due_local(&parsed), "2026-11-22T16:30:00");

        let parsed = parse("call 4 pm");
        assert_eq!(parsed.text, "call");
        assert_eq!(due_local(&parsed), "2026-11-22T16:00:00");

        let parsed = parse("call 09:30");
        assert_eq!(parsed.text, "call");
        assert_eq!(due_local(&parsed), "2026-11-22T09:30:00");

        let parsed = parse("lunch at noon");
        assert_eq!(due_local(&parsed), "2026-11-22T12:00:00");

        // Midnight has already passed at 08:00, so it rolls to tomorrow.
        let parsed = parse("sleep at midnight");
        assert_eq!(due_local(&parsed), "2026-11-23T00:00:00");

        // 17:00 is past, so a bare time rolls to tomorrow.
        let parsed = parse_capture("call at 5pm", now_at("2026-11-22T17:00:00"), TZ, &[]);
        assert_eq!(due_local(&parsed), "2026-11-23T17:00:00");
        assert_eq!(parsed.text, "call");
    }

    #[test]
    fn dates_and_times_combine() {
        let cases = [
            ("meet tomorrow at 4pm", "meet", "2026-11-23T16:00:00"),
            ("meet tomorrow 16:00", "meet", "2026-11-23T16:00:00"),
            ("ship 2026-12-01 09:30", "ship", "2026-12-01T09:30:00"),
            ("pay in 2 days at 17:00", "pay", "2026-11-24T17:00:00"),
            ("lunch fri at noon", "lunch", "2026-11-27T12:00:00"),
            ("demo tonight at 9pm", "demo", "2026-11-22T21:00:00"),
        ];
        for (input, text, due) in cases {
            let parsed = parse(input);
            assert_eq!(parsed.text, text, "input: {input}");
            assert_eq!(due_local(&parsed), due, "input: {input}");
        }
    }

    #[test]
    fn a_weekday_rolls_forward_when_its_time_has_passed() {
        // Sunday 08:00: Sunday 09:00 is still ahead, so plain "sun" is today.
        let parsed = parse("meet sun");
        assert_eq!(due_local(&parsed), "2026-11-22T09:00:00");

        // At 10:00 the default 09:00 has passed, so it rolls a week.
        let parsed = parse_capture("meet sun", now_at("2026-11-22T10:00:00"), TZ, &[]);
        assert_eq!(due_local(&parsed), "2026-11-29T09:00:00");

        // "next sun" is strictly after today.
        let parsed = parse("meet next sun");
        assert_eq!(due_local(&parsed), "2026-11-29T09:00:00");

        let parsed = parse("meet friday at 4pm");
        assert_eq!(due_local(&parsed), "2026-11-27T16:00:00");
    }

    #[test]
    fn a_deadline_must_be_at_the_end_or_prefixed() {
        for input in [
            "the 4pm thing",
            "4pm standup",
            "tomorrow is another day",
            "call at 25:00",
            "ship 2026-02-30",
            "ship 31/13",
        ] {
            let parsed = parse(input);
            assert!(parsed.due.is_none(), "input: {input}");
            assert_eq!(parsed.text, input, "input: {input}");
        }
    }

    #[test]
    fn chips_and_text_can_share_a_line() {
        let parsed = parse("#home !high buy milk tomorrow at 4pm");
        assert_eq!(parsed.text, "buy milk");
        assert_eq!(parsed.group.as_ref().expect("group").name, "home");
        assert!(parsed.important);
        assert_eq!(due_local(&parsed), "2026-11-23T16:00:00");

        let input = "#home !high buy milk tomorrow at 4pm";
        let chips: Vec<&str> = parsed
            .consumed
            .iter()
            .map(|span| &input[span.start..span.end])
            .collect();
        assert_eq!(chips, vec!["#home", "!high", "tomorrow", "at", "4pm"]);
    }

    #[test]
    fn a_trailing_date_after_a_time_keeps_the_time_as_text() {
        let parsed = parse("meet at 4pm tomorrow");
        assert_eq!(parsed.text, "meet at 4pm");
        assert_eq!(due_local(&parsed), "2026-11-23T09:00:00");
    }

    #[test]
    fn chips_after_the_deadline_still_parse() {
        let parsed = parse("buy milk tomorrow #home");
        assert_eq!(parsed.text, "buy milk");
        assert_eq!(due_local(&parsed), "2026-11-23T09:00:00");
        assert_eq!(parsed.group.expect("group").name, "home");
    }

    #[test]
    fn an_unknown_zone_leaves_the_deadline_as_text() {
        let parsed = parse_capture(
            "buy milk tomorrow",
            now_at("2026-11-22T08:00:00"),
            "Not/AZone",
            &[],
        );
        assert!(parsed.due.is_none());
        assert_eq!(parsed.text, "buy milk tomorrow");
    }

    #[test]
    fn spans_are_sorted_and_point_at_the_original_text() {
        let input = "!high buy milk #home tomorrow";
        let parsed = parse(input);
        assert_eq!(parsed.text, "buy milk");
        let mut previous_end = 0;
        for span in &parsed.consumed {
            assert!(span.start >= previous_end, "spans must not overlap");
            assert!(span.start < span.end);
            assert!(span.end <= input.len());
            previous_end = span.end;
        }
        let words: Vec<&str> = parsed
            .consumed
            .iter()
            .map(|span| &input[span.start..span.end])
            .collect();
        assert_eq!(words, vec!["!high", "#home", "tomorrow"]);
    }

    #[test]
    fn whitespace_is_cleaned_without_dropping_punctuation() {
        let parsed = parse("  buy   milk,   #home   ");
        assert_eq!(parsed.text, "buy milk,");
        assert_eq!(parsed.group.expect("group").name, "home");
    }

    #[test]
    fn a_bare_date_is_all_text_that_remains() {
        let parsed = parse("tomorrow");
        assert_eq!(parsed.text, "");
        assert_eq!(due_local(&parsed), "2026-11-23T09:00:00");
    }
}
