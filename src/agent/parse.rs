//! Strict JSONL parsing for Pi's RPC stdout.
//!
//! Framing is deliberately byte-level: records are split only on LF (the
//! reader does that) and a trailing CR is stripped. U+2028/U+2029 are valid
//! inside JSON strings and must never be treated as record separators.

use std::collections::{HashMap, HashSet};

use serde_json::Value;

use super::shared::Shared;
use super::{AgentEvent, HistoryMessage, ModelChoice};

/// A Pi Model object ({provider, id, name, ...}).
fn model_choice(value: &Value) -> Option<ModelChoice> {
    let provider = value.get("provider").and_then(Value::as_str)?.to_owned();
    let id = value.get("id").and_then(Value::as_str)?.to_owned();
    let name = value
        .get("name")
        .and_then(Value::as_str)
        .filter(|name| !name.is_empty())
        .unwrap_or(&id)
        .to_owned();
    Some(ModelChoice { provider, id, name })
}

/// User and assistant text from Pi's `get_messages` AgentMessage list.
fn history_messages(messages: &[Value]) -> Vec<HistoryMessage> {
    messages
        .iter()
        .filter_map(|message| {
            let user = match message.get("role").and_then(Value::as_str)? {
                "user" => true,
                "assistant" => false,
                _ => return None,
            };
            let text = match message.get("content")? {
                Value::String(text) => text.clone(),
                Value::Array(parts) => parts
                    .iter()
                    .filter(|part| part.get("type").and_then(Value::as_str) == Some("text"))
                    .filter_map(|part| part.get("text").and_then(Value::as_str))
                    .collect::<Vec<_>>()
                    .join("\n\n"),
                _ => return None,
            };
            let text = text.trim().to_owned();
            (!text.is_empty()).then_some(HistoryMessage { user, text })
        })
        .collect()
}

/// Per-process parser state. Created fresh on every spawn so a restart cannot
/// confuse old tool ids or text blocks with a new run.
#[derive(Debug, Default)]
pub(crate) struct ParserState {
    text_started: HashSet<u64>,
    text_streamed: HashSet<u64>,
    tools: HashMap<String, String>,
    stop_reason: Option<String>,
    pending_agent_end: Option<String>,
    last_cost: Option<(Option<u64>, Option<u64>, Option<f64>)>,
}

impl ParserState {
    /// Parse one complete JSONL record (without its trailing LF).
    pub(crate) fn parse_line(&mut self, line: &[u8], shared: &Shared) -> Vec<AgentEvent> {
        let mut end = line.len();
        while end > 0 && matches!(line[end - 1], b'\n' | b'\r') {
            end -= 1;
        }
        let line = &line[..end];
        if line.iter().all(u8::is_ascii_whitespace) {
            return Vec::new();
        }

        let record: Value = match serde_json::from_slice(line) {
            Ok(record) => record,
            Err(error) => {
                return vec![AgentEvent::Error(format!(
                    "pi sent invalid JSON: {error} (record: {})",
                    truncate(&String::from_utf8_lossy(line), 200)
                ))];
            }
        };
        self.parse_record(&record, shared)
    }

