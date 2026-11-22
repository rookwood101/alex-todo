//! Bottom-centre capture/agent composer (task 090).
//!
//! One input, two modes. Capture parses the line with [crate::capture] as it is
//! typed, previews the chips that Enter will apply, and emits a
//! [Command::AddNote]. Agent mode emits the raw prompt for the chat layer.
//!
//! The widget is deliberately independent of [crate::app::RustboardApp]: the
//! caller supplies the board zone, the current instant and the known group
//! names, and receives [ComposerOutput] values. Nothing here reads the clock,
//! touches SQLite, or calls a model.

use eframe::egui::{
    self, Align2, Event, EventFilter, FontId, Id, Key, Margin, Modifiers, RichText, Stroke,
    TextStyle, vec2,
};
use jiff::Timestamp;
use jiff::tz::TimeZone;

use crate::capture::{self, ParsedCapture};
use crate::command::Command;
use crate::model::Priority;
use crate::theme;
use crate::time::TimeSpec;

/// Width of the floating composer panel in points.
const PANEL_WIDTH: f32 = 720.0;

/// The editor grows from one to this many lines, then scrolls.
const MAX_LINES: usize = 6;

/// Distance between the panel's bottom edge and the bottom of the screen.
const BOTTOM_OFFSET: f32 = 18.0;

/// Font size used by the composer editor.
const TEXT_SIZE: f32 = 16.0;

/// Which input mode the composer is in. Tab toggles them.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ComposerMode {
    /// Plain text becomes a note. Hash groups, bang priorities and a trailing
    /// date are parsed into note fields.
    #[default]
    Capture,
    /// Plain text becomes a prompt for the agent; no date parsing.
    Agent,
}

impl ComposerMode {
    /// Short label shown in the mode chip.
    pub fn label(self) -> &'static str {
        match self {
            Self::Capture => "Capture",
            Self::Agent => "Agent",
        }
    }

    /// The mode Tab switches to.
    pub fn toggled(self) -> Self {
        match self {
            Self::Capture => Self::Agent,
            Self::Agent => Self::Capture,
        }
    }

    /// One-line usage hint for this mode.
    pub fn hint(self) -> &'static str {
        match self {
            Self::Capture => "Enter captures  ·  Tab for Agent  ·  #group !high tomorrow",
            Self::Agent => "Enter sends  ·  Shift+Enter adds a line  ·  Tab for Capture",
        }
    }
}

/// Everything the caller must supply to render one composer frame.
///
/// The values come from the app's board state; the composer never reads the
/// system clock or the database itself.
#[derive(Clone, Copy, Debug)]
pub struct ComposerContext<'a> {
    /// Current instant as epoch milliseconds UTC.
    pub now_utc_ms: i64,
    /// The board's IANA time zone, used for today/tomorrow and chip labels.
    pub tz: &'a str,
    /// Active group names, used to distinguish known groups from proposals.
    pub known_groups: &'a [String],
}

impl<'a> ComposerContext<'a> {
    /// Builds a context for one frame.
    pub fn new(now_utc_ms: i64, tz: &'a str, known_groups: &'a [String]) -> Self {
        Self {
            now_utc_ms,
            tz,
            known_groups,
        }
    }
}

/// What one interaction with the composer produced.
///
/// At most one field is set per frame; the caller applies it and reloads.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ComposerOutput {
    /// Set when Enter was pressed in Capture mode and the line was non-empty.
    pub capture: Option<Command>,
    /// Set when Enter was pressed in Agent mode and the prompt was non-empty.
    pub agent_prompt: Option<String>,
}

/// Persistent state of the composer widget.
///
/// The app owns one of these. request_focus is the hotkey/startup entry point;
/// set_busy is how the chat layer disables submitting while a turn is
/// streaming.
#[derive(Clone, Debug, Default)]
pub struct ComposerState {
    mode: ComposerMode,
    text: String,
    focus_requested: bool,
    busy: bool,
}

impl ComposerState {
    /// A fresh composer in Capture mode with no text.
    pub fn new() -> Self {
        Self::default()
    }

