//! Civil time and DST resolution (task 060).
//!
//! Deadlines, reminders and snoozes are stored as wall-clock time plus an IANA
//! time zone and resolved to epoch milliseconds at runtime. Resolution has an
//! explicit policy for the two daylight-saving ambiguities:
//!
//! * a spring-forward **gap** resolves to the first valid instant after the
//!   gap (London 2026-03-29T01:30:00 becomes 02:00 BST, i.e. 01:00Z);
//! * an autumn **fold** resolves to the earlier of the two occurrences
//!   (London 2026-10-25T01:30:00 becomes 00:30Z, still BST).
//!
//! Nothing in this module reads the system clock: callers pass now_utc_ms to
//! [today_bounds]. The system *time zone* is read only by [system_zone], which
//! supplies the board default.

use jiff::Timestamp;
use jiff::civil::DateTime;
use jiff::tz::{AmbiguousOffset, TimeZone};
use serde::{Deserialize, Serialize};

/// Exact length of the stored civil timestamp, YYYY-MM-DDTHH:MM:SS.
const LOCAL_LEN: usize = 19;

/// A wall-clock time plus the IANA zone it is written in.
///
/// "local" is always exactly YYYY-MM-DDTHH:MM:SS; "tz" is an IANA id such as
/// Europe/London. The pair is the source of truth for a deadline, reminder or
/// snooze; the resolved instant is a cache.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TimeSpec {
    /// Civil wall-clock time, YYYY-MM-DDTHH:MM:SS.
    pub local: String,
    /// IANA time zone id, for example Europe/London.
    pub tz: String,
}

impl TimeSpec {
    /// Resolves this wall-clock time in its zone to an instant.
    ///
    /// A time in a spring-forward gap resolves to the first valid instant
    /// after the gap and sets [Resolved::gap]. A time in an autumn fold
    /// resolves to the earlier occurrence and sets [Resolved::ambiguous].
    /// Normal times resolve exactly.
    ///
    /// # Errors
    ///
    /// Returns [TimeError::InvalidZone] when "tz" is not in the time zone
    /// database, [TimeError::InvalidLocalFormat] when "local" is not exactly
    /// YYYY-MM-DDTHH:MM:SS, and [TimeError::InvalidLocal] when the components
    /// do not name a real civil time.
    pub fn resolve(&self) -> Result<Resolved, TimeError> {
        let (zone, local) = self.parse()?;
        resolve_local(&zone, local, &self.tz)
    }

    /// Produces the three SQL column values that must always be written
    /// together: *_local, *_tz and *_utc_ms.
    ///
    /// The returned "local" is the canonical form of the requested wall time,
    /// so re-resolving it reproduces "utc_ms" (including the gap/fold policy).
    ///
    /// # Errors
    ///
    /// Returns the same errors as [TimeSpec::resolve].
    pub fn to_sql_parts(&self) -> Result<TimeParts, TimeError> {
        let (zone, local) = self.parse()?;
        let resolved = resolve_local(&zone, local, &self.tz)?;
        Ok(TimeParts {
            local: format!("{local:.0}"),
            tz: self.tz.clone(),
            utc_ms: resolved.utc_ms,
        })
    }

    /// Parses the zone and civil time, rejecting any non-canonical spelling.
    fn parse(&self) -> Result<(TimeZone, DateTime), TimeError> {
        let zone = TimeZone::get(&self.tz).map_err(|source| TimeError::InvalidZone {
            tz: self.tz.clone(),
            source,
        })?;
        let local = parse_local(&self.local)?;
        Ok((zone, local))
    }
}

/// An instant resolved from a [TimeSpec], plus the ambiguity flags that
/// explain how it was chosen.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Resolved {
    /// Unix epoch milliseconds, UTC.
    pub utc_ms: i64,
    /// The requested wall time did not exist; the instant is the first valid
    /// instant after the spring-forward gap.
    pub gap: bool,
    /// The requested wall time occurred twice; the instant is the earlier
    /// occurrence.
    pub ambiguous: bool,
}