    /// Parse one decoded record. Public to the crate so parser tests can feed
    /// values without a child process.
    pub(crate) fn parse_record(&mut self, record: &Value, shared: &Shared) -> Vec<AgentEvent> {
        let mut events = Vec::new();
        let kind = record
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or_default();

        match kind {
            "agent_start" => {
                self.pending_agent_end = None;
                self.stop_reason = None;
                self.last_cost = None;
                self.text_started.clear();
                self.text_streamed.clear();
            }
            "response" => self.parse_response(record, shared, &mut events),
            "message_update" => self.parse_message_update(record, &mut events),
            "message_end" => self.parse_message_end(record, &mut events),
            "tool_execution_start" => {
                if let Some(name) = tool_name(record) {
                    let id = record
                        .get("toolCallId")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_owned();
                    if !id.is_empty() {
                        self.tools.insert(id.clone(), name.clone());
                    }
                    events.push(AgentEvent::ToolStart { id, name });
                }
            }
            "tool_execution_end" => {
                let id = record.get("toolCallId").and_then(Value::as_str);
                let name = tool_name(record).or_else(|| id.and_then(|id| self.tools.remove(id)));
                if let Some(id) = id {
                    self.tools.remove(id);
                }
                if let Some(name) = name {
                    let ok = !record
                        .get("isError")
                        .and_then(Value::as_bool)
                        .unwrap_or(false);
                    events.push(AgentEvent::ToolEnd {
                        id: id.unwrap_or_default().to_owned(),
                        name,
                        ok,
                    });
                }
            }
            "agent_end" => {
                let aborted = record
                    .get("aborted")
                    .and_then(Value::as_bool)
                    .unwrap_or(false);
                let reason = if aborted {
                    "aborted".to_owned()
                } else {
                    self.stop_reason
                        .clone()
                        .unwrap_or_else(|| "completed".to_owned())
                };
                self.pending_agent_end = Some(reason);
            }
            "agent_settled" => {
                let aborted = record
                    .get("aborted")
                    .and_then(Value::as_bool)
                    .unwrap_or(false);
                let reason = if aborted {
                    "aborted".to_owned()
                } else {
                    self.pending_agent_end
                        .take()
                        .or_else(|| self.stop_reason.clone())
                        .unwrap_or_else(|| "completed".to_owned())
                };
                events.push(AgentEvent::TurnEnd { reason });
                self.reset_turn();
            }
            "error" | "extension_error" | "auth_error" => {
                let message =
                    error_text(record).unwrap_or_else(|| "pi reported an error".to_owned());
                events.push(AgentEvent::Error(format!("pi error: {message}")));
            }
            other if other.ends_with("_error") => {
                let message = error_text(record).unwrap_or_else(|| format!("pi reported {other}"));
                events.push(AgentEvent::Error(format!("pi error: {message}")));
            }
            // Unknown record types are forward-compatible: ignore them.
            _ => {}
        }

        if let Some(cost) = self.cost_event(record) {
            events.push(cost);
        }
        events
    }

    /// Flush a turn that ended with `agent_end` but no `agent_settled` (for
    /// example a child that exits right after a turn).
    pub(crate) fn finish(&mut self) -> Vec<AgentEvent> {
        let Some(reason) = self.pending_agent_end.take() else {
            return Vec::new();
        };
        self.reset_turn();
        vec![AgentEvent::TurnEnd { reason }]
    }

    fn reset_turn(&mut self) {
        self.text_started.clear();
        self.text_streamed.clear();
        self.tools.clear();
        self.stop_reason = None;
        self.pending_agent_end = None;
    }

    fn parse_response(&mut self, record: &Value, shared: &Shared, events: &mut Vec<AgentEvent>) {
        let pending = record
            .get("id")
            .and_then(Value::as_str)
            .and_then(|id| shared.take_pending(id));
        let success = record
            .get("success")
            .and_then(Value::as_bool)
            .unwrap_or(true);
        let command = record
            .get("command")
            .and_then(Value::as_str)
            .map(str::to_owned)
            .or(pending)
            .unwrap_or_else(|| "command".to_owned());
        if success {
            let data = record.get("data");
            match command.as_str() {
                "get_messages" => {
                    if let Some(messages) = data
                        .and_then(|data| data.get("messages"))
                        .and_then(Value::as_array)
                    {
                        events.push(AgentEvent::History(history_messages(messages)));
                    }
                }
                "get_available_models" => {
                    if let Some(models) = data
                        .and_then(|data| data.get("models"))
                        .and_then(Value::as_array)
                    {
                        events.push(AgentEvent::Models(
                            models.iter().filter_map(model_choice).collect(),
                        ));
                    }
                }
                "get_state" => {
                    let model = data
                        .and_then(|data| data.get("model"))
                        .and_then(model_choice);
                    events.push(AgentEvent::CurrentModel(model));
                }
                "set_model" => {
                    let model = data.and_then(|data| {
                        model_choice(data).or_else(|| data.get("model").and_then(model_choice))
                    });
                    if model.is_some() {
                        events.push(AgentEvent::CurrentModel(model));
                    }
                }
                _ => {}
            }
            return;
        }
        let message = record
            .get("error")
            .map(value_text)
            .filter(|message| !message.is_empty())
            .unwrap_or_else(|| "unknown error".to_owned());
        events.push(AgentEvent::Error(format!(
            "pi {command} failed: {}",
            truncate(&message, 400)
        )));
        if command == "prompt" {
            // A rejected prompt (for example missing auth) never starts a
            // run, so Pi sends no agent_settled: end the turn here.
            events.push(AgentEvent::TurnEnd {
                reason: "error".to_owned(),
            });
        }
    }

