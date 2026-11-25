//! Anthropic Messages API.

use std::collections::BTreeMap;

use arustex_core::Secret;
use async_trait::async_trait;
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

use crate::http::{HttpSettings, Transport};
use crate::{
    ChatRequest, ChatResponse, ContentPart, DeltaSink, LlmError, LlmResult, Message, Provider,
    Role, StopReason, StreamDelta, Usage, coalesce, emit,
};

const API_VERSION: &str = "2023-06-01";

#[derive(Debug)]
pub struct AnthropicProvider {
    name: String,
    model: String,
    api_key: Option<Secret>,
    http: Transport,
    context_window: u32,
}

impl AnthropicProvider {
    pub fn new(
        name: impl Into<String>,
        model: impl Into<String>,
        api_key: Option<Secret>,
        settings: HttpSettings,
        context_window: u32,
    ) -> LlmResult<Self> {
        Ok(Self {
            name: name.into(),
            model: model.into(),
            api_key,
            http: Transport::new(settings)?,
            context_window,
        })
    }

    pub(crate) fn body(&self, req: &ChatRequest) -> Value {
        let messages: Vec<Value> = coalesce(&req.messages, true)
            .iter()
            .map(|m| {
                let role = if m.role == Role::Assistant { "assistant" } else { "user" };
                let content: Vec<Value> = m
                    .content
                    .iter()
                    .map(|p| match p {
                        ContentPart::Text { text } => json!({"type": "text", "text": text}),
                        ContentPart::ToolUse { id, name, input } => {
                            json!({"type": "tool_use", "id": id, "name": name, "input": input})
                        }
                        ContentPart::ToolResult { tool_use_id, content, is_error } => json!({
                            "type": "tool_result", "tool_use_id": tool_use_id, "content": content, "is_error": is_error,
                        }),
                    })
                    .collect();
                json!({"role": role, "content": content})
            })
            .collect();
        let mut body = json!({
            "model": self.model,
            "max_tokens": req.max_output_tokens,
            "messages": messages,
            "stream": true,
        });
        if !req.system.is_empty() {
            body["system"] = json!(req.system);
        }
        if !req.tools.is_empty() {
            body["tools"] = req
                .tools
                .iter()
                .map(|t| json!({"name": t.name, "description": t.description, "input_schema": t.input_schema}))
                .collect();
        }
        body
    }

    fn headers(&self) -> Vec<(&'static str, String)> {
        let mut h = vec![("anthropic-version", API_VERSION.to_owned())];
        if let Some(k) = self.api_key.as_ref().filter(|k| !k.is_empty()) {
            h.push(("x-api-key", k.expose().to_owned()));
        }
        h
    }
}

enum Block {
    Text(String),
    Tool {
        id: String,
        name: String,
        json: String,
    },
}

#[derive(Default)]
struct Fold {
    blocks: BTreeMap<u64, Block>,
    stop: Option<String>,
    usage: Usage,
    model: String,
    done: bool,
}

