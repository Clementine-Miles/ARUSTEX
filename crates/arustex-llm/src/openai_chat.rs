//! OpenAI Chat Completions (and compatible endpoints).

use std::collections::BTreeMap;

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
pub struct OpenAiChatProvider {
    name: String,
    model: String,
    api_key: Option<Secret>,
    http: Transport,
    context_window: u32,
}

impl OpenAiChatProvider {
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

    fn official(&self) -> bool {
        self.http
            .settings
            .base_url
            .starts_with("https://api.openai.com")
    }

    pub(crate) fn body(&self, req: &ChatRequest) -> Value {
        let mut messages = Vec::new();
        if !req.system.is_empty() {
            messages.push(json!({"role": "system", "content": req.system}));
        }
        for m in &req.messages {
            messages.extend(wire_messages(m));
        }
        let mut body = json!({
            "model": self.model,
            "messages": messages,
            "stream": true,
            "stream_options": {"include_usage": true},
        });
        let key = if self.official() {
            "max_completion_tokens"
        } else {
            "max_tokens"
        };
        body[key] = json!(req.max_output_tokens);
        if !req.tools.is_empty() {
            body["tools"] = req
                .tools
                .iter()
                .map(|t| {
                    json!({"type": "function", "function": {
                        "name": t.name, "description": t.description, "parameters": t.input_schema,
                    }})
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

fn wire_messages(m: &Message) -> Vec<Value> {
    match m.role {
        Role::User => vec![json!({"role": "user", "content": m.text()})],
        Role::Tool => m
            .content
            .iter()
            .filter_map(|p| match p {
                ContentPart::ToolResult { tool_use_id, content, is_error } => Some(json!({
                    "role": "tool",
                    "tool_call_id": tool_use_id,
                    "content": if *is_error { format!("ERROR: {content}") } else { content.clone() },
                })),
                _ => None,
            })
            .collect(),
        Role::Assistant => {
            let text = m.text();
            let calls: Vec<Value> = m
                .tool_uses()
                .map(|(id, name, input)| {
                    json!({"id": id, "type": "function", "function": {"name": name, "arguments": input.to_string()}})
                })
                .collect();
            let mut v = json!({"role": "assistant", "content": if text.is_empty() { Value::Null } else { json!(text) }});
            if !calls.is_empty() {
                v["tool_calls"] = json!(calls);
            }
            vec![v]
        }
    }
}

#[derive(Default)]
struct Fold {
    text: String,
    calls: BTreeMap<u64, (String, String, String)>,
    finish: Option<String>,
    usage: Usage,
    model: String,
    done: bool,
}

impl Fold {
    fn apply(&mut self, data: &str, sink: &DeltaSink) -> LlmResult<bool> {
        if data.trim() == "[DONE]" {
            self.done = true;
            return Ok(false);
        }
        let v: Value =
            serde_json::from_str(data).map_err(|e| LlmError::Protocol(format!("chunk: {e}")))?;
        if let Some(err) = v.get("error") {
            let msg = err
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or("unknown error");
            return Err(LlmError::Provider(msg.chars().take(500).collect()));
        }
        if let Some(m) = v.get("model").and_then(Value::as_str) {
            self.model = m.to_owned();
        }
        if let Some(u) = v.get("usage").filter(|u| u.is_object()) {
            self.usage.input_tokens = u.get("prompt_tokens").and_then(Value::as_u64).unwrap_or(0);
            self.usage.output_tokens = u
                .get("completion_tokens")
                .and_then(Value::as_u64)
                .unwrap_or(0);
        }
        let Some(choice) = v.pointer("/choices/0") else {
            return Ok(true);
        };
        if let Some(text) = choice.pointer("/delta/content").and_then(Value::as_str)
            && !text.is_empty()
        {
            self.text.push_str(text);
            emit(sink, StreamDelta::Text(text.to_owned()));
        }
        if let Some(calls) = choice
            .pointer("/delta/tool_calls")
            .and_then(Value::as_array)
        {
            for (pos, call) in calls.iter().enumerate() {
                let index = call
                    .get("index")
                    .and_then(Value::as_u64)
                    .unwrap_or(pos as u64);
                let entry = self.calls.entry(index).or_default();
                if let Some(id) = call.get("id").and_then(Value::as_str) {
                    entry.0 = id.to_owned();
                }
                if let Some(name) = call.pointer("/function/name").and_then(Value::as_str) {
                    entry.1.push_str(name);
                    emit(
                        sink,
                        StreamDelta::ToolCall {
                            name: name.to_owned(),
                        },
                    );
                }
                if let Some(args) = call.pointer("/function/arguments").and_then(Value::as_str) {
                    entry.2.push_str(args);
                }
            }
        }
        if let Some(reason) = choice.get("finish_reason").and_then(Value::as_str) {
            self.finish = Some(reason.to_owned());
        }
        Ok(true)
    }

    fn finish(self, fallback_model: &str) -> LlmResult<ChatResponse> {
        if !self.done && self.finish.is_none() {
            return Err(LlmError::Protocol("stream ended before completion".into()));
        }
        let mut content = Vec::new();
        if !self.text.is_empty() {
            content.push(ContentPart::Text { text: self.text });
        }
        let has_calls = !self.calls.is_empty();
        for (i, (id, name, args)) in self.calls {
            let input = if args.trim().is_empty() {
                json!({})
            } else {
                serde_json::from_str(&args).unwrap_or(Value::String(args))
            };
            let id = if id.is_empty() {
                format!("call_{i}")
            } else {
                id
            };
            content.push(ContentPart::ToolUse { id, name, input });
        }
        let stop_reason = match self.finish.as_deref() {
            _ if has_calls => StopReason::ToolUse,
            Some("stop") => StopReason::EndTurn,
            Some("length") => StopReason::MaxTokens,
            Some("tool_calls") => StopReason::ToolUse,
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
impl Provider for OpenAiChatProvider {
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
                    .post("chat/completions", &headers, &body, cancel)
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