/// The three values stored for one deadline/reminder/snooze, always as a set.
///
/// Keeping them in one struct makes it hard to write "utc_ms" without the
/// matching "local" and "tz".
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TimeParts {
    /// Canonical wall-clock time, YYYY-MM-DDTHH:MM:SS.
    pub local: String,
    /// IANA time zone id.
    pub tz: String,
    /// Unix epoch milliseconds, UTC.
    pub utc_ms: i64,
}

/// Errors from parsing or resolving civil times.
#[derive(Debug, thiserror::Error)]
pub enum TimeError {
    #[error("invalid IANA time zone {tz:?}: {source}")]
    InvalidZone {
        tz: String,
        #[source]
        source: jiff::Error,
    },

    #[error("malformed local time {local:?}: {source}")]
    InvalidLocal {
        local: String,
        #[source]
        source: jiff::Error,
    },

    #[error("local time {local:?} must be exactly YYYY-MM-DDTHH:MM:SS")]
    InvalidLocalFormat { local: String },

    #[error("cannot resolve local time {local:?} in {tz:?}: {source}")]
    Resolve {
        local: String,
        tz: String,
        #[source]
        source: jiff::Error,
    },

    #[error("cannot find the first instant after the gap for {local:?} in {tz:?}")]
    MissingTransition { local: String, tz: String },

    #[error("instant {utc_ms} ms UTC is outside the supported time range: {source}")]
    InstantOutOfRange {
        utc_ms: i64,
        #[source]
        source: jiff::Error,
    },

    #[error("cannot find the next local midnight after {date} in {tz:?}: {source}")]
    NextMidnight {
        date: String,
        tz: String,
        #[source]
        source: jiff::Error,
    },
}

/// Resolves a due time, mirroring [TimeSpec::resolve].
///
/// "None" stays "None"; a present value is resolved and its errors are
/// propagated.
///
/// # Errors
///
/// Returns the same errors as [TimeSpec::resolve].
pub fn resolve_due(spec: Option<&TimeSpec>) -> Result<Option<Resolved>, TimeError> {
    resolve_optional(spec)
}

/// Resolves a reminder time, mirroring [TimeSpec::resolve].
///
/// # Errors
///
/// Returns the same errors as [TimeSpec::resolve].
pub fn resolve_remind(spec: Option<&TimeSpec>) -> Result<Option<Resolved>, TimeError> {
    resolve_optional(spec)
}

/// Resolves a snooze time, mirroring [TimeSpec::resolve].
///
/// # Errors
///
/// Returns the same errors as [TimeSpec::resolve].
pub fn resolve_snooze(spec: Option<&TimeSpec>) -> Result<Option<Resolved>, TimeError> {
    resolve_optional(spec)
}

/// Computes the half-open [start, end) bounds of the civil day in "tz" that
/// contains "now_utc_ms".
///
/// The day runs from local midnight to the next local midnight, so it is 23
/// hours long across a spring-forward transition and 25 hours long across an
/// autumn fold. Midnight itself is resolved with the gap/fold policy of
/// [TimeSpec::resolve].
///
/// # Errors
///
/// Returns [TimeError::InvalidZone] for an unknown zone,
/// [TimeError::InstantOutOfRange] when "now_utc_ms" is not a valid jiff
/// instant, or [TimeError::Resolve]/[TimeError::NextMidnight] when a midnight
/// cannot be represented.
pub fn today_bounds(now_utc_ms: i64, tz: &str) -> Result<(i64, i64), TimeError> {
    let zone = TimeZone::get(tz).map_err(|source| TimeError::InvalidZone {
        tz: tz.to_owned(),
        source,
    })?;
    let now =
        Timestamp::from_millisecond(now_utc_ms).map_err(|source| TimeError::InstantOutOfRange {
            utc_ms: now_utc_ms,
            source,
        })?;

    let today = zone.to_datetime(now).date();
    let start_local = today.at(0, 0, 0, 0);
    let tomorrow = today.tomorrow().map_err(|source| TimeError::NextMidnight {
        date: today.to_string(),
        tz: tz.to_owned(),
        source,
    })?;
    let end_local = tomorrow.at(0, 0, 0, 0);

    let start = resolve_local(&zone, start_local, tz)?.utc_ms;
    let end = resolve_local(&zone, end_local, tz)?.utc_ms;
    Ok((start, end))
}