impl Fold {
    fn apply(&mut self, data: &str, sink: &DeltaSink) -> LlmResult<bool> {
        let v: Value =
            serde_json::from_str(data).map_err(|e| LlmError::Protocol(format!("event: {e}")))?;
        let index = v.get("index").and_then(Value::as_u64).unwrap_or(0);
        match v.get("type").and_then(Value::as_str).unwrap_or_default() {
            "message_start" => {
                if let Some(m) = v.pointer("/message/model").and_then(Value::as_str) {
                    self.model = m.to_owned();
                }
                self.usage.input_tokens = v
                    .pointer("/message/usage/input_tokens")
                    .and_then(Value::as_u64)
                    .unwrap_or(0);
            }
            "content_block_start" => {
                let block = &v["content_block"];
                match block.get("type").and_then(Value::as_str) {
                    Some("tool_use") => {
                        let name = block
                            .get("name")
                            .and_then(Value::as_str)
                            .unwrap_or_default()
                            .to_owned();
                        emit(sink, StreamDelta::ToolCall { name: name.clone() });
                        self.blocks.insert(
                            index,
                            Block::Tool {
                                id: block
                                    .get("id")
                                    .and_then(Value::as_str)
                                    .unwrap_or_default()
                                    .to_owned(),
                                name,
                                json: String::new(),
                            },
                        );
                    }
                    _ => {
                        self.blocks.insert(index, Block::Text(String::new()));
                    }
                }
            }
            "content_block_delta" => match (
                self.blocks.get_mut(&index),
                v.pointer("/delta/type").and_then(Value::as_str),
            ) {
                (Some(Block::Text(t)), Some("text_delta")) => {
                    let piece = v
                        .pointer("/delta/text")
                        .and_then(Value::as_str)
                        .unwrap_or_default();
                    t.push_str(piece);
                    emit(sink, StreamDelta::Text(piece.to_owned()));
                }
                (Some(Block::Tool { json, .. }), Some("input_json_delta")) => {
                    json.push_str(
                        v.pointer("/delta/partial_json")
                            .and_then(Value::as_str)
                            .unwrap_or_default(),
                    );
                }
                _ => {}
            },
            "message_delta" => {
                if let Some(s) = v.pointer("/delta/stop_reason").and_then(Value::as_str) {
                    self.stop = Some(s.to_owned());
                }
                if let Some(o) = v.pointer("/usage/output_tokens").and_then(Value::as_u64) {
                    self.usage.output_tokens = o;
                }
            }
            "message_stop" => {
                self.done = true;
                return Ok(false);
            }
            "error" => {
                let msg = v
                    .pointer("/error/message")
                    .and_then(Value::as_str)
                    .unwrap_or("unknown error");
                return Err(LlmError::Provider(msg.chars().take(500).collect()));
            }
            _ => {}
        }
        Ok(true)
    }

    fn finish(self, fallback_model: &str) -> LlmResult<ChatResponse> {
        if !self.done {
            return Err(LlmError::Protocol(
                "stream ended before message_stop".into(),
            ));
        }
        let content = self
            .blocks
            .into_values()
            .filter_map(|b| match b {
                Block::Text(text) if text.is_empty() => None,
                Block::Text(text) => Some(ContentPart::Text { text }),
                Block::Tool { id, name, json } => {
                    let input = if json.trim().is_empty() {
                        json!({})
                    } else {
                        serde_json::from_str(&json).unwrap_or(Value::String(json))
                    };
                    Some(ContentPart::ToolUse { id, name, input })
                }
            })
            .collect();
        let stop_reason = match self.stop.as_deref() {
            Some("end_turn" | "stop_sequence") => StopReason::EndTurn,
            Some("tool_use") => StopReason::ToolUse,
            Some("max_tokens") => StopReason::MaxTokens,
            _ => StopReason::Other,
        };
        Ok(ChatResponse {
            message: Message {
                role: Role::Assistant,
                content,
            },
            stop_reason,
            usage: self.usage,
            model: if self.model.is_empty() {
                fallback_model.to_owned()
            } else {
                self.model
            },
        })
    }
}

#[async_trait]
impl Provider for AnthropicProvider {
    fn name(&self) -> &str {
        &self.name
    }
    fn model(&self) -> &str {
        &self.model
    }
    fn context_window(&self) -> u32 {
        self.context_window
    }

    async fn complete(
        &self,
        request: &ChatRequest,
        cancel: &CancellationToken,
        deltas: DeltaSink,
    ) -> LlmResult<ChatResponse> {
        let body = self.body(request);
        let headers = self.headers();
        self.http
            .run(cancel, async {
                let resp = self
                    .http
                    .post("v1/messages", &headers, &body, cancel)
                    .await?;
                let mut fold = Fold::default();
                self.http
                    .for_each_event(resp, |ev| fold.apply(&ev.data, &deltas))
                    .await?;
                fold.finish(&self.model)
            })
            .await
    }
}