    /// The current mode.
    pub fn mode(&self) -> ComposerMode {
        self.mode
    }

    /// Switches modes without touching the text.
    pub fn set_mode(&mut self, mode: ComposerMode) {
        self.mode = mode;
    }

    /// Toggles Capture/Agent.
    pub fn toggle_mode(&mut self) {
        self.mode = self.mode.toggled();
    }

    /// The current editor text.
    pub fn text(&self) -> &str {
        &self.text
    }

    /// Replaces the editor text, for restore and tests.
    pub fn set_text(&mut self, text: impl Into<String>) {
        self.text = text.into();
    }

    /// Asks the next rendered frame to focus the editor.
    pub fn request_focus(&mut self) {
        self.focus_requested = true;
    }

    /// True when a focus request is waiting for its next frame.
    pub fn focus_requested(&self) -> bool {
        self.focus_requested
    }

    /// Marks the composer as busy (streaming). Submitting is disabled while set.
    pub fn set_busy(&mut self, busy: bool) {
        self.busy = busy;
    }

    /// Whether the composer was marked busy with [Self::set_busy].
    pub fn is_busy(&self) -> bool {
        self.busy
    }

    /// Whether the editor currently has keyboard focus.
    pub fn has_focus(&self, ctx: &egui::Context) -> bool {
        ctx.memory(|memory| memory.has_focus(editor_id()))
    }
}

/// The kind of preview chip rendered above the input.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ComposerChipKind {
    /// A hash group token, known or proposed.
    Group,
    /// A bang priority token.
    Priority,
    /// A parsed deadline.
    Due,
}

/// One chip previewing what Capture mode will set on Enter.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ComposerChip {
    /// Which field the chip belongs to.
    pub kind: ComposerChipKind,
    /// Short human label, for example "#home", "!high", "due 2026-11-23 09:00".
    pub label: String,
    /// True when this is a group name that does not exist yet.
    pub proposed: bool,
}

/// Parses text for the preview and submission, returning the parsed line and
/// the chips that describe it.
///
/// This is pure: the same context and text always produce the same chips. It
/// is public so the canvas and tests can reuse the exact preview the composer
/// renders.
pub fn capture_preview(
    context: &ComposerContext<'_>,
    text: &str,
) -> (ParsedCapture, Vec<ComposerChip>) {
    let parsed = capture::parse_capture(text, context.now_utc_ms, context.tz, context.known_groups);
    let chips = chips_for(&parsed, context);
    (parsed, chips)
}

