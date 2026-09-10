//! Claude Code `--output-format stream-json` normalisation.

use std::collections::HashMap;

use ethogram::{
    AGENT_COMPLETED, AGENT_STARTED, AGENT_TEXT, AGENT_TOOL_RESULT, AGENT_TOOL_USE, AGENT_WARNING,
    AgentCompletedPayload, AgentStartedPayload, AgentTextPayload, AgentToolResultPayload,
    AgentToolUsePayload, AgentWarningPayload, EventDraft, MAX_EXCERPT_SCALARS, MAX_TEXT_SCALARS,
    PayloadExtension, RunUsage, excerpt,
};
use indexmap::IndexMap;
use serde::{Deserialize, Serialize};

use crate::{CaptureFault, Normaliser};

/// Stateful normaliser for Claude Code's stream-JSON output.
#[derive(Debug, Default)]
pub struct ClaudeNormaliser {
    line: u64,
    session_id: Option<String>,
    tool_names: HashMap<String, String>,
}

impl ClaudeNormaliser {
    /// Construct an empty Claude Code stream normaliser.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    fn handle_system(&mut self, frame: RawFrame) -> Result<Vec<EventDraft>, CaptureFault> {
        let subtype = required(frame.subtype, self.line, "system.subtype")?;
        match subtype.as_str() {
            "init" => {
                let session_id = required(frame.session_id, self.line, "system.session_id")?;
                let model = required(frame.model, self.line, "system.model")?;
                self.session_id = Some(session_id.clone());
                Ok(vec![draft(
                    AGENT_STARTED,
                    AgentStartedPayload {
                        stage: None,
                        model: Some(model),
                        session_id: Some(session_id),
                        pid: None,
                        extra: PayloadExtension::new(),
                    },
                )])
            }
            // Named omissions: these system frames report progress telemetry,
            // not agent behaviour, so they deliberately produce no draft.
            "thinking_tokens"
            | "background_tasks_changed"
            | "task_started"
            | "task_progress"
            | "task_updated"
            | "task_notification" => Ok(Vec::new()),
            _ => Err(rejected(format!(
                "unknown Claude system subtype {subtype:?} at line {}",
                self.line
            ))),
        }
    }

    fn handle_assistant(&mut self, frame: RawFrame) -> Result<Vec<EventDraft>, CaptureFault> {
        let parent_tool_use_id = frame.parent_tool_use_id;
        let content = required_message_content(frame.message, self.line, "assistant")?;
        let mut drafts = Vec::new();

        for block in content {
            match block.block_type.as_str() {
                "text" => {
                    let text = required(block.text, self.line, "assistant text.text")?;
                    let excerpt = excerpt(&text, MAX_TEXT_SCALARS);
                    drafts.push(draft(
                        AGENT_TEXT,
                        AgentTextPayload {
                            stage: None,
                            text: excerpt.text,
                            truncated: Some(excerpt.truncated),
                            parent_tool_use_id: parent_tool_use_id.clone(),
                            extra: PayloadExtension::new(),
                        },
                    ));
                }
                "tool_use" => {
                    let tool_use_id = required(block.id, self.line, "assistant tool_use.id")?;
                    let tool = required(block.name, self.line, "assistant tool_use.name")?;
                    let input = required(block.input, self.line, "assistant tool_use.input")?;
                    let input = compact_json(&input, self.line, "tool input")?;
                    let excerpt = excerpt(&input, MAX_EXCERPT_SCALARS);

                    if self
                        .tool_names
                        .insert(tool_use_id.clone(), tool.clone())
                        .is_some()
                    {
                        return Err(rejected(format!(
                            "duplicate Claude tool_use id {tool_use_id:?} at line {}",
                            self.line
                        )));
                    }

                    drafts.push(draft(
                        AGENT_TOOL_USE,
                        AgentToolUsePayload {
                            stage: None,
                            tool,
                            input_excerpt: Some(excerpt.text),
                            truncated: Some(excerpt.truncated),
                            tool_use_id: Some(tool_use_id),
                            parent_tool_use_id: parent_tool_use_id.clone(),
                            extra: PayloadExtension::new(),
                        },
                    ));
                }
                // Deliberate information loss: AgentTextPayload cannot say
                // whether text is reasoning or output. Emitting this as
                // agent.text would mislabel internal reasoning as something
                // the model said. Revisit if ethogram gains that distinction;
                // chreode's matching omission is corroboration, not authority.
                "thinking" => {}
                other => {
                    return Err(rejected(format!(
                        "unknown Claude assistant content type {other:?} at line {}",
                        self.line
                    )));
                }
            }
        }

        Ok(drafts)
    }

    fn handle_user(&mut self, frame: RawFrame) -> Result<Vec<EventDraft>, CaptureFault> {
        let parent_tool_use_id = frame.parent_tool_use_id;
        let content = required_message_content(frame.message, self.line, "user")?;
        let mut drafts = Vec::new();

        for block in content {
            if block.block_type != "tool_result" {
                return Err(rejected(format!(
                    "unknown Claude user content type {:?} at line {}",
                    block.block_type, self.line
                )));
            }

            let tool_use_id =
                required(block.tool_use_id, self.line, "user tool_result.tool_use_id")?;
            let tool = self.tool_names.remove(&tool_use_id).ok_or_else(|| {
                rejected(format!(
                    "Claude tool_result references unknown tool_use id {tool_use_id:?} at line {}",
                    self.line
                ))
            })?;
            let content = required(block.content, self.line, "user tool_result.content")?;
            let result = result_text(&content, self.line)?;
            let excerpt = excerpt(&result, MAX_EXCERPT_SCALARS);

            drafts.push(draft(
                AGENT_TOOL_RESULT,
                AgentToolResultPayload {
                    stage: None,
                    tool,
                    is_error: block.is_error,
                    result_excerpt: Some(excerpt.text),
                    truncated: Some(excerpt.truncated),
                    tool_use_id: Some(tool_use_id),
                    parent_tool_use_id: parent_tool_use_id.clone(),
                    extra: PayloadExtension::new(),
                },
            ));
        }

        Ok(drafts)
    }

    fn handle_result(&self, frame: RawFrame) -> Result<Vec<EventDraft>, CaptureFault> {
        let subtype = required(frame.subtype, self.line, "result.subtype")?;

        let session_id = self.session_id.clone().ok_or_else(|| {
            rejected(format!(
                "Claude result appeared before system/init at line {}",
                self.line
            ))
        })?;
        let reported_session_id = required(frame.session_id, self.line, "result.session_id")?;
        if reported_session_id != session_id {
            return Err(rejected(format!(
                "Claude result session {reported_session_id:?} does not match active session {session_id:?} at line {}",
                self.line
            )));
        }

        // A capped or errored invocation still spent real money, tokens, turns,
        // and time. Production does not retain the raw stream, so refusing an
        // unfamiliar subtype would discard exactly the usage that ceilings are
        // meant to bound. The four totals are the guard against guessing: when
        // present they are facts we can report, while the subtype is only a
        // label carried through verbatim and never interpreted here.
        let cost_usd = required(frame.total_cost_usd, self.line, "result.total_cost_usd")?;
        let usage = required(frame.usage, self.line, "result.usage")?;
        let turns = required(frame.num_turns, self.line, "result.num_turns")?;
        let duration_ms = required(frame.duration_ms, self.line, "result.duration_ms")?;
        let mut drafts = vec![draft(
            AGENT_COMPLETED,
            AgentCompletedPayload {
                stage: None,
                turns: Some(turns),
                session_id: Some(session_id),
                cost_usd: Some(cost_usd),
                model: None,
                usage: Some(RunUsage {
                    input_tokens: Some(required(
                        usage.input_tokens,
                        self.line,
                        "result.usage.input_tokens",
                    )?),
                    output_tokens: Some(required(
                        usage.output_tokens,
                        self.line,
                        "result.usage.output_tokens",
                    )?),
                    cache_read_tokens: Some(required(
                        usage.cache_read_input_tokens,
                        self.line,
                        "result.usage.cache_read_input_tokens",
                    )?),
                    cache_creation_tokens: Some(required(
                        usage.cache_creation_input_tokens,
                        self.line,
                        "result.usage.cache_creation_input_tokens",
                    )?),
                    // Ethogram defines an absent unit as tokens.
                    unit: None,
                    extra: PayloadExtension::new(),
                }),
                duration_ms: Some(duration_ms),
                estimated: None,
                extra: PayloadExtension::new(),
            },
        )];

        if subtype != "success" {
            drafts.push(draft(
                AGENT_WARNING,
                AgentWarningPayload {
                    stage: None,
                    message: format!("claude result subtype \"{subtype}\""),
                    extra: PayloadExtension::new(),
                },
            ));
        }

        Ok(drafts)
    }
}

