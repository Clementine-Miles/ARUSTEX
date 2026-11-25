//! Providers that never touch the network: a deterministic mock and a scripted one.

use std::collections::VecDeque;
use std::sync::Mutex;

use async_trait::async_trait;
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

use crate::{
    ChatRequest, ChatResponse, ContentPart, DeltaSink, LlmError, LlmResult, Message, Provider,
    Role, StopReason, StreamDelta, Usage, emit, estimate_tokens,
};

/// Deterministic offline provider used for demonstrations and tests.
///
/// First turn: calls `summarize_scope` when offered. After tool results: calls
/// `submit_result` (with an input synthesised from its schema) when offered, otherwise
/// ends with a short text answer.
#[derive(Debug, Clone)]
pub struct MockProvider {
    name: String,
    model: String,
}

impl MockProvider {
    pub fn new(name: impl Into<String>, model: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            model: model.into(),
        }
    }
}

/// Builds a minimal value satisfying a JSON schema's required properties.
pub fn sample_for_schema(schema: &Value) -> Value {
    if let Some(values) = schema.get("enum").and_then(Value::as_array) {
        return values.first().cloned().unwrap_or(Value::Null);
    }
    if let Some(c) = schema.get("const") {
        return c.clone();
    }
    for key in ["oneOf", "anyOf"] {
        if let Some(first) = schema
            .get(key)
            .and_then(Value::as_array)
            .and_then(|a| a.first())
        {
            return sample_for_schema(first);
        }
    }
    let ty = match schema.get("type") {
        Some(Value::Array(types)) => types
            .iter()
            .filter_map(Value::as_str)
            .find(|t| *t != "null")
            .unwrap_or("null"),
        Some(Value::String(t)) => t.as_str(),
        _ if schema.get("properties").is_some() => "object",
        _ => "null",
    };
    match ty {
        "object" => {
            let mut out = serde_json::Map::new();
            let required: Vec<&str> = schema
                .get("required")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(Value::as_str)
                .collect();
            if let Some(props) = schema.get("properties").and_then(Value::as_object) {
                for (k, v) in props {
                    if required.contains(&k.as_str()) {
                        out.insert(k.clone(), sample_for_schema(v));
                    }
                }
            }
            Value::Object(out)
        }
        "array" => json!([]),
        "string" => json!("mock"),
        "integer" | "number" => json!(0),
        "boolean" => json!(false),
        _ => Value::Null,
    }
}

#[async_trait]
impl Provider for MockProvider {
    fn name(&self) -> &str {
        &self.name
    }
    fn model(&self) -> &str {
        &self.model
    }
    fn context_window(&self) -> u32 {
        200_000
    }

    async fn complete(
        &self,
        request: &ChatRequest,
        cancel: &CancellationToken,
        deltas: DeltaSink,
    ) -> LlmResult<ChatResponse> {
        if cancel.is_cancelled() {
            return Err(LlmError::Cancelled);
        }
        let turn = request
            .messages
            .iter()
            .filter(|m| m.role == Role::Assistant)
            .count();
        let after_tools = request
            .messages
            .last()
            .is_some_and(|m| m.role == Role::Tool);
        let tool = |name: &str| request.tools.iter().find(|t| t.name == name);
        let (content, stop) = if !after_tools && turn == 0 && tool("summarize_scope").is_some() {
            emit(
                &deltas,
                StreamDelta::ToolCall {
                    name: "summarize_scope".into(),
                },
            );
            (
                vec![ContentPart::ToolUse {
                    id: format!("mock_{turn}"),
                    name: "summarize_scope".into(),
                    input: json!({}),
                }],
                StopReason::ToolUse,
            )
        } else if let Some(submit) = tool("submit_result") {
            emit(
                &deltas,
                StreamDelta::ToolCall {
                    name: "submit_result".into(),
                },
            );
            (
                vec![ContentPart::ToolUse {
                    id: format!("mock_{turn}"),
                    name: submit.name.clone(),
                    input: sample_for_schema(&submit.input_schema),
                }],
                StopReason::ToolUse,
            )
        } else {
            let last = request
                .messages
                .iter()
                .rev()
                .find(|m| m.role == Role::User)
                .map(Message::text)
                .unwrap_or_default();
            let text = format!(
                "Mock response to: {}",
                last.chars().take(80).collect::<String>()
            );
            emit(&deltas, StreamDelta::Text(text.clone()));
            (vec![ContentPart::Text { text }], StopReason::EndTurn)
        };
        let message = Message {
            role: Role::Assistant,
            content,
        };
        let input_chars: usize = request.system.len()
            + request
                .messages
                .iter()
                .map(Message::approx_chars)
                .sum::<usize>();
        Ok(ChatResponse {
            usage: Usage {
                input_tokens: estimate_tokens(input_chars),
                output_tokens: estimate_tokens(message.approx_chars()),
            },
            message,
            stop_reason: stop,
            model: self.model.clone(),
        })
    }
}

