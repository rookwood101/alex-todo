# 080 — Capture parser

> **Read [000-index](000-index.md) first.** It carries the decisions, write-set
> ownership, dependency graph, and gates this task assumes.

**Goal:** turn one typed line into a note plus optional group, priority and deadline chips. Fast,
local, deterministic — the model is never on this path.

**Depends on:** 060 (`TimeSpec`). **Parallel with:** 040, 050, 070.
**Owns:** `src/capture.rs`, `src/capture` module declaration. Do not edit UI files.

**Read first:** `src/time.rs`; the existing note text conventions in `src/board.rs`.

## Steps

1. `parse_capture(input, now_utc_ms, tz, known_groups) -> ParsedCapture`:
   - `#name` → group name (last wins; unknown names are proposed, not created).
   - `!high` / `!med` / `!low` (also `!1..3`) → priority.
   - Trailing date phrase → `TimeSpec`: `today`, `tomorrow`, `tonight`, `mon..sun`,
     `next <weekday>`, `in N days`, `in N weeks`, `YYYY-MM-DD`, `D/M`, optionally `at HH:MM`,
     `HH:MM`, `4pm`, `noon`, `midnight`.
   - Date-only resolves to 09:00 local; a time without a date attaches to today (or tomorrow if
     already past). Everything else is left in the text untouched.
2. Return `ParsedCapture { text, group, priority, due: Option<TimeSpec>, consumed: Vec<Span> }`
   so the composer can highlight chips. Clean whitespace and do not drop punctuation.
3. Conservative rules: parse only tokens at the end or explicitly prefixed; "the 4pm thing" is
   text, not a deadline; `#` inside a word is not a group; ambiguous dates are left alone.
4. Tests: table-driven cases for each token; weekday rollover; `next` semantics; invalid dates;
   multiple tokens; text with `#`/`!`/digits; date-only default; time-only past → tomorrow;
   empty input yields empty text.

## Acceptance

- `cargo test`/`clippy`/`fmt` clean.
- Parser is pure, takes `now` and `tz`, and never calls the network/model.
- Chips' spans point at the exact consumed substrings.

## Out of scope

Creating notes (040), chips rendering (090), reminders, natural-language parsing beyond the
listed forms. The agent handles the ambiguous leftovers later.
