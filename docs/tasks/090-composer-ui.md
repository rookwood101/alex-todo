# 090 — Capture/Agent composer

**Goal:** one bottom-centre input with two modes. Capture creates a note instantly on Enter;
Agent sends a prompt on Enter. Tab switches; chips preview what Capture will set.

**Depends on:** 040 (`Command`), 080 (`parse_capture`). **Parallel with:** 110.
**Owns:** `src/ui/mod.rs`, `src/ui/composer.rs`; may add one `ui::composer::show(...)` call in
`src/app.rs` (the only app change; 170 consolidates).

**Read first:** this index; `src/app.rs` panel structure; `src/capture.rs`; the existing
`TextEdit` usage in `src/canvas.rs`.

## Steps

1. `ComposerState { mode: Capture | Agent, text: String, focus_requested: bool }` and
   `ComposerOutput { capture: Option<Command>, agent_prompt: Option<String> }`.
2. Draw a floating panel anchored bottom-centre over the canvas (width ~720, grows to 6 lines).
   Show a mode chip (`Capture` / `Agent`) and a one-line hint; 170 removes the old hint bar, so
   do not depend on it.
3. Input handling, in this order:
   - If the editor has focus and `Tab` is pressed: consume it, toggle mode, keep focus.
   - If focused and `Enter` without Shift: consume it, submit, keep focus.
   - `Shift+Enter`: let egui insert a newline.
   - `Esc`: leave editing; do not hide the window here (160 owns hiding).
4. Capture mode: run `parse_capture` on every change; render chips above the input for group,
   priority and due; on Enter emit `Command::AddNote` (text cleaned, group proposed, due
   `TimeSpec`), clear, stay focused. Never block on the model.
5. Agent mode: on Enter emit `agent_prompt` with the raw text (do not parse dates); clear.
   Show a disabled "streaming…" state when a `busy` flag is set by 130.
6. `focus_requested` sets editor focus next frame; expose `request_focus()` for the global
   hotkey (160) and startup.
7. Headless tests: Tab toggles and does not insert a tab; Enter in Capture emits one AddNote
   with parsed chips and clears; Shift+Enter does not submit; Enter in Agent emits a prompt;
   focus request focuses the editor; empty input does not submit.

## Acceptance

- `cargo test`/`clippy`/`fmt` clean.
- No model/network calls; the composer only emits values.
- Headless tests use the existing egui harness in `src/headless_tests.rs` (extend it).

## Out of scope

Applying commands to SQLite (170), chat transcript (130), global hotkey (160), chips' final
visual design. Keep the widget independent of `RustboardApp` internals.