/// Returns the system's IANA time zone id for the board default, falling back
/// to UTC when the platform cannot name a zone.
pub fn system_zone() -> String {
    TimeZone::try_system()
        .ok()
        .and_then(|zone| zone.iana_name().map(str::to_owned))
        .unwrap_or_else(|| "UTC".to_owned())
}

/// Checks that "tz" is a time zone id known to the IANA database.
///
/// # Errors
///
/// Returns [TimeError::InvalidZone] when the zone is unknown.
pub fn validate_zone(tz: &str) -> Result<(), TimeError> {
    TimeZone::get(tz)
        .map(|_| ())
        .map_err(|source| TimeError::InvalidZone {
            tz: tz.to_owned(),
            source,
        })
}

/// Parses YYYY-MM-DDTHH:MM:SS exactly, rejecting alternate spellings that
/// jiff would otherwise accept (a space separator, missing seconds, leap
/// seconds and fractional seconds).
fn parse_local(local: &str) -> Result<DateTime, TimeError> {
    if local.len() != LOCAL_LEN {
        return Err(TimeError::InvalidLocalFormat {
            local: local.to_owned(),
        });
    }
    let parsed: DateTime = local.parse().map_err(|source| TimeError::InvalidLocal {
        local: local.to_owned(),
        source,
    })?;
    if format!("{parsed:.0}") != local {
        return Err(TimeError::InvalidLocalFormat {
            local: local.to_owned(),
        });
    }
    Ok(parsed)
}

/// Resolves one civil datetime in an already-validated zone.
fn resolve_local(zone: &TimeZone, local: DateTime, tz: &str) -> Result<Resolved, TimeError> {
    let ambiguous = zone.to_ambiguous_timestamp(local);
    match ambiguous.offset() {
        AmbiguousOffset::Unambiguous { .. } => {
            let timestamp = ambiguous
                .compatible()
                .map_err(|source| resolve_error(local, tz, source))?;
            Ok(Resolved {
                utc_ms: timestamp.as_millisecond(),
                gap: false,
                ambiguous: false,
            })
        }
        AmbiguousOffset::Fold { .. } => {
            // "earlier" selects the pre-transition offset, giving the first
            // of the two repeated wall-clock instants.
            let timestamp = ambiguous
                .earlier()
                .map_err(|source| resolve_error(local, tz, source))?;
            Ok(Resolved {
                utc_ms: timestamp.as_millisecond(),
                gap: false,
                ambiguous: true,
            })
        }
        AmbiguousOffset::Gap { .. } => {
            // "earlier" applies the post-transition offset, which maps the
            // non-existent wall time to an instant shortly *before* the
            // transition. The transition itself is the first instant whose
            // wall clock is valid again.
            let before_gap = ambiguous
                .earlier()
                .map_err(|source| resolve_error(local, tz, source))?;
            let transition =
                zone.following(before_gap)
                    .next()
                    .ok_or_else(|| TimeError::MissingTransition {
                        local: format!("{local:.0}"),
                        tz: tz.to_owned(),
                    })?;
            Ok(Resolved {
                utc_ms: transition.timestamp().as_millisecond(),
                gap: true,
                ambiguous: false,
            })
        }
    }
}

/// Builds a [TimeError::Resolve] for a jiff conversion failure.
fn resolve_error(local: DateTime, tz: &str, source: jiff::Error) -> TimeError {
    TimeError::Resolve {
        local: format!("{local:.0}"),
        tz: tz.to_owned(),
        source,
    }
}

/// Shared body for the optional due/remind/snooze helpers.
fn resolve_optional(spec: Option<&TimeSpec>) -> Result<Option<Resolved>, TimeError> {
    spec.map(TimeSpec::resolve).transpose()
}

#[cfg(test)]
mod tests {
    use super::*;

    const LONDON: &str = "Europe/London";

    fn spec(local: &str, tz: &str) -> TimeSpec {
        TimeSpec {
            local: local.to_owned(),
            tz: tz.to_owned(),
        }
    }

    fn resolved(local: &str) -> Resolved {
        spec(local, LONDON).resolve().expect("fixture resolves")
    }