impl Normaliser for ClaudeNormaliser {
    fn line(&mut self, raw: &str) -> Result<Vec<EventDraft>, CaptureFault> {
        self.line = self.line.checked_add(1).ok_or_else(|| {
            rejected("Claude stream contains more than u64::MAX lines".to_owned())
        })?;
        let frame: RawFrame =
            serde_json::from_str(raw).map_err(|error| CaptureFault::MalformedLine {
                line: self.line,
                reason: error.to_string(),
            })?;

        match frame.frame_type.as_str() {
            "system" => self.handle_system(frame),
            "assistant" => self.handle_assistant(frame),
            "user" => self.handle_user(frame),
            "result" => self.handle_result(frame),
            // Named omission: this describes account rate-limit state, not
            // this run. It is informational and therefore not agent.warning.
            "rate_limit_event" => Ok(Vec::new()),
            other => Err(rejected(format!(
                "unknown Claude frame type {other:?} at line {}",
                self.line
            ))),
        }
    }

    fn finish(&mut self) -> Result<Vec<EventDraft>, CaptureFault> {
        // EOF carries no implied terminal state. A capped capture may end
        // without a result frame, and synthesising agent.completed would lie.
        Ok(Vec::new())
    }
}

#[derive(Debug, Deserialize)]
struct RawFrame {
    #[serde(rename = "type")]
    frame_type: String,
    #[serde(default)]
    subtype: Option<String>,
    #[serde(default)]
    session_id: Option<String>,
    #[serde(default)]
    parent_tool_use_id: Option<String>,
    #[serde(default)]
    model: Option<String>,
    #[serde(default)]
    message: Option<RawMessage>,
    #[serde(default)]
    duration_ms: Option<u64>,
    #[serde(default)]
    num_turns: Option<u64>,
    #[serde(default)]
    total_cost_usd: Option<f64>,
    #[serde(default)]
    usage: Option<RawUsage>,
}

