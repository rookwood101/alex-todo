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

use eframe::egui::text::{CCursor, CCursorRange, LayoutJob, TextFormat};
use eframe::egui::{
    self, Align2, Color32, Event, EventFilter, FontId, Id, Key, Margin, Modifiers, RichText,
    Stroke, TextStyle, vec2,
};
use jiff::Timestamp;
use jiff::tz::TimeZone;

use crate::capture::{self, MarkKind, ParsedCapture};
use crate::command::Command;
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
            Self::Capture => "Enter adds  ·  Tab for Agent  ·  F1 syntax help",
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
    /// The syntax explainer was switched on (true) or off (false).
    pub help_toggled: Option<bool>,
    /// Height of the floating panel this frame (the chat panel sits above).
    pub panel_height: f32,
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
    /// Show the syntax explainer above the input while it has focus.
    help_visible: bool,
    /// Highlighted row in the # autocomplete list.
    suggest_index: usize,
    /// The # token the user dismissed with Escape (hidden until it changes).
    suggest_dismissed: Option<String>,
    /// Where to put the cursor after accepting a suggestion (char index).
    cursor_after: Option<usize>,
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

    /// Show or hide the syntax explainer (the app restores it from config).
    pub fn set_help_visible(&mut self, visible: bool) {
        self.help_visible = visible;
    }

    pub fn help_visible(&self) -> bool {
        self.help_visible
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
    /// The "!" importance token.
    Important,
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
    let suggestion = if state.mode == ComposerMode::Capture && state.has_focus(ctx) {
        current_suggestions(state, ctx, context)
    } else {
        None
    };
    if state.has_focus(ctx) && ctx.input_mut(|input| input.consume_key(Modifiers::NONE, Key::F1)) {
        state.help_visible = !state.help_visible;
        output.help_toggled = Some(state.help_visible);
        refocus = true;
    }
    if let Some((token, names)) = &suggestion {
        let (up, down, accept, dismiss) = ctx.input_mut(|input| {
            (
                input.consume_key(Modifiers::NONE, Key::ArrowUp),
                input.consume_key(Modifiers::NONE, Key::ArrowDown),
                input.consume_key(Modifiers::NONE, Key::Tab)
                    | input.consume_key(Modifiers::NONE, Key::Enter),
                input.consume_key(Modifiers::NONE, Key::Escape),
            )
        });
        if up {
            state.suggest_index = state.suggest_index.saturating_sub(1);
        }
        if down {
            state.suggest_index = (state.suggest_index + 1).min(names.len().saturating_sub(1));
        }
        if accept && let Some(name) = names.get(state.suggest_index) {
            let (text, cursor) = apply_suggestion(&state.text, token.start, token.end, name);
            state.text = text;
            state.cursor_after = Some(cursor);
            state.suggest_index = 0;
            refocus = true;
        }
        if dismiss {
            state.suggest_dismissed = Some(token.text.clone());
            refocus = true;
        }
    }

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

    let shown = area.show(ctx, |ui| {
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
                    if state.mode == ComposerMode::Capture {
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            let label = RichText::new("?").size(12.0).strong().color(
                                if state.help_visible {
                                    theme::ACCENT
                                } else {
                                    theme::TEXT_MUTED
                                },
                            );
                            if ui
                                .add(egui::Button::new(label).frame(false))
                                .on_hover_text("Show or hide the syntax help (F1)")
                                .clicked()
                            {
                                state.help_visible = !state.help_visible;
                                output.help_toggled = Some(state.help_visible);
                                state.focus_requested = true;
                            }
                        });
                    }
                });

                let focused = state.has_focus(ui.ctx()) || state.focus_requested;
                if state.mode == ComposerMode::Capture && state.help_visible && focused {
                    ui.add_space(6.0);
                    syntax_help(ui);
                }
                if let Some((_, names)) = &suggestion {
                    ui.add_space(6.0);
                    let mut picked = None;
                    egui::Frame::new()
                        .fill(theme::HINT_BG)
                        .stroke(Stroke::new(1.0, theme::BORDER))
                        .corner_radius(6)
                        .inner_margin(Margin::symmetric(6, 4))
                        .show(ui, |ui| {
                            for (index, name) in names.iter().enumerate() {
                                let active = index == state.suggest_index;
                                let text = RichText::new(format!("#{}", tag_for(name)))
                                    .size(13.0)
                                    .color(if active {
                                        theme::TEXT_PRIMARY
                                    } else {
                                        theme::TEXT_MUTED
                                    });
                                let response = ui.selectable_label(active, text);
                                if response.clicked() {
                                    picked = Some(index);
                                }
                            }
                            ui.label(
                                RichText::new("↑↓ choose · Tab or Enter accept · Esc dismiss")
                                    .size(10.5)
                                    .color(theme::TEXT_MUTED),
                            );
                        });
                    if let (Some(index), Some((token, names))) = (picked, &suggestion)
                        && let Some(name) = names.get(index)
                    {
                        let (text, cursor) =
                            apply_suggestion(&state.text, token.start, token.end, name);
                        state.text = text;
                        state.cursor_after = Some(cursor);
                        state.focus_requested = true;
                    }
                }

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
                    ComposerMode::Capture => {
                        if state.text.is_empty() {
                            ui.ctx()
                                .request_repaint_after(std::time::Duration::from_secs(4));
                        }
                        placeholder_example(context.now_utc_ms)
                    }
                    ComposerMode::Agent => "Ask the agent and press Enter…",
                };
                let highlight = state.mode == ComposerMode::Capture;
                let now_utc_ms = context.now_utc_ms;
                let tz = context.tz;
                let known = context.known_groups;
                let mut layouter = |ui: &egui::Ui, buffer: &dyn egui::TextBuffer, wrap: f32| {
                    let text = buffer.as_str();
                    let marks = if highlight {
                        capture::parse_capture(text, now_utc_ms, tz, known).marks
                    } else {
                        Vec::new()
                    };
                    let mut job = highlight_job(text, &marks);
                    job.wrap.max_width = wrap;
                    ui.fonts_mut(|fonts| fonts.layout_job(job))
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
                    .event_filter(filter)
                    .layouter(&mut layouter);

                let scroll = egui::ScrollArea::vertical()
                    .id_salt("rustboard.composer.scroll")
                    .max_height(row_height * MAX_LINES as f32 + 16.0)
                    .auto_shrink([false, true]);
                let cursor_after = state.cursor_after.take();
                scroll.show(ui, |ui| {
                    let mut shown = editor.show(ui);
                    if let Some(index) = cursor_after {
                        shown
                            .state
                            .cursor
                            .set_char_range(Some(CCursorRange::one(CCursor::new(index))));
                        shown.state.store(ui.ctx(), editor_id());
                    }
                });
            });
    });

    output.panel_height = shown.response.rect.height() + BOTTOM_OFFSET;

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
            let has_chips = parsed.group.is_some() || parsed.important || parsed.due.is_some();
            if parsed.text.trim().is_empty() && !has_chips {
                return;
            }
            output.capture = Some(Command::AddNote {
                text: parsed.text,
                parent: None,
                container: parsed.group.map(|group| group.name),
                at_root: false,
                due: parsed.due,
                remind: None,
                important: parsed.important,
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
    if parsed.important {
        chips.push(ComposerChip {
            kind: ComposerChipKind::Important,
            label: "! important".to_owned(),
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
        ComposerChipKind::Important => (theme::BUTTON_BG, theme::SAVE_ERROR),
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
            .on_hover_text("This container does not exist yet; Enter creates it");
    }
}

/// Example lines the empty input cycles through.
const EXAMPLES: &[&str] = &[
    "buy milk #home @tomorrow",
    "pay rent fri #home !",
    "call bob @fri 4pm about the invoice",
    "draft roadmap #work/q4 next week",
    "stretch in 2h",
    "send report eod #work !",
];

/// The placeholder example for this moment (changes every four seconds).
fn placeholder_example(now_utc_ms: i64) -> &'static str {
    let index = (now_utc_ms / 4000).rem_euclid(EXAMPLES.len() as i64) as usize;
    EXAMPLES[index]
}

/// The compact syntax reference shown above the focused input.
fn syntax_help(ui: &mut egui::Ui) {
    egui::Frame::new()
        .fill(theme::HINT_BG)
        .corner_radius(6)
        .inner_margin(Margin::symmetric(10, 6))
        .show(ui, |ui| {
            let row = |ui: &mut egui::Ui, token: &str, color: Color32, meaning: &str| {
                ui.horizontal(|ui| {
                    ui.add_sized(
                        vec2(110.0, 16.0),
                        egui::Label::new(RichText::new(token).monospace().size(12.0).color(color)),
                    );
                    ui.label(RichText::new(meaning).size(12.0).color(theme::TEXT_MUTED));
                });
            };
            row(
                ui,
                "#home",
                mark_color(MarkKind::Container),
                "put it in a container (made if missing); #work/q4 nests",
            );
            row(ui, "!", mark_color(MarkKind::Important), "important");
            row(
                ui,
                "@fri 4pm",
                mark_color(MarkKind::Due),
                "due date anywhere in the line",
            );
            row(
                ui,
                "… tomorrow",
                mark_color(MarkKind::Due),
                "or at the end, no @: today, fri 4pm, in 2h, eod, next week, weekend, 12/1",
            );
        });
}

fn mark_color(kind: MarkKind) -> Color32 {
    match kind {
        MarkKind::Container => theme::ACCENT,
        MarkKind::Important => theme::IMPORTANT,
        MarkKind::Due => theme::SAVE_WARN,
    }
}

/// The composer text with parsed chips coloured by kind.
fn highlight_job(text: &str, marks: &[capture::Mark]) -> LayoutJob {
    let plain = TextFormat {
        font_id: FontId::proportional(TEXT_SIZE),
        color: theme::TEXT_PRIMARY,
        ..Default::default()
    };
    let mut job = LayoutJob::default();
    let mut cursor = 0;
    for mark in marks {
        let (start, end) = (
            mark.span.start.min(text.len()),
            mark.span.end.min(text.len()),
        );
        if start < cursor || !text.is_char_boundary(start) || !text.is_char_boundary(end) {
            continue;
        }
        job.append(&text[cursor..start], 0.0, plain.clone());
        let mut format = plain.clone();
        format.color = mark_color(mark.kind);
        format.background = mark_color(mark.kind).gamma_multiply(0.15);
        job.append(&text[start..end], 0.0, format);
        cursor = end;
    }
    job.append(&text[cursor..], 0.0, plain);
    job
}

/// The "#..." token that ends at the cursor, as byte offsets.
#[derive(Clone, Debug, PartialEq, Eq)]
struct HashToken {
    start: usize,
    end: usize,
    /// The whole token, including "#".
    text: String,
}

fn hash_token_at(text: &str, cursor_chars: usize) -> Option<HashToken> {
    let cursor = text
        .char_indices()
        .nth(cursor_chars)
        .map_or(text.len(), |(byte, _)| byte);
    let start = text[..cursor]
        .rfind(char::is_whitespace)
        .map_or(0, |index| {
            index + text[index..].chars().next().map_or(1, char::len_utf8)
        });
    let end = text[cursor..]
        .find(char::is_whitespace)
        .map_or(text.len(), |offset| cursor + offset);
    let token = &text[start..end];
    token.starts_with('#').then(|| HashToken {
        start,
        end,
        text: token.to_owned(),
    })
}

/// Container names whose normalised form starts with the typed prefix.
fn suggestions_for(prefix: &str, known: &[String]) -> Vec<String> {
    let wanted = crate::model::normalize_container_name(prefix);
    let mut names: Vec<String> = known
        .iter()
        .filter(|name| {
            let normal = crate::model::normalize_container_name(name);
            normal.starts_with(&wanted) && normal != wanted
        })
        .cloned()
        .collect();
    names.truncate(6);
    names
}

/// A name as a # tag: spaces become hyphens.
fn tag_for(name: &str) -> String {
    name.split_whitespace().collect::<Vec<_>>().join("-")
}

/// Replace text[start..end] with "#tag " and return the new text and the
/// cursor position (in chars) after the inserted space.
fn apply_suggestion(text: &str, start: usize, end: usize, name: &str) -> (String, usize) {
    let insert = format!("#{} ", tag_for(name));
    let rest = text[end..].trim_start();
    let new_text = format!("{}{}{}", &text[..start], insert, rest);
    let cursor = text[..start].chars().count() + insert.chars().count();
    (new_text, cursor)
}

/// The # token at the cursor and its suggestions, when there are any.
fn current_suggestions(
    state: &mut ComposerState,
    ctx: &egui::Context,
    context: &ComposerContext<'_>,
) -> Option<(HashToken, Vec<String>)> {
    let cursor = egui::text_edit::TextEditState::load(ctx, editor_id())
        .and_then(|edit| edit.cursor.char_range())
        .map(|range| range.primary.index.0)?;
    let token = hash_token_at(&state.text, cursor)?;
    if state.suggest_dismissed.as_deref() == Some(token.text.as_str()) {
        return None;
    }
    state.suggest_dismissed = None;
    let names = suggestions_for(&token.text[1..], context.known_groups);
    if names.is_empty() {
        state.suggest_index = 0;
        return None;
    }
    state.suggest_index = state.suggest_index.min(names.len() - 1);
    Some((token, names))
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

    fn add_note(mut output: ComposerOutput) -> (String, Option<String>, bool, Option<TimeSpec>) {
        let capture = output.capture.take();
        let Some(Command::AddNote {
            text,
            container: group,
            important: priority,
            due,
            source,
            parent,
            remind,
            at_root,
        }) = capture
        else {
            panic!("expected one AddNote, got {:?}", output);
        };
        assert_eq!(source, "capture");
        assert_eq!(parent, None);
        assert_eq!(remind, None);
        assert!(!at_root);
        (text, group, priority, due)
    }

    #[test]
    fn hash_tokens_suggestions_and_acceptance() {
        let text = "buy milk #ho";
        let token = hash_token_at(text, text.chars().count()).expect("token");
        assert_eq!(token.text, "#ho");
        assert_eq!(&text[token.start..token.end], "#ho");
        assert!(hash_token_at("buy milk", 3).is_none());

        let known = vec![
            "Home".to_owned(),
            "Big project".to_owned(),
            "Work".to_owned(),
        ];
        assert_eq!(suggestions_for("ho", &known), vec!["Home".to_owned()]);
        assert_eq!(
            suggestions_for("big-p", &known),
            vec!["Big project".to_owned()]
        );
        assert!(
            suggestions_for("home", &known).is_empty(),
            "exact match hides the list"
        );

        let (text, cursor) = apply_suggestion("buy #bi now", 4, 7, "Big project");
        assert_eq!(text, "buy #Big-project now");
        assert_eq!(cursor, "buy #Big-project ".chars().count());
    }

    #[test]
    fn highlight_colours_each_mark() {
        let parsed = capture::parse_capture("a #home ! @fri", 1_800_000_000_000, TZ, &[]);
        let job = highlight_job("a #home ! @fri", &parsed.marks);
        let colours: Vec<Color32> = job.sections.iter().map(|s| s.format.color).collect();
        assert!(colours.contains(&theme::ACCENT));
        assert!(colours.contains(&theme::IMPORTANT));
        assert!(colours.contains(&theme::SAVE_WARN));
        assert_eq!(job.text, "a #home ! @fri");
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
        assert!(priority);
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
    fn chips_reflect_group_importance_and_tomorrow() {
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

        assert_eq!(chips[1].kind, ComposerChipKind::Important);

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
    fn a_dst_fold_is_called_out_on_the_due_chip() {
        let harness = Harness::new();
        let context = ComposerContext {
            now_utc_ms: london_now("2026-10-24T08:00:00"),
            tz: TZ,
            known_groups: &harness.known_groups,
        };

        let (_, chips) = capture_preview(&context, "clock change 2026-10-25 01:30");
        let due = chips
            .iter()
            .find(|chip| chip.kind == ComposerChipKind::Due)
            .expect("due chip");
        assert!(
            due.label.contains("DST fold"),
            "fold note missing from {:?}",
            due.label
        );
    }

    #[test]
    fn capture_preview_does_not_need_a_focused_editor() {
        let harness = Harness::new();
        let context = ComposerContext::new(harness.now_utc_ms, TZ, &[]);
        let (parsed, chips) = capture_preview(&context, "#home !");
        assert_eq!(parsed.text, "");
        assert_eq!(chips.len(), 2);
        assert!(
            chips
                .iter()
                .any(|chip| chip.kind == ComposerChipKind::Important)
        );
    }
}
