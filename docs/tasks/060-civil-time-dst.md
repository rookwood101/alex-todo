# 060 — Civil time and DST

**Goal:** store deadlines as wall-clock time plus an IANA zone, and resolve them to instants at
runtime with explicit, tested behaviour across GMT/BST transitions.

**Depends on:** 030 (`jiff` is added by 020). **Parallel with:** 070, 080.
**Owns:** `src/time.rs`, `src/time` module declaration. Do not edit `Cargo.toml`.

**Read first:** this index (data conventions); `030`'s `*_local`/`*_tz`/`*_utc_ms` columns.

## Steps

1. `TimeSpec { local: String, tz: String }` (serde). `local` is `YYYY-MM-DDTHH:MM:SS`; `tz` is
   an IANA id such as `Europe/London`. Parse/format with `jiff::civil::DateTime` and
   `jiff::tz::TimeZone::get`.
2. `resolve(&self) -> Result<Resolved, TimeError>` with `Resolved { utc_ms, gap, ambiguous }`:
   - **Spring-forward gap:** the requested local time does not exist; use the **first valid
     instant after the gap** (e.g. London 2026-03-29 01:30 → 02:00 BST) and set `gap = true`.
   - **Autumn fold:** the local time occurs twice; use the **earlier** occurrence and set
     `ambiguous = true`.
   - Normal times resolve exactly. Invalid zone or malformed local returns a typed error.
3. `today_bounds(now_utc_ms, tz) -> (start_utc_ms, end_utc_ms)` using local midnight to next
   local midnight; day length may be 23/25 hours. `system_zone() -> String` for the board
   default. `validate_zone`.
4. Helpers used by 040/140: `resolve_due`, `resolve_remind`, `resolve_snooze`, and a
   `to_sql_parts` that produces the three columns together. Never write `*_utc_ms` without the
   `*_local`/`*_tz` pair.
5. Tests (fixed input, no wall clock): London 2026-03-29 01:30 (gap → first valid), 2026-10-25
   01:30 (fold → earlier), normal summer/winter, invalid zone, malformed local, today across a
   DST change (23 h and 25 h days), round-trip local/zone.

## Acceptance

- `cargo test`/`clippy`/`fmt` clean.
- The two transition tests assert the exact UTC instants and flags.
- No code uses the system clock directly; callers pass `now`.

## Out of scope

UI formatting/chips (080/090), reminder firing (140), recurring tasks, timezone conversion for
travelling users beyond the documented "task keeps its zone" rule.