    #[test]
    fn spring_forward_gap_uses_the_first_valid_instant() {
        // 01:30 GMT does not exist on 2026-03-29. The first valid wall clock
        // after the gap is 02:00 BST, whose exact instant is 01:00:00Z.
        let got = resolved("2026-03-29T01:30:00");
        assert_eq!(
            got,
            Resolved {
                utc_ms: 1_774_746_000_000,
                gap: true,
                ambiguous: false,
            }
        );
        assert_eq!(
            Timestamp::from_millisecond(got.utc_ms)
                .expect("fixture instant")
                .to_string(),
            "2026-03-29T01:00:00Z"
        );
    }

    #[test]
    fn spring_forward_gap_at_its_first_second_uses_the_transition() {
        // The transition instant is itself the answer at the gap boundary.
        assert_eq!(
            resolved("2026-03-29T01:00:00"),
            Resolved {
                utc_ms: 1_774_746_000_000,
                gap: true,
                ambiguous: false,
            }
        );
        // 02:00 is the first valid wall time and is not ambiguous.
        assert_eq!(
            resolved("2026-03-29T02:00:00"),
            Resolved {
                utc_ms: 1_774_746_000_000,
                gap: false,
                ambiguous: false,
            }
        );
    }

    #[test]
    fn autumn_fold_uses_the_earlier_occurrence() {
        // 01:30 occurs twice on 2026-10-25. The earlier occurrence is still
        // BST, so the exact instant is 00:30:00Z.
        let got = resolved("2026-10-25T01:30:00");
        assert_eq!(
            got,
            Resolved {
                utc_ms: 1_792_888_200_000,
                gap: false,
                ambiguous: true,
            }
        );
        assert_eq!(
            Timestamp::from_millisecond(got.utc_ms)
                .expect("fixture instant")
                .to_string(),
            "2026-10-25T00:30:00Z"
        );
        // The second (GMT) occurrence would be 01:30:00Z.
        assert_eq!(got.utc_ms + 3_600_000, 1_792_891_800_000);
    }

    #[test]
    fn normal_summer_time_resolves_exactly() {
        // 12:00 BST is 11:00Z.
        assert_eq!(
            resolved("2026-07-15T12:00:00"),
            Resolved {
                utc_ms: 1_784_113_200_000,
                gap: false,
                ambiguous: false,
            }
        );
    }

    #[test]
    fn normal_winter_time_resolves_exactly() {
        // 12:00 GMT is 12:00Z.
        assert_eq!(
            resolved("2026-01-15T12:00:00"),
            Resolved {
                utc_ms: 1_768_478_400_000,
                gap: false,
                ambiguous: false,
            }
        );
    }

    #[test]
    fn invalid_zone_is_a_typed_error() {
        let error = spec("2026-03-29T12:00:00", "Mars/Olympus_Mons")
            .resolve()
            .expect_err("unknown zone must fail");
        assert!(matches!(error, TimeError::InvalidZone { .. }));
        assert!(matches!(
            validate_zone("not a zone"),
            Err(TimeError::InvalidZone { .. })
        ));
        assert!(validate_zone(LONDON).is_ok());
        assert!(validate_zone("UTC").is_ok());
    }

    #[test]
    fn malformed_local_is_a_typed_error() {
        for local in [
            "2026-03-29 01:30:00",   // space instead of T
            "2026-03-29T01:30",      // missing seconds
            "2026-03-29T01:30:00Z",  // offset must not be stored
            "2026-03-29T01:30:00.5", // fractional seconds
            "2026-02-30T00:00:00",   // impossible date
            "not a time",
        ] {
            let error = spec(local, LONDON)
                .resolve()
                .expect_err("malformed local must fail");
            assert!(
                matches!(
                    error,
                    TimeError::InvalidLocal { .. } | TimeError::InvalidLocalFormat { .. }
                ),
                "unexpected error for {local:?}: {error:?}"
            );
        }
    }

    #[test]
    fn today_bounds_across_spring_forward_are_23_hours() {
        // The day starts at midnight GMT (00:00Z) and ends at midnight BST
        // the next day (23:00Z), so it lasts 23 hours.
        let (start, end) =
            today_bounds(1_774_785_600_000, LONDON).expect("2026-03-29 noon resolves");
        assert_eq!(start, 1_774_742_400_000);
        assert_eq!(end, 1_774_825_200_000);
        assert_eq!(end - start, 23 * 60 * 60 * 1000);
    }