/// Draws the floating composer and returns anything Enter produced.
///
/// busy is normally [ComposerState::is_busy]; passing either the argument or
/// the stored flag disables submitting. The caller passes now_utc_ms, tz and
/// known_groups through [ComposerContext] so this function stays free of clock
/// and database access.
pub fn show(
    state: &mut ComposerState,
    ctx: &egui::Context,
    busy: bool,
    context: &ComposerContext<'_>,
) -> ComposerOutput {
    let busy = busy || state.busy;
    let mut output = ComposerOutput::default();
    let mut refocus = false;

    // Handle keys before the TextEdit is added. Consuming them here means the
    // editor never sees Tab, plain Enter or Esc as text-editing events.
    if state.has_focus(ctx) {
        let mut toggle = false;
        let mut submit = false;
        let mut escape = false;
        ctx.input_mut(|input| {
            toggle = input.consume_key(Modifiers::NONE, Key::Tab);
            submit = consume_plain_enter(input);
        });
        ctx.input_mut(|input| {
            escape = input.consume_key(Modifiers::NONE, Key::Escape);
        });

        if toggle {
            state.toggle_mode();
            refocus = true;
        }
        if submit && !busy {
            submit_line(state, context, &mut output);
            refocus = true;
        } else if submit {
            // Busy: consume Enter so it cannot become a newline in a disabled
            // editor, but emit nothing.
            refocus = true;
        }
        if escape {
            refocus = false;
            state.focus_requested = false;
            ctx.memory_mut(|memory| memory.surrender_focus(editor_id()));
        }
    }

    if state.focus_requested {
        state.focus_requested = false;
        refocus = true;
    }

    let area = egui::Area::new(panel_id())
        .anchor(Align2::CENTER_BOTTOM, vec2(0.0, -BOTTOM_OFFSET))
        .order(egui::Order::Foreground)
        .fade_in(false)
        .constrain(true)
        .default_width(PANEL_WIDTH);

    area.show(ctx, |ui| {
        ui.set_width(PANEL_WIDTH);

        egui::Frame::new()
            .fill(theme::TOOLBAR_BG)
            .stroke(Stroke::new(1.0, theme::BORDER))
            .corner_radius(12)
            .inner_margin(Margin::symmetric(12, 10))
            .show(ui, |ui| {
                ui.set_width(PANEL_WIDTH - 24.0);

                ui.horizontal(|ui| {
                    let mode = state.mode.label();
                    let chip = egui::Button::new(
                        RichText::new(mode)
                            .size(11.0)
                            .strong()
                            .color(theme::TEXT_PRIMARY),
                    )
                    .fill(theme::ACCENT_SOFT)
                    .corner_radius(6)
                    .min_size(vec2(68.0, 24.0));
                    if ui
                        .add(chip)
                        .on_hover_text("Tab switches between Capture and Agent")
                        .clicked()
                    {
                        state.toggle_mode();
                        state.focus_requested = true;
                    }

                    ui.add_space(8.0);
                    if busy {
                        ui.add(egui::Spinner::new().size(12.0));
                        ui.label(
                            RichText::new("streaming…")
                                .size(11.0)
                                .color(theme::TEXT_MUTED),
                        );
                    } else {
                        ui.label(
                            RichText::new(state.mode.hint())
                                .size(11.0)
                                .color(theme::TEXT_MUTED),
                        );
                    }
                });

                let (_, chips) = if state.mode == ComposerMode::Capture {
                    capture_preview(context, &state.text)
                } else {
                    (ParsedCapture::default(), Vec::new())
                };
                if !chips.is_empty() {
                    ui.add_space(6.0);
                    ui.horizontal_wrapped(|ui| {
                        for chip in &chips {
                            chip_badge(ui, chip);
                        }
                    });
                }

                ui.add_space(6.0);
                let mut line_count = state.text.lines().count();
                if state.text.ends_with('\n') {
                    line_count += 1;
                }
                let lines = line_count.clamp(1, MAX_LINES);
                let row_height = ui.text_style_height(&TextStyle::Body);
                let hint = match state.mode {
                    ComposerMode::Capture => "Type a note and press Enter…",
                    ComposerMode::Agent => "Ask the agent and press Enter…",
                };

                let filter = EventFilter {
                    tab: true,
                    horizontal_arrows: true,
                    vertical_arrows: true,
                    escape: true,
                };
                let editor = egui::TextEdit::multiline(&mut state.text)
                    .id(editor_id())
                    .desired_width(f32::INFINITY)
                    .desired_rows(lines)
                    .font(FontId::proportional(TEXT_SIZE))
                    .text_color(theme::TEXT_PRIMARY)
                    .margin(Margin::symmetric(8, 8))
                    .hint_text(RichText::new(hint).color(theme::TEXT_MUTED))
                    .interactive(!busy)
                    .event_filter(filter);

                let scroll = egui::ScrollArea::vertical()
                    .id_salt("rustboard.composer.scroll")
                    .max_height(row_height * MAX_LINES as f32 + 16.0)
                    .auto_shrink([false, true]);
                scroll.show(ui, |ui| {
                    editor.show(ui);
                });
            });
    });

    // The TextEdit has been registered for this frame; focusing it now takes
    // effect for the next frame and survives the end-of-pass focus check.
    if refocus || state.focus_requested {
        state.focus_requested = false;
        ctx.memory_mut(|memory| memory.request_focus(editor_id()));
    }

    output
}