    fn parse_message_update(&mut self, record: &Value, events: &mut Vec<AgentEvent>) {
        let Some(update) = record
            .get("assistantMessageEvent")
            .or_else(|| record.get("assistant_message_event"))
        else {
            return;
        };
        let kind = update
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let index = update
            .get("contentIndex")
            .and_then(Value::as_u64)
            .unwrap_or(0);

        match kind {
            "text_start" => {
                self.text_started.insert(index);
                self.text_streamed.remove(&index);
                events.push(AgentEvent::TextBreak);
            }
            "text_delta" => {
                if let Some(delta) = update.get("delta") {
                    let delta = value_text(delta);
                    if !delta.is_empty() {
                        self.text_streamed.insert(index);
                        events.push(AgentEvent::TextDelta(delta));
                    }
                }
            }
            "text_end" => {
                // `text_end` is authoritative, but do not duplicate a block
                // that already streamed as deltas.
                let streamed = self.text_streamed.remove(&index);
                self.text_started.remove(&index);
                if !streamed && let Some(content) = update.get("content") {
                    let content = value_text(content);
                    if !content.is_empty() {
                        events.push(AgentEvent::TextDelta(content));
                    }
                }
            }
            "error" => {
                let message =
                    error_text(update).unwrap_or_else(|| "provider stream failed".to_owned());
                events.push(AgentEvent::Error(format!(
                    "pi stream error: {}",
                    truncate(&message, 400)
                )));
            }
            // thinking_*, toolcall_*, start and done are not transcript text.
            _ => {}
        }
    }

    fn parse_message_end(&mut self, record: &Value, events: &mut Vec<AgentEvent>) {
        let Some(message) = record.get("message") else {
            return;
        };
        if message.get("role").and_then(Value::as_str) != Some("assistant") {
            return;
        }
        match message
            .get("stopReason")
            .and_then(Value::as_str)
            .unwrap_or_default()
        {
            "aborted" => self.stop_reason = Some("aborted".to_owned()),
            "error" => {
                self.stop_reason = Some("error".to_owned());
                if let Some(error) = message
                    .get("errorMessage")
                    .map(value_text)
                    .filter(|message| !message.is_empty())
                {
                    events.push(AgentEvent::Error(format!(
                        "pi turn failed: {}",
                        truncate(&error, 400)
                    )));
                }
            }
            _ => {
                if self.stop_reason.is_none() {
                    self.stop_reason = Some("completed".to_owned());
                }
            }
        }
    }

    fn cost_event(&mut self, record: &Value) -> Option<AgentEvent> {
        let usage = record.get("usage").or_else(|| {
            record
                .get("message")
                .and_then(|message| message.get("usage"))
        })?;
        let input_tokens =
            number_u64(usage.get("input")).or_else(|| number_u64(usage.get("inputTokens")));
        let output_tokens =
            number_u64(usage.get("output")).or_else(|| number_u64(usage.get("outputTokens")));
        let cost_usd = usage
            .get("cost")
            .and_then(|cost| cost.get("total").or(Some(cost)))
            .and_then(number_f64)
            .or_else(|| usage.get("costUsd").and_then(number_f64));

        if input_tokens.is_none() && output_tokens.is_none() && cost_usd.is_none() {
            return None;
        }
        let next = (input_tokens, output_tokens, cost_usd);
        if self.last_cost.as_ref() == Some(&next) {
            return None;
        }
        self.last_cost = Some(next);
        Some(AgentEvent::Cost {
            input_tokens,
            output_tokens,
            cost_usd,
        })
    }
}

fn tool_name(record: &Value) -> Option<String> {
    record
        .get("toolName")
        .or_else(|| record.get("name"))
        .and_then(Value::as_str)
        .map(str::to_owned)
}

fn error_text(value: &Value) -> Option<String> {
    for key in [
        "message",
        "error",
        "reason",
        "errorMessage",
        "error_message",
    ] {
        if let Some(field) = value.get(key) {
            let text = value_text(field);
            if !text.is_empty() {
                return Some(text);
            }
        }
    }
    None
}

fn value_text(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        Value::Null => String::new(),
        other => other.to_string(),
    }
}

fn number_u64(value: Option<&Value>) -> Option<u64> {
    match value? {
        Value::Number(number) => number
            .as_u64()
            .or_else(|| number.as_i64().and_then(|n| u64::try_from(n).ok()))
            .or_else(|| number.as_f64().filter(|n| *n >= 0.0).map(|n| n as u64)),
        _ => None,
    }
}

