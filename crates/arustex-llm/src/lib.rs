//! Model provider adapters.
//!
//! Providers are constructed only from operator configuration; nothing in a workflow
//! can choose an endpoint. Every adapter streams, honours a request timeout and a
//! cancellation token, retries transient failures with backoff and respects a
//! per-provider request rate.

mod anthropic;
mod http;
mod mock;
mod openai_chat;
mod openai_responses;
mod router;
mod sse;

use std::time::Duration;

use arustex_core::CoreError;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

pub use anthropic::AnthropicProvider;
pub use http::{HttpSettings, RateLimiter};
pub use mock::{MockProvider, ScriptedProvider};
pub use openai_chat::OpenAiChatProvider;
pub use openai_responses::OpenAiResponsesProvider;
pub use router::{ProviderKind, ProviderRouter, ProviderSettings, build_provider};
pub use sse::{SseEvent, SseParser};

/// Who wrote a message.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    User,
    Assistant,
    /// Results of operations, sent back to the model.
    Tool,
}

/// One part of a message.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ContentPart {
    Text {
        text: String,
    },
    ToolUse {
        id: String,
        name: String,
        input: serde_json::Value,
    },
    ToolResult {
        tool_use_id: String,
        content: String,
        is_error: bool,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Message {
    pub role: Role,
    pub content: Vec<ContentPart>,
}

impl Message {
    pub fn user(text: impl Into<String>) -> Self {
        Self {
            role: Role::User,
            content: vec![ContentPart::Text { text: text.into() }],
        }
    }

    pub fn assistant(text: impl Into<String>) -> Self {
        Self {
            role: Role::Assistant,
            content: vec![ContentPart::Text { text: text.into() }],
        }
    }

    pub fn tool_result(
        tool_use_id: impl Into<String>,
        content: impl Into<String>,
        is_error: bool,
    ) -> Self {
        Self {
            role: Role::Tool,
            content: vec![ContentPart::ToolResult {
                tool_use_id: tool_use_id.into(),
                content: content.into(),
                is_error,
            }],
        }
    }

    /// Concatenated text parts.
    pub fn text(&self) -> String {
        self.content
            .iter()
            .filter_map(|p| match p {
                ContentPart::Text { text } => Some(text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("")
    }

    pub fn tool_uses(&self) -> impl Iterator<Item = (&str, &str, &serde_json::Value)> {
        self.content.iter().filter_map(|p| match p {
            ContentPart::ToolUse { id, name, input } => Some((id.as_str(), name.as_str(), input)),
            _ => None,
        })
    }

    /// Rough size used for context budgeting.
    pub fn approx_chars(&self) -> usize {
        self.content
            .iter()
            .map(|p| match p {
                ContentPart::Text { text } => text.len(),
                ContentPart::ToolUse { name, input, .. } => name.len() + input.to_string().len(),
                ContentPart::ToolResult { content, .. } => content.len(),
            })
            .sum()
    }
}

/// A tool the model may call.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolDefinition {
    pub name: String,
    pub description: String,
    pub input_schema: serde_json::Value,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ChatRequest {
    pub system: String,
    pub messages: Vec<Message>,
    pub tools: Vec<ToolDefinition>,
    pub max_output_tokens: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StopReason {
    EndTurn,
    ToolUse,
    MaxTokens,
    Other,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Usage {
    pub input_tokens: u64,
    pub output_tokens: u64,
}

impl Usage {
    pub fn total(&self) -> u64 {
        self.input_tokens + self.output_tokens
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ChatResponse {
    pub message: Message,
    pub stop_reason: StopReason,
    pub usage: Usage,
    pub model: String,
}

/// Incremental output for live display. The final [`ChatResponse`] is authoritative;
/// deltas are dropped rather than slowing the provider when the receiver lags.
#[derive(Debug, Clone, PartialEq)]
pub enum StreamDelta {
    Text(String),
    ToolCall { name: String },
}

/// A channel for [`StreamDelta`]s. `None` disables live output.
pub type DeltaSink = Option<mpsc::Sender<StreamDelta>>;

pub(crate) fn emit(sink: &DeltaSink, delta: StreamDelta) {
    if let Some(tx) = sink {
        let _ = tx.try_send(delta);
    }
}

#[derive(Debug, thiserror::Error)]
pub enum LlmError {
    #[error("request cancelled")]
    Cancelled,
    #[error("request timed out after {0:?}")]
    Timeout(Duration),
    #[error("provider returned HTTP {status}: {message}")]
    Http {
        status: u16,
        message: String,
        retry_after: Option<Duration>,
    },
    #[error("transport error: {0}")]
    Transport(String),
    #[error("malformed provider response: {0}")]
    Protocol(String),
    #[error("provider error: {0}")]
    Provider(String),
    #[error("configuration error: {0}")]
    Config(String),
}

impl LlmError {
    /// Whether a fresh attempt may succeed.
    pub fn is_retryable(&self) -> bool {
        match self {
            Self::Http { status, .. } => {
                *status == 408 || *status == 409 || *status == 429 || *status >= 500
            }
            Self::Transport(_) | Self::Timeout(_) => true,
            _ => false,
        }
    }
}

impl From<LlmError> for CoreError {
    fn from(value: LlmError) -> Self {
        match value {
            LlmError::Cancelled => CoreError::Cancelled,
            LlmError::Timeout(d) => {
                CoreError::Timeout(format!("model request timed out after {d:?}"))
            }
            LlmError::Config(m) => CoreError::invalid(m),
            other => CoreError::Provider(other.to_string()),
        }
    }
}

pub type LlmResult<T> = Result<T, LlmError>;

/// A configured model endpoint.
#[async_trait]
pub trait Provider: Send + Sync + std::fmt::Debug {
    fn name(&self) -> &str;
    fn model(&self) -> &str;
    /// Largest context the model accepts, in tokens.
    fn context_window(&self) -> u32;
    async fn complete(
        &self,
        request: &ChatRequest,
        cancel: &CancellationToken,
        deltas: DeltaSink,
    ) -> LlmResult<ChatResponse>;
}

/// A rough token estimate (four bytes per token) used for budgeting before a request.
pub fn estimate_tokens(chars: usize) -> u64 {
    chars.div_ceil(4) as u64
}

/// Merges consecutive messages of the same wire role (tool results travel as user
/// messages for providers without a tool role).
pub(crate) fn coalesce(messages: &[Message], tool_as_user: bool) -> Vec<Message> {
    let mut out: Vec<Message> = Vec::new();
    for m in messages {
        let role = if tool_as_user && m.role == Role::Tool {
            Role::User
        } else {
            m.role
        };
        match out.last_mut() {
            Some(last) if last.role == role => last.content.extend(m.content.iter().cloned()),
            _ => out.push(Message {
                role,
                content: m.content.clone(),
            }),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn coalesce_merges_tool_results_into_user_turns() {
        let msgs = vec![
            Message::user("a"),
            Message::tool_result("1", "r", false),
            Message::assistant("b"),
            Message::assistant("c"),
        ];
        let merged = coalesce(&msgs, true);
        assert_eq!(merged.len(), 2);
        assert_eq!(merged[0].content.len(), 2);
        assert_eq!(merged[1].text(), "bc");
        assert_eq!(coalesce(&msgs, false).len(), 3);
    }

    #[test]
    fn retryable_errors() {
        assert!(
            LlmError::Http {
                status: 429,
                message: String::new(),
                retry_after: None
            }
            .is_retryable()
        );
        assert!(
            LlmError::Http {
                status: 503,
                message: String::new(),
                retry_after: None
            }
            .is_retryable()
        );
        assert!(
            !LlmError::Http {
                status: 400,
                message: String::new(),
                retry_after: None
            }
            .is_retryable()
        );
        assert!(!LlmError::Cancelled.is_retryable());
        assert_eq!(estimate_tokens(9), 3);
    }
}