/// Consumes Enter only when Shift/Alt/Ctrl are not held, leaving Shift+Enter
/// for the TextEdit so it inserts a newline.
fn consume_plain_enter(input: &mut egui::InputState) -> bool {
    let mut consumed = false;
    input.events.retain(|event| {
        let Event::Key {
            key,
            pressed: true,
            modifiers,
            ..
        } = event
        else {
            return true;
        };
        if *key != Key::Enter || modifiers.any() {
            return true;
        }
        consumed = true;
        false
    });
    consumed
}

/// Turns the parsed line into the command or prompt Enter should emit.
fn submit_line(
    state: &mut ComposerState,
    context: &ComposerContext<'_>,
    output: &mut ComposerOutput,
) {
    match state.mode {
        ComposerMode::Capture => {
            let parsed = capture::parse_capture(
                &state.text,
                context.now_utc_ms,
                context.tz,
                context.known_groups,
            );
            let has_chips =
                parsed.group.is_some() || parsed.priority.is_some() || parsed.due.is_some();
            if parsed.text.trim().is_empty() && !has_chips {
                return;
            }
            output.capture = Some(Command::AddNote {
                text: parsed.text,
                group: parsed.group.map(|group| group.name),
                parent: None,
                due: parsed.due,
                remind: None,
                priority: parsed.priority.map(Priority::from),
                source: "capture".to_owned(),
            });
            state.text.clear();
        }
        ComposerMode::Agent => {
            if state.text.trim().is_empty() {
                return;
            }
            // Agent prompts are raw: no date, group or priority parsing.
            output.agent_prompt = Some(state.text.clone());
            state.text.clear();
        }
    }
}

/// Builds the preview chips for one parsed line.
fn chips_for(parsed: &ParsedCapture, context: &ComposerContext<'_>) -> Vec<ComposerChip> {
    let mut chips = Vec::new();
    if let Some(group) = &parsed.group {
        chips.push(ComposerChip {
            kind: ComposerChipKind::Group,
            label: format!("#{}", group.name),
            proposed: !group.known,
        });
    }
    if let Some(priority) = parsed.priority {
        chips.push(ComposerChip {
            kind: ComposerChipKind::Priority,
            label: format!("!{}", priority.label()),
            proposed: false,
        });
    }
    if let Some(due) = &parsed.due {
        chips.push(ComposerChip {
            kind: ComposerChipKind::Due,
            label: format!("due {}", due_label(due, context)),
            proposed: false,
        });
    }
    chips
}

/// Formats a due time in the board zone, adding a note when resolution reports
/// a DST gap or fold.
fn due_label(due: &TimeSpec, context: &ComposerContext<'_>) -> String {
    let resolved = due.resolve().ok();
    let mut label = match resolved {
        Some(resolved) => {
            format_in_zone(resolved.utc_ms, context.tz).unwrap_or_else(|| civil_label(&due.local))
        }
        None => civil_label(&due.local),
    };
    if let Some(resolved) = resolved {
        if resolved.gap {
            label.push_str(" (DST gap)");
        }
        if resolved.ambiguous {
            label.push_str(" (DST fold)");
        }
    }
    label
}

/// Formats an instant as YYYY-MM-DD HH:MM in the supplied IANA zone.
fn format_in_zone(utc_ms: i64, tz: &str) -> Option<String> {
    let zone = TimeZone::get(tz).ok()?;
    let timestamp = Timestamp::from_millisecond(utc_ms).ok()?;
    let local = zone.to_datetime(timestamp);
    Some(format!(
        "{:04}-{:02}-{:02} {:02}:{:02}",
        local.year(),
        local.month(),
        local.day(),
        local.hour(),
        local.minute()
    ))
}

/// Fallback display for a civil time that cannot be resolved.
fn civil_label(local: &str) -> String {
    let mut label = local.replacen('T', " ", 1);
    if label.len() > 16 {
        label.truncate(16);
    }
    label
}