    #[test]
    fn today_bounds_across_autumn_fold_are_25_hours() {
        // The day starts at midnight BST (23:00Z the previous day) and ends
        // at midnight GMT the next day (00:00Z), so it lasts 25 hours.
        let (start, end) =
            today_bounds(1_792_929_600_000, LONDON).expect("2026-10-25 noon resolves");
        assert_eq!(start, 1_792_882_800_000);
        assert_eq!(end, 1_792_972_800_000);
        assert_eq!(end - start, 25 * 60 * 60 * 1000);
    }

    #[test]
    fn today_bounds_respects_the_zone_boundary() {
        // 23:30Z on 2026-03-29 is already 00:30 BST on 2026-03-30.
        let (start, end) = today_bounds(1_774_827_000_000, LONDON).expect("late evening resolves");
        assert_eq!(start, 1_774_825_200_000, "start is 2026-03-30 00:00 BST");
        assert_eq!(end, 1_774_911_600_000, "end is 2026-03-31 00:00 BST");
    }

    #[test]
    fn today_bounds_rejects_bad_input() {
        assert!(matches!(
            today_bounds(0, "Mars/Olympus_Mons"),
            Err(TimeError::InvalidZone { .. })
        ));
        assert!(matches!(
            today_bounds(i64::MAX, LONDON),
            Err(TimeError::InstantOutOfRange { .. })
        ));
    }

    #[test]
    fn sql_parts_round_trip_local_and_zone() {
        let original = spec("2026-03-29T01:30:00", LONDON);
        let parts = original.to_sql_parts().expect("parts");
        assert_eq!(
            parts,
            TimeParts {
                local: "2026-03-29T01:30:00".to_owned(),
                tz: LONDON.to_owned(),
                utc_ms: 1_774_746_000_000,
            }
        );

        let rebuilt = spec(&parts.local, &parts.tz);
        assert_eq!(
            rebuilt.resolve().expect("rebuilt parts resolve"),
            original.resolve().expect("original resolves")
        );
    }

    #[test]
    fn sql_parts_keep_the_fold_policy_on_round_trip() {
        let parts = spec("2026-10-25T01:30:00", LONDON)
            .to_sql_parts()
            .expect("parts");
        assert_eq!(parts.local, "2026-10-25T01:30:00");
        assert_eq!(parts.tz, LONDON);
        assert_eq!(parts.utc_ms, 1_792_888_200_000);
        assert_eq!(
            spec(&parts.local, &parts.tz)
                .resolve()
                .expect("rebuilt fold")
                .utc_ms,
            parts.utc_ms
        );
    }

    #[test]
    fn time_spec_serde_round_trip() {
        let original = spec("2026-10-25T01:30:00", LONDON);
        let json = serde_json::to_string(&original).expect("serialize");
        assert_eq!(
            json,
            r#"{"local":"2026-10-25T01:30:00","tz":"Europe/London"}"#
        );
        let decoded: TimeSpec = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(decoded, original);
    }

    #[test]
    fn optional_helpers_pass_through_none_and_resolve_some() {
        assert!(resolve_due(None).expect("none").is_none());
        assert!(resolve_remind(None).expect("none").is_none());
        assert!(resolve_snooze(None).expect("none").is_none());

        let due = spec("2026-07-15T12:00:00", LONDON);
        let expected = due.resolve().expect("fixture");
        assert_eq!(resolve_due(Some(&due)).expect("due"), Some(expected));
        assert_eq!(resolve_remind(Some(&due)).expect("remind"), Some(expected));
        assert_eq!(resolve_snooze(Some(&due)).expect("snooze"), Some(expected));

        let bad = spec("2026-07-15T12:00:00", "Mars/Olympus_Mons");
        assert!(matches!(
            resolve_due(Some(&bad)),
            Err(TimeError::InvalidZone { .. })
        ));
    }

    #[test]
    fn system_zone_is_usable_as_a_board_default() {
        let zone = system_zone();
        assert!(validate_zone(&zone).is_ok(), "system zone {zone:?}");
    }
}