fn number_f64(value: &Value) -> Option<f64> {
    match value {
        Value::Number(number) => number.as_f64(),
        _ => None,
    }
}

fn truncate(text: &str, max_chars: usize) -> String {
    if text.chars().count() <= max_chars {
        text.to_owned()
    } else {
        text.chars().take(max_chars).collect::<String>() + "..."
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn shared() -> Shared {
        let (sender, _receiver) = crossbeam_channel::unbounded();
        Shared::new(sender)
    }

    #[test]
    fn model_responses_become_model_events() {
        let shared = shared();
        let mut parser = ParserState::default();
        let models = parser.parse_line(
            br#"{"id":"m1","type":"response","command":"get_available_models","success":true,"data":{"models":[{"provider":"google-vertex","id":"gemini-4-carbon","name":"Gemini 4 Carbon"},{"provider":"google-vertex","id":"gemini-3.8-flash"}]}}"#,
            &shared,
        );
        assert_eq!(
            models,
            vec![AgentEvent::Models(vec![
                ModelChoice {
                    provider: "google-vertex".to_owned(),
                    id: "gemini-4-carbon".to_owned(),
                    name: "Gemini 4 Carbon".to_owned()
                },
                ModelChoice {
                    provider: "google-vertex".to_owned(),
                    id: "gemini-3.8-flash".to_owned(),
                    name: "gemini-3.8-flash".to_owned()
                },
            ])]
        );
        let state = parser.parse_line(
            br#"{"id":"s1","type":"response","command":"get_state","success":true,"data":{"model":{"provider":"google-vertex","id":"gemini-4-carbon"}}}"#,
            &shared,
        );
        assert!(matches!(
            state.as_slice(),
            [AgentEvent::CurrentModel(Some(model))] if model.id == "gemini-4-carbon"
        ));
        let changed = parser.parse_line(
            br#"{"id":"x","type":"response","command":"set_model","success":true,"data":{"provider":"google-vertex","id":"gemini-3.8-flash"}}"#,
            &shared,
        );
        assert!(matches!(
            changed.as_slice(),
            [AgentEvent::CurrentModel(Some(model))] if model.id == "gemini-3.8-flash"
        ));
    }

    #[test]
    fn get_messages_becomes_history_with_text_only() {
        let shared = shared();
        shared.register_pending("g1", "get_messages");
        let mut parser = ParserState::default();
        let events = parser.parse_line(
            br#"{"id":"g1","type":"response","success":true,"data":{"messages":[
                {"role":"user","content":"add milk"},
                {"role":"assistant","content":[{"type":"toolCall","name":"x"},{"type":"text","text":"Added."}]},
                {"role":"toolResult","content":[{"type":"text","text":"ok"}]},
                {"role":"assistant","content":[{"type":"thinking","thinking":"hm"}]}
            ]}}"#,
            &shared,
        );
        assert_eq!(
            events,
            vec![AgentEvent::History(vec![
                HistoryMessage {
                    user: true,
                    text: "add milk".to_owned()
                },
                HistoryMessage {
                    user: false,
                    text: "Added.".to_owned()
                },
            ])]
        );
    }

    fn parse_all(jsonl: &str) -> Vec<AgentEvent> {
        let shared = shared();
        let mut parser = ParserState::default();
        let mut events = Vec::new();
        for line in jsonl.as_bytes().split(|byte| *byte == b'\n') {
            events.extend(parser.parse_line(line, &shared));
        }
        events.extend(parser.finish());
        events
    }

    const NORMAL_TURN: &str = r#"{"type":"agent_start"}
{"type":"turn_start"}
{"type":"message_start","message":{"role":"assistant","stopReason":"pending"}}
{"type":"message_update","assistantMessageEvent":{"type":"text_start","contentIndex":0}}
{"type":"message_update","assistantMessageEvent":{"type":"text_delta","contentIndex":0,"delta":"Hello"}}
{"type":"message_update","assistantMessageEvent":{"type":"text_delta","contentIndex":0,"delta":" world"}}
{"type":"message_update","assistantMessageEvent":{"type":"text_end","contentIndex":0,"content":"Hello world"}}
{"type":"message_end","message":{"role":"assistant","stopReason":"stop"}}
{"type":"turn_end","message":{"role":"assistant"},"toolResults":[]}
{"type":"agent_end","messages":[],"willRetry":false}
{"type":"agent_settled","aborted":false}"#;

    #[test]
    fn normal_turn_streams_text_once_and_ends_completed() {
        assert_eq!(
            parse_all(NORMAL_TURN),
            vec![
                AgentEvent::TextBreak,
                AgentEvent::TextDelta("Hello".to_owned()),
                AgentEvent::TextDelta(" world".to_owned()),
                AgentEvent::TurnEnd {
                    reason: "completed".to_owned()
                },
            ]
        );
    }

    #[test]
    fn text_end_without_deltas_emits_the_authoritative_content() {
        let jsonl = r#"{"type":"message_update","assistantMessageEvent":{"type":"text_start","contentIndex":0}}
{"type":"message_update","assistantMessageEvent":{"type":"text_end","contentIndex":0,"content":"whole answer"}}
{"type":"message_update","assistantMessageEvent":{"type":"text_start","contentIndex":1}}
{"type":"message_update","assistantMessageEvent":{"type":"text_delta","contentIndex":1,"delta":"streamed"}}
{"type":"message_update","assistantMessageEvent":{"type":"text_end","contentIndex":1,"content":"streamed"}}"#;
        assert_eq!(
            parse_all(jsonl),
            vec![
                AgentEvent::TextBreak,
                AgentEvent::TextDelta("whole answer".to_owned()),
                AgentEvent::TextBreak,
                AgentEvent::TextDelta("streamed".to_owned()),
            ]
        );
    }

    const TOOL_TURN: &str = r#"{"type":"agent_start"}
{"type":"tool_execution_start","toolCallId":"c1","toolName":"mcp__rustboard__add_note","args":{}}
{"type":"tool_execution_end","toolCallId":"c1","toolName":"mcp__rustboard__add_note","result":{},"isError":false}
{"type":"tool_execution_start","toolCallId":"c2","toolName":"mcp__rustboard__board_sql","args":{}}
{"type":"tool_execution_end","toolCallId":"c2","toolName":"mcp__rustboard__board_sql","result":{},"isError":true}
{"type":"tool_execution_start","toolCallId":"c3","toolName":"mcp__rustboard__search","args":{}}
{"type":"tool_execution_end","toolCallId":"c3","isError":false}
{"type":"agent_end","willRetry":false}
{"type":"agent_settled","aborted":false}"#;

    #[test]
    fn tool_lifecycle_maps_to_start_and_end_events() {
        assert_eq!(
            parse_all(TOOL_TURN),
            vec![
                AgentEvent::ToolStart {
                    id: "c1".to_owned(),
                    name: "mcp__rustboard__add_note".to_owned()
                },
                AgentEvent::ToolEnd {
                    id: "c1".to_owned(),
                    name: "mcp__rustboard__add_note".to_owned(),
                    ok: true
                },
                AgentEvent::ToolStart {
                    id: "c2".to_owned(),
                    name: "mcp__rustboard__board_sql".to_owned()
                },
                AgentEvent::ToolEnd {
                    id: "c2".to_owned(),
                    name: "mcp__rustboard__board_sql".to_owned(),
                    ok: false
                },
                AgentEvent::ToolStart {
                    id: "c3".to_owned(),
                    name: "mcp__rustboard__search".to_owned()
                },
                AgentEvent::ToolEnd {
                    id: "c3".to_owned(),
                    name: "mcp__rustboard__search".to_owned(),
                    ok: true
                },
                AgentEvent::TurnEnd {
                    reason: "completed".to_owned()
                },
            ]
        );
    }

    const ABORTED_TURN: &str = r#"{"type":"agent_start"}
{"type":"message_update","assistantMessageEvent":{"type":"text_start","contentIndex":0}}
{"type":"message_update","assistantMessageEvent":{"type":"text_delta","contentIndex":0,"delta":"Working"}}
{"type":"message_end","message":{"role":"assistant","stopReason":"aborted","errorMessage":"Request was aborted"}}
{"type":"agent_end","willRetry":false}
{"type":"agent_settled","aborted":true}
{"type":"response","id":"a1","command":"abort","success":true}"#;

    #[test]
    fn abort_ends_the_turn_with_reason_aborted() {
        assert_eq!(
            parse_all(ABORTED_TURN),
            vec![
                AgentEvent::TextBreak,
                AgentEvent::TextDelta("Working".to_owned()),
                AgentEvent::TurnEnd {
                    reason: "aborted".to_owned()
                },
            ]
        );
    }

    #[test]
    fn error_records_and_failed_responses_become_error_events() {
        let shared = shared();
        shared.register_pending("p1", "prompt");
        let mut parser = ParserState::default();

        let error = parser.parse_line(
            br#"{"type":"error","message":"No API key found for deepseek."}"#,
            &shared,
        );
        assert_eq!(
            error,
            vec![AgentEvent::Error(
                "pi error: No API key found for deepseek.".to_owned()
            )]
        );

        let response = parser.parse_line(
            br#"{"id":"p1","type":"response","command":"prompt","success":false,"error":"No API key found for deepseek."}"#,
            &shared,
        );
        assert_eq!(
            response,
            vec![
                AgentEvent::Error("pi prompt failed: No API key found for deepseek.".to_owned()),
                AgentEvent::TurnEnd {
                    reason: "error".to_owned()
                },
            ],
            "a rejected prompt ends the turn so the UI cannot stay busy"
        );
        assert_eq!(shared.take_pending("p1"), None, "response id is correlated");
    }

    #[test]
    fn provider_usage_maps_to_cost_once_per_value() {
        let jsonl = r#"{"type":"message_update","usage":{"input":100,"output":5,"cost":{"total":0.00123}},"assistantMessageEvent":{"type":"text_delta","contentIndex":0,"delta":"hi"}}
{"type":"message_update","usage":{"input":100,"output":5,"cost":{"total":0.00123}},"assistantMessageEvent":{"type":"text_delta","contentIndex":0,"delta":"!"}}
{"type":"message_update","usage":{"input":100,"output":9,"cost":{"total":0.002}},"assistantMessageEvent":{"type":"text_delta","contentIndex":0,"delta":"?"}}"#;
        assert_eq!(
            parse_all(jsonl),
            vec![
                AgentEvent::TextDelta("hi".to_owned()),
                AgentEvent::Cost {
                    input_tokens: Some(100),
                    output_tokens: Some(5),
                    cost_usd: Some(0.00123)
                },
                AgentEvent::TextDelta("!".to_owned()),
                AgentEvent::TextDelta("?".to_owned()),
                AgentEvent::Cost {
                    input_tokens: Some(100),
                    output_tokens: Some(9),
                    cost_usd: Some(0.002)
                },
            ]
        );
    }

    #[test]
    fn unknown_records_are_ignored_and_malformed_records_surface_an_error() {
        let shared = shared();
        let mut parser = ParserState::default();
        let unknown = parser.parse_line(
            br#"{"type":"some_future_record","payload":{"anything":true}}"#,
            &shared,
        );
        assert!(unknown.is_empty());

        let broken = parser.parse_line(b"{not json", &shared);
        assert_eq!(broken.len(), 1);
        assert!(matches!(broken[0], AgentEvent::Error(_)));
    }

    #[test]
    fn unicode_line_separators_stay_inside_the_json_value() {
        let record = serde_json::json!({
            "type": "message_update",
            "assistantMessageEvent": {
                "type": "text_delta",
                "contentIndex": 0,
                "delta": "a\u{2028}b\u{2029}c"
            }
        });
        let line = record.to_string();
        let events = parse_all(&line);
        assert_eq!(
            events,
            vec![AgentEvent::TextDelta("a\u{2028}b\u{2029}c".to_owned())]
        );
    }

    #[test]
    fn agent_end_without_settled_is_flushed_at_eof() {
        let jsonl = r#"{"type":"message_end","message":{"role":"assistant","stopReason":"stop"}}
{"type":"agent_end","willRetry":false}"#;
        assert_eq!(
            parse_all(jsonl),
            vec![AgentEvent::TurnEnd {
                reason: "completed".to_owned()
            }]
        );
    }

    #[test]
    fn retry_agent_end_is_not_a_turn_end_until_settled() {
        let jsonl = r#"{"type":"agent_end","willRetry":true}
{"type":"agent_start"}
{"type":"message_end","message":{"role":"assistant","stopReason":"stop"}}
{"type":"agent_end","willRetry":false}
{"type":"agent_settled","aborted":false}"#;
        assert_eq!(
            parse_all(jsonl),
            vec![AgentEvent::TurnEnd {
                reason: "completed".to_owned()
            }]
        );
    }
}