/// Draws one preview chip.
fn chip_badge(ui: &mut egui::Ui, chip: &ComposerChip) {
    let (fill, color) = match chip.kind {
        ComposerChipKind::Group if chip.proposed => (theme::BUTTON_BG, theme::SAVE_WARN),
        ComposerChipKind::Group => (theme::ACCENT_SOFT, theme::TEXT_PRIMARY),
        ComposerChipKind::Priority => (theme::BUTTON_BG, theme::SAVE_ERROR),
        ComposerChipKind::Due => (theme::BUTTON_BG, theme::SAVE_WARN),
    };
    let label = if chip.proposed {
        format!("{} · new", chip.label)
    } else {
        chip.label.clone()
    };
    let frame = egui::Frame::new()
        .fill(fill)
        .corner_radius(6)
        .inner_margin(Margin::symmetric(7, 2))
        .show(ui, |ui| {
            ui.label(RichText::new(label).size(11.0).color(color));
        });
    if chip.proposed {
        frame
            .response
            .on_hover_text("This group does not exist yet");
    }
}

/// Stable id for the floating panel.
fn panel_id() -> Id {
    Id::unique("rustboard.composer")
}

/// Stable id for the editor; shared by focus requests and input handling.
fn editor_id() -> Id {
    Id::unique("rustboard.composer.editor")
}

#[cfg(test)]
mod tests {
    use eframe::egui::{Context, Event, Key, Modifiers, Pos2, RawInput, Rect, vec2};

    use super::*;

    const TZ: &str = "Europe/London";
    const FRAME_SECONDS: f64 = 1.0 / 60.0;

    fn london_now(local: &str) -> i64 {
        TimeSpec {
            local: local.to_owned(),
            tz: TZ.to_owned(),
        }
        .resolve()
        .expect("fixture instant resolves")
        .utc_ms
    }

    fn key_event(key: Key, modifiers: Modifiers) -> Event {
        Event::Key {
            key,
            physical_key: Some(key),
            pressed: true,
            repeat: false,
            modifiers,
        }
    }

    struct Harness {
        ctx: Context,
        state: ComposerState,
        known_groups: Vec<String>,
        now_utc_ms: i64,
        busy: bool,
        time: f64,
    }

    impl Harness {
        fn new() -> Self {
            let ctx = Context::default();
            crate::theme::apply(&ctx);
            let mut harness = Self {
                ctx,
                state: ComposerState::new(),
                known_groups: Vec::new(),
                // Sunday 2026-11-22 08:00 GMT.
                now_utc_ms: london_now("2026-11-22T08:00:00"),
                busy: false,
                time: 0.0,
            };
            harness.run(Vec::new());
            harness.run(Vec::new());
            harness
        }

        fn with_groups(groups: &[&str]) -> Self {
            let mut harness = Self::new();
            harness.known_groups = groups.iter().map(|name| (*name).to_owned()).collect();
            harness
        }

        fn focus_editor(&mut self) {
            self.state.request_focus();
            self.run(Vec::new());
            assert!(self.state.has_focus(&self.ctx), "editor should be focused");
        }

        fn run(&mut self, events: Vec<Event>) -> ComposerOutput {
            self.time += FRAME_SECONDS;
            let raw = RawInput {
                screen_rect: Some(Rect::from_min_size(Pos2::ZERO, vec2(1280.0, 800.0))),
                time: Some(self.time),
                focused: true,
                events,
                ..Default::default()
            };
            let ctx = self.ctx.clone();
            let context = ComposerContext {
                now_utc_ms: self.now_utc_ms,
                tz: TZ,
                known_groups: &self.known_groups,
            };
            let state = &mut self.state;
            let busy = self.busy;
            let mut output = ComposerOutput::default();
            let mut frame = ctx.run_ui(raw, |_ui| {
                output = show(state, &ctx, busy, &context);
            });
            frame.textures_delta.clear();
            output
        }
    }