/// Replays queued responses in order and records every request it receives.
#[derive(Debug, Default)]
pub struct ScriptedProvider {
    responses: Mutex<VecDeque<LlmResult<ChatResponse>>>,
    requests: Mutex<Vec<ChatRequest>>,
    context_window: u32,
}

impl ScriptedProvider {
    pub fn new(responses: impl IntoIterator<Item = LlmResult<ChatResponse>>) -> Self {
        Self {
            responses: Mutex::new(responses.into_iter().collect()),
            requests: Mutex::new(Vec::new()),
            context_window: 100_000,
        }
    }

    pub fn with_context_window(mut self, tokens: u32) -> Self {
        self.context_window = tokens;
        self
    }

    /// A response consisting of one tool call.
    pub fn tool_call(id: &str, name: &str, input: Value) -> LlmResult<ChatResponse> {
        Ok(ChatResponse {
            message: Message {
                role: Role::Assistant,
                content: vec![ContentPart::ToolUse {
                    id: id.into(),
                    name: name.into(),
                    input,
                }],
            },
            stop_reason: StopReason::ToolUse,
            usage: Usage {
                input_tokens: 100,
                output_tokens: 20,
            },
            model: "scripted".into(),
        })
    }

    /// A plain text response.
    pub fn text(text: &str) -> LlmResult<ChatResponse> {
        Ok(ChatResponse {
            message: Message::assistant(text),
            stop_reason: StopReason::EndTurn,
            usage: Usage {
                input_tokens: 100,
                output_tokens: 20,
            },
            model: "scripted".into(),
        })
    }

    pub fn requests(&self) -> Vec<ChatRequest> {
        self.requests.lock().expect("lock").clone()
    }

    pub fn remaining(&self) -> usize {
        self.responses.lock().expect("lock").len()
    }
}

#[async_trait]
impl Provider for ScriptedProvider {
    fn name(&self) -> &str {
        "scripted"
    }
    fn model(&self) -> &str {
        "scripted"
    }
    fn context_window(&self) -> u32 {
        self.context_window
    }

    async fn complete(
        &self,
        request: &ChatRequest,
        cancel: &CancellationToken,
        _deltas: DeltaSink,
    ) -> LlmResult<ChatResponse> {
        if cancel.is_cancelled() {
            return Err(LlmError::Cancelled);
        }
        self.requests.lock().expect("lock").push(request.clone());
        self.responses
            .lock()
            .expect("lock")
            .pop_front()
            .unwrap_or_else(|| Err(LlmError::Provider("script exhausted".into())))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ToolDefinition;

    #[test]
    fn schema_sampler_fills_required_fields() {
        let schema = json!({
            "type": "object",
            "required": ["a", "b", "c", "e"],
            "properties": {
                "a": {"type": "string"},
                "b": {"type": "array", "items": {"type": "integer"}},
                "c": {"enum": ["x", "y"]},
                "d": {"type": "string"},
                "e": {"type": ["integer", "null"]},
            }
        });
        assert_eq!(
            sample_for_schema(&schema),
            json!({"a": "mock", "b": [], "c": "x", "e": 0})
        );
    }

    #[tokio::test]
    async fn mock_calls_summary_then_submit() {
        let p = MockProvider::new("mock", "mock-1");
        let tools = vec![
            ToolDefinition {
                name: "summarize_scope".into(),
                description: String::new(),
                input_schema: json!({}),
            },
            ToolDefinition {
                name: "submit_result".into(),
                description: String::new(),
                input_schema: json!({"type": "object", "required": ["summary"], "properties": {"summary": {"type": "string"}}}),
            },
        ];
        let mut req = ChatRequest {
            system: "s".into(),
            messages: vec![Message::user("go")],
            tools,
            max_output_tokens: 100,
        };
        let cancel = CancellationToken::new();
        let r = p.complete(&req, &cancel, None).await.unwrap();
        assert_eq!(r.message.tool_uses().next().unwrap().1, "summarize_scope");
        req.messages.push(r.message);
        req.messages
            .push(Message::tool_result("mock_0", "{}", false));
        let r = p.complete(&req, &cancel, None).await.unwrap();
        let (_, name, input) = r.message.tool_uses().next().unwrap();
        assert_eq!(name, "submit_result");
        assert_eq!(input, &json!({"summary": "mock"}));
    }
}
