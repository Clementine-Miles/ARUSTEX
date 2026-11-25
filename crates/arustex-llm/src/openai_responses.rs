//! OpenAI Responses API (stateless: `store` is disabled and history is resent).

use arustex_core::Secret;
use async_trait::async_trait;
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

use crate::http::{HttpSettings, Transport};
use crate::{
    ChatRequest, ChatResponse, ContentPart, DeltaSink, LlmError, LlmResult, Message, Provider,
    Role, StopReason, StreamDelta, Usage, emit,
};

#[derive(Debug)]
pub struct OpenAiResponsesProvider {
    name: String,
    model: String,
    api_key: Option<Secret>,
    http: Transport,
    context_window: u32,
}

impl OpenAiResponsesProvider {
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
        let mut input = Vec::new();
        for m in &req.messages {
            for part in &m.content {
                match (m.role, part) {
                    (Role::User, ContentPart::Text { text }) => input.push(json!({
                        "type": "message", "role": "user", "content": [{"type": "input_text", "text": text}],
                    })),
                    (Role::Assistant, ContentPart::Text { text }) => input.push(json!({
                        "type": "message", "role": "assistant", "content": [{"type": "output_text", "text": text}],
                    })),
                    (_, ContentPart::ToolUse { id, name, input: args }) => input.push(json!({
                        "type": "function_call", "call_id": id, "name": name, "arguments": args.to_string(),
                    })),
                    (_, ContentPart::ToolResult { tool_use_id, content, is_error }) => input.push(json!({
                        "type": "function_call_output",
                        "call_id": tool_use_id,
                        "output": if *is_error { format!("ERROR: {content}") } else { content.clone() },
                    })),
                    (Role::Tool, ContentPart::Text { text }) => input.push(json!({
                        "type": "message", "role": "user", "content": [{"type": "input_text", "text": text}],
                    })),
                }
            }
        }
        let mut body = json!({
            "model": self.model,
            "input": input,
            "max_output_tokens": req.max_output_tokens,
            "stream": true,
            "store": false,
        });
        if !req.system.is_empty() {
            body["instructions"] = json!(req.system);
        }
        if !req.tools.is_empty() {
            body["tools"] = req
                .tools
                .iter()
                .map(|t| {
                    json!({"type": "function", "name": t.name, "description": t.description,
                           "parameters": t.input_schema, "strict": false})
                })
                .collect();
        }
        body
    }

    fn headers(&self) -> Vec<(&'static str, String)> {
        match &self.api_key {
            Some(k) if !k.is_empty() => vec![("authorization", format!("Bearer {}", k.expose()))],
            _ => Vec::new(),
        }
    }
}

#[derive(Default)]
struct Fold {
    completed: Option<Value>,
}

impl Fold {
    fn apply(&mut self, event: Option<&str>, data: &str, sink: &DeltaSink) -> LlmResult<bool> {
        if data.trim() == "[DONE]" {
            return Ok(false);
        }
        let v: Value =
            serde_json::from_str(data).map_err(|e| LlmError::Protocol(format!("event: {e}")))?;
        let kind = v
            .get("type")
            .and_then(Value::as_str)
            .or(event)
            .unwrap_or_default()
            .to_owned();
        match kind.as_str() {
            "response.output_text.delta" => {
                if let Some(d) = v.get("delta").and_then(Value::as_str) {
                    emit(sink, StreamDelta::Text(d.to_owned()));
                }
            }
            "response.output_item.added" => {
                if v.pointer("/item/type").and_then(Value::as_str) == Some("function_call") {
                    let name = v
                        .pointer("/item/name")
                        .and_then(Value::as_str)
                        .unwrap_or_default();
                    emit(
                        sink,
                        StreamDelta::ToolCall {
                            name: name.to_owned(),
                        },
                    );
                }
            }
            "response.completed" | "response.incomplete" => {
                self.completed = v.get("response").cloned();
                return Ok(false);
            }
            "response.failed" | "error" => {
                let msg = v
                    .pointer("/response/error/message")
                    .or_else(|| v.pointer("/error/message"))
                    .or_else(|| v.get("message"))
                    .and_then(Value::as_str)
                    .unwrap_or("unknown error");
                return Err(LlmError::Provider(msg.chars().take(500).collect()));
            }
            _ => {}
        }
        Ok(true)
    }

    fn finish(self, fallback_model: &str) -> LlmResult<ChatResponse> {
        let resp = self
            .completed
            .ok_or_else(|| LlmError::Protocol("stream ended before completion".into()))?;
        let mut content = Vec::new();
        for item in resp
            .get("output")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            match item.get("type").and_then(Value::as_str) {
                Some("message") => {
                    for part in item
                        .get("content")
                        .and_then(Value::as_array)
                        .into_iter()
                        .flatten()
                    {
                        if part.get("type").and_then(Value::as_str) == Some("output_text") {
                            let text = part.get("text").and_then(Value::as_str).unwrap_or_default();
                            if !text.is_empty() {
                                content.push(ContentPart::Text {
                                    text: text.to_owned(),
                                });
                            }
                        }
                    }
                }
                Some("function_call") => {
                    let args = item
                        .get("arguments")
                        .and_then(Value::as_str)
                        .unwrap_or_default();
                    let input = if args.trim().is_empty() {
                        json!({})
                    } else {
                        serde_json::from_str(args).unwrap_or(Value::String(args.to_owned()))
                    };
                    content.push(ContentPart::ToolUse {
                        id: item
                            .get("call_id")
                            .and_then(Value::as_str)
                            .unwrap_or_default()
                            .to_owned(),
                        name: item
                            .get("name")
                            .and_then(Value::as_str)
                            .unwrap_or_default()
                            .to_owned(),
                        input,
                    });
                }
                _ => {}
            }
        }
        let has_calls = content
            .iter()
            .any(|c| matches!(c, ContentPart::ToolUse { .. }));
        let incomplete = resp
            .pointer("/incomplete_details/reason")
            .and_then(Value::as_str);
        let stop_reason = match (resp.get("status").and_then(Value::as_str), incomplete) {
            _ if has_calls => StopReason::ToolUse,
            (Some("incomplete"), Some("max_output_tokens")) => StopReason::MaxTokens,
            (Some("completed"), _) => StopReason::EndTurn,
            _ => StopReason::Other,
        };
        let usage = Usage {
            input_tokens: resp
                .pointer("/usage/input_tokens")
                .and_then(Value::as_u64)
                .unwrap_or(0),
            output_tokens: resp
                .pointer("/usage/output_tokens")
                .and_then(Value::as_u64)
                .unwrap_or(0),
        };
        let model = resp
            .get("model")
            .and_then(Value::as_str)
            .unwrap_or(fallback_model)
            .to_owned();
        Ok(ChatResponse {
            message: Message {
                role: Role::Assistant,
                content,
            },
            stop_reason,
            usage,
            model,
        })
    }
}

#[async_trait]
impl Provider for OpenAiResponsesProvider {
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
                let resp = self.http.post("responses", &headers, &body, cancel).await?;
                let mut fold = Fold::default();
                self.http
                    .for_each_event(resp, |ev| {
                        fold.apply(ev.event.as_deref(), &ev.data, &deltas)
                    })
                    .await?;
                fold.finish(&self.model)
            })
            .await
    }
}