    fn add_note(
        mut output: ComposerOutput,
    ) -> (String, Option<String>, Option<Priority>, Option<TimeSpec>) {
        let capture = output.capture.take();
        let Some(Command::AddNote {
            text,
            group,
            priority,
            due,
            source,
            parent,
            remind,
        }) = capture
        else {
            panic!("expected one AddNote, got {:?}", output);
        };
        assert_eq!(source, "capture");
        assert_eq!(parent, None);
        assert_eq!(remind, None);
        (text, group, priority, due)
    }

    #[test]
    fn focus_request_focuses_the_editor() {
        let mut harness = Harness::new();
        assert!(!harness.state.has_focus(&harness.ctx));
        harness.focus_editor();
        assert!(harness.state.has_focus(&harness.ctx));
        assert!(!harness.state.focus_requested(), "the request is consumed");
    }

    #[test]
    fn tab_toggles_mode_and_is_not_inserted_as_text() {
        let mut harness = Harness::new();
        harness.state.set_text("keep me");
        harness.focus_editor();

        let output = harness.run(vec![key_event(Key::Tab, Modifiers::NONE)]);

        assert!(output.capture.is_none());
        assert!(output.agent_prompt.is_none());
        assert_eq!(harness.state.mode(), ComposerMode::Agent);
        assert_eq!(harness.state.text(), "keep me", "Tab must not be inserted");
        assert!(
            harness.state.has_focus(&harness.ctx),
            "Tab must keep the editor focused"
        );
    }

    #[test]
    fn enter_in_capture_emits_one_add_note_and_clears() {
        let mut harness = Harness::new();
        harness.state.set_text("buy milk #home !high tomorrow");
        harness.focus_editor();

        let output = harness.run(vec![key_event(Key::Enter, Modifiers::NONE)]);
        let (text, group, priority, due) = add_note(output);

        assert_eq!(text, "buy milk");
        assert_eq!(group.as_deref(), Some("home"));
        assert_eq!(priority, Some(Priority::High));
        let due = due.expect("tomorrow should parse into a due time");
        assert_eq!(due.local, "2026-11-23T09:00:00");
        assert_eq!(harness.state.text(), "", "Enter must clear the input");
        assert!(
            harness.state.has_focus(&harness.ctx),
            "Enter must keep the editor focused"
        );
    }

    #[test]
    fn enter_is_consumed_so_it_does_not_become_a_newline() {
        let mut harness = Harness::new();
        harness.state.set_text("   ");
        harness.focus_editor();

        let output = harness.run(vec![key_event(Key::Enter, Modifiers::NONE)]);

        assert!(output.capture.is_none());
        assert_eq!(
            harness.state.text(),
            "   ",
            "whitespace-only input must not submit or gain a newline"
        );
    }

    #[test]
    fn escape_leaves_editing_without_clearing_the_text() {
        let mut harness = Harness::new();
        harness.state.set_text("keep me");
        harness.focus_editor();

        let output = harness.run(vec![key_event(Key::Escape, Modifiers::NONE)]);

        assert!(output.capture.is_none());
        assert!(output.agent_prompt.is_none());
        assert_eq!(harness.state.text(), "keep me");
        assert!(
            !harness.state.has_focus(&harness.ctx),
            "Esc should surrender focus"
        );
    }

    #[test]
    fn shift_enter_does_not_submit_and_leaves_a_newline() {
        let mut harness = Harness::new();
        harness.state.set_text("line one");
        harness.focus_editor();

        let output = harness.run(vec![key_event(Key::Enter, Modifiers::SHIFT)]);

        assert!(output.capture.is_none());
        assert!(output.agent_prompt.is_none());
        assert_eq!(harness.state.text(), "line one\n");
    }

    #[test]
    fn enter_in_agent_emits_the_raw_prompt_and_clears() {
        let mut harness = Harness::new();
        harness.state.set_mode(ComposerMode::Agent);
        harness.state.set_text("summarise #home tomorrow");
        harness.focus_editor();

        let output = harness.run(vec![key_event(Key::Enter, Modifiers::NONE)]);

        assert_eq!(
            output.agent_prompt.as_deref(),
            Some("summarise #home tomorrow"),
            "Agent mode must not parse dates or chips"
        );
        assert!(output.capture.is_none());
        assert_eq!(harness.state.text(), "");
        assert!(harness.state.has_focus(&harness.ctx));
    }