#[derive(Debug, Deserialize)]
struct RawMessage {
    content: Vec<RawContentBlock>,
}

#[derive(Debug, Deserialize)]
struct RawContentBlock {
    #[serde(rename = "type")]
    block_type: String,
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    input: Option<OrderedValue>,
    #[serde(default)]
    text: Option<String>,
    #[serde(default)]
    tool_use_id: Option<String>,
    #[serde(default)]
    content: Option<OrderedValue>,
    #[serde(default)]
    is_error: Option<bool>,
}

#[derive(Debug, Deserialize)]
struct RawUsage {
    #[serde(default)]
    input_tokens: Option<u64>,
    #[serde(default)]
    output_tokens: Option<u64>,
    #[serde(default)]
    cache_read_input_tokens: Option<u64>,
    #[serde(default)]
    cache_creation_input_tokens: Option<u64>,
}

/// JSON value retaining object insertion order without enabling
/// `serde_json/preserve_order` across the workspace dependency graph.
#[derive(Debug, Deserialize, Serialize)]
#[serde(untagged)]
enum OrderedValue {
    Null(()),
    Bool(bool),
    Number(serde_json::Number),
    String(String),
    Array(Vec<OrderedValue>),
    Object(IndexMap<String, OrderedValue>),
}

fn required<T>(value: Option<T>, line: u64, field: &str) -> Result<T, CaptureFault> {
    value.ok_or_else(|| CaptureFault::MalformedLine {
        line,
        reason: format!("missing required field {field}"),
    })
}

fn required_message_content(
    message: Option<RawMessage>,
    line: u64,
    frame_type: &str,
) -> Result<Vec<RawContentBlock>, CaptureFault> {
    required(message, line, &format!("{frame_type}.message")).map(|message| message.content)
}

fn compact_json(value: &OrderedValue, line: u64, field: &str) -> Result<String, CaptureFault> {
    serde_json::to_string(value).map_err(|error| CaptureFault::MalformedLine {
        line,
        reason: format!("cannot serialise {field}: {error}"),
    })
}

fn result_text(value: &OrderedValue, line: u64) -> Result<String, CaptureFault> {
    match value {
        OrderedValue::String(text) => Ok(text.clone()),
        OrderedValue::Array(blocks) => {
            let text: Vec<&str> = blocks
                .iter()
                .filter_map(|block| match block {
                    OrderedValue::Object(fields) => match fields.get("text") {
                        Some(OrderedValue::String(text)) => Some(text.as_str()),
                        _ => None,
                    },
                    _ => None,
                })
                .collect();
            if text.is_empty() {
                compact_json(value, line, "structured tool result")
            } else {
                Ok(text.join("\n"))
            }
        }
        _ => compact_json(value, line, "structured tool result"),
    }
}

fn draft<P>(event_type: &str, payload: P) -> EventDraft
where
    P: Serialize,
{
    EventDraft {
        event_type: event_type.to_owned(),
        payload: serde_json::to_value(payload).expect("ethogram payload serialises"),
        captured_at: None,
    }
}

fn rejected(reason: String) -> CaptureFault {
    CaptureFault::NormaliserRejected { reason }
}