    #[test]
    fn empty_input_does_not_submit() {
        for mode in [ComposerMode::Capture, ComposerMode::Agent] {
            let mut harness = Harness::new();
            harness.state.set_mode(mode);
            harness.focus_editor();

            let output = harness.run(vec![key_event(Key::Enter, Modifiers::NONE)]);

            assert!(output.capture.is_none(), "mode {mode:?}");
            assert!(output.agent_prompt.is_none(), "mode {mode:?}");
            assert_eq!(harness.state.text(), "", "Enter must still be consumed");
        }
    }

    #[test]
    fn busy_blocks_submitting_and_keeps_the_text() {
        let mut harness = Harness::new();
        harness.state.set_text("buy milk");
        harness.state.set_busy(true);
        harness.focus_editor();

        let output = harness.run(vec![key_event(Key::Enter, Modifiers::NONE)]);

        assert!(output.capture.is_none());
        assert!(output.agent_prompt.is_none());
        assert_eq!(harness.state.text(), "buy milk", "busy must not clear text");
        assert!(harness.state.is_busy());
    }

    #[test]
    fn chips_reflect_group_priority_and_tomorrow() {
        let harness = Harness::with_groups(&["Home"]);
        let context = ComposerContext {
            now_utc_ms: harness.now_utc_ms,
            tz: TZ,
            known_groups: &harness.known_groups,
        };

        let (parsed, chips) = capture_preview(&context, "buy milk #home !high tomorrow");
        assert_eq!(parsed.text, "buy milk");
        assert_eq!(chips.len(), 3);

        assert_eq!(chips[0].kind, ComposerChipKind::Group);
        assert_eq!(chips[0].label, "#Home", "known names are canonicalised");
        assert!(!chips[0].proposed);

        assert_eq!(chips[1].kind, ComposerChipKind::Priority);
        assert_eq!(chips[1].label, "!high");

        assert_eq!(chips[2].kind, ComposerChipKind::Due);
        assert!(
            chips[2].label.contains("2026-11-23 09:00"),
            "due should be shown in the board zone, got {:?}",
            chips[2].label
        );
    }

    #[test]
    fn an_unknown_group_is_a_proposal_chip() {
        let harness = Harness::with_groups(&["Home"]);
        let context = ComposerContext {
            now_utc_ms: harness.now_utc_ms,
            tz: TZ,
            known_groups: &harness.known_groups,
        };

        let (_, chips) = capture_preview(&context, "buy milk #errands");
        let group = chips
            .iter()
            .find(|chip| chip.kind == ComposerChipKind::Group)
            .expect("group chip");
        assert_eq!(group.label, "#errands");
        assert!(group.proposed, "unknown groups are proposed, not known");
    }

    #[test]
    fn a_dst_gap_is_called_out_on_the_due_chip() {
        let harness = Harness::new();
        let context = ComposerContext {
            now_utc_ms: harness.now_utc_ms,
            tz: TZ,
            known_groups: &harness.known_groups,
        };

        let (_, chips) = capture_preview(&context, "spring forward 2026-03-29 01:30");
        let due = chips
            .iter()
            .find(|chip| chip.kind == ComposerChipKind::Due)
            .expect("due chip");
        assert!(
            due.label.contains("DST gap"),
            "gap note missing from {:?}",
            due.label
        );
    }

    #[test]
    fn capture_preview_does_not_need_a_focused_editor() {
        let harness = Harness::new();
        let context = ComposerContext::new(harness.now_utc_ms, TZ, &[]);
        let (parsed, chips) = capture_preview(&context, "#home !low");
        assert_eq!(parsed.text, "");
        assert_eq!(chips.len(), 2);
        let priority = chips
            .iter()
            .find(|chip| chip.kind == ComposerChipKind::Priority)
            .expect("priority chip");
        assert_eq!(priority.label, "!low");
    }
}
