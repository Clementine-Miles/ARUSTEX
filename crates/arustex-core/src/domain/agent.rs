use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use super::{TaskId, WorkItemId, str_enum};
use crate::{CoreError, CoreResult};

/// Workflow roles. Each role has a fixed set of visible operations (see `arustex-policy`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentRole {
    Planner,
    Worker,
    Assistant,
    Reviewer,
}

str_enum!(AgentRole {
    Planner => "planner",
    Worker => "worker",
    Assistant => "assistant",
    Reviewer => "reviewer",
});

/// Static description of an agent; loaded from configuration, never from model output.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AgentDefinition {
    pub key: String,
    pub name: String,
    pub role: AgentRole,
    pub enabled: bool,
    /// Name of the configured model profile; `None` selects the router default.
    pub model_profile: Option<String>,
    pub max_turns: u32,
    pub token_budget: u64,
    pub prompt_key: String,
}

impl AgentDefinition {
    pub fn validate_key(key: &str) -> CoreResult<()> {
        let mut chars = key.chars();
        let valid = matches!(chars.next(), Some('a'..='z'))
            && chars.all(|c| matches!(c, 'a'..='z' | '0'..='9' | '_'))
            && key.len() <= 64;
        if valid {
            Ok(())
        } else {
            Err(CoreError::invalid(format!("invalid agent key: {key:?}")))
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct SessionId(pub Uuid);

impl SessionId {
    pub fn new() -> Self {
        Self(Uuid::now_v7())
    }
}

impl Default for SessionId {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Display for SessionId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionStatus {
    Pending,
    Running,
    AwaitingApproval,
    Completed,
    NeedsReview,
    Failed,
    Cancelled,
}

str_enum!(SessionStatus {
    Pending => "pending",
    Running => "running",
    AwaitingApproval => "awaiting_approval",
    Completed => "completed",
    NeedsReview => "needs_review",
    Failed => "failed",
    Cancelled => "cancelled",
});

impl SessionStatus {
    pub fn is_terminal(self) -> bool {
        matches!(
            self,
            Self::Completed | Self::NeedsReview | Self::Failed | Self::Cancelled
        )
    }
}

/// Explicit terminal outcome of a workflow session.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum TerminalOutcome {
    /// A validated structured result was produced and persisted.
    Completed,
    /// The result needs human confirmation before it is accepted.
    NeedsReview { reason: String },
    /// The session stopped on an error that retrying the same input will not fix.
    Failed { reason: String },
    /// The operator or shutdown cancelled the session.
    Cancelled,
    /// The token budget or turn limit was reached before a result was produced.
    BudgetExhausted { limit: String },
}

impl TerminalOutcome {
    pub fn status(&self) -> SessionStatus {
        match self {
            Self::Completed => SessionStatus::Completed,
            Self::NeedsReview { .. } | Self::BudgetExhausted { .. } => SessionStatus::NeedsReview,
            Self::Failed { .. } => SessionStatus::Failed,
            Self::Cancelled => SessionStatus::Cancelled,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Session {
    pub id: SessionId,
    pub task_id: Option<TaskId>,
    pub work_item_id: Option<WorkItemId>,
    pub role: AgentRole,
    pub agent_key: String,
    pub status: SessionStatus,
    pub prompt_key: String,
    pub prompt_version: i32,
    pub model_profile: String,
    pub model: String,
    pub token_budget: u64,
    pub tokens_used: u64,
    pub turn: u32,
    pub max_turns: u32,
    pub outcome: Option<TerminalOutcome>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// Input for creating a session row.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NewSession {
    pub id: SessionId,
    pub task_id: Option<TaskId>,
    pub work_item_id: Option<WorkItemId>,
    pub role: AgentRole,
    pub agent_key: String,
    pub prompt_key: String,
    pub prompt_version: i32,
    pub model_profile: String,
    pub model: String,
    pub token_budget: u64,
    pub max_turns: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TranscriptKind {
    System,
    User,
    Assistant,
    OperationCall,
    OperationResult,
    Note,
}

str_enum!(TranscriptKind {
    System => "system",
    User => "user",
    Assistant => "assistant",
    OperationCall => "operation_call",
    OperationResult => "operation_result",
    Note => "note",
});

/// One durable transcript entry. `seq` is dense and starts at 1 per session.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TranscriptEntry {
    pub session_id: SessionId,
    pub seq: i64,
    pub kind: TranscriptKind,
    pub content: serde_json::Value,
    pub input_tokens: u32,
    pub output_tokens: u32,
    pub created_at: DateTime<Utc>,
}

/// A versioned prompt template. Bodies are application configuration, not model output.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PromptTemplate {
    pub key: String,
    pub version: i32,
    pub body: String,
    pub created_at: DateTime<Utc>,
}

impl PromptTemplate {
    /// Substitutes `{{name}}` placeholders. Unknown placeholders are rejected so templates
    /// cannot silently drop context; values are inserted verbatim and never re-expanded.
    pub fn render(&self, vars: &[(&str, &str)]) -> CoreResult<String> {
        let mut out = String::with_capacity(self.body.len());
        let mut rest = self.body.as_str();
        while let Some(start) = rest.find("{{") {
            out.push_str(&rest[..start]);
            let after = &rest[start + 2..];
            let end = after
                .find("}}")
                .ok_or_else(|| CoreError::invalid("unterminated template placeholder"))?;
            let name = after[..end].trim();
            let value = vars
                .iter()
                .find(|(key, _)| *key == name)
                .map(|(_, value)| *value)
                .ok_or_else(|| CoreError::invalid(format!("unknown template variable {name}")))?;
            out.push_str(value);
            rest = &after[end + 2..];
        }
        out.push_str(rest);
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn template(body: &str) -> PromptTemplate {
        PromptTemplate {
            key: "k".into(),
            version: 1,
            body: body.into(),
            created_at: Utc::now(),
        }
    }

    #[test]
    fn render_substitutes_without_reexpansion() {
        let rendered = template("Objective: {{ objective }}.")
            .render(&[("objective", "{{secret}}")])
            .expect("render");
        assert_eq!(rendered, "Objective: {{secret}}.");
    }

    #[test]
    fn render_rejects_unknown_and_unterminated() {
        assert!(template("{{missing}}").render(&[]).is_err());
        assert!(template("{{open").render(&[]).is_err());
    }

    #[test]
    fn agent_keys() {
        assert!(AgentDefinition::validate_key("worker_1").is_ok());
        assert!(AgentDefinition::validate_key("1worker").is_err());
        assert!(AgentDefinition::validate_key("Worker").is_err());
    }

    #[test]
    fn outcome_maps_to_status() {
        assert_eq!(TerminalOutcome::Completed.status(), SessionStatus::Completed);
        assert_eq!(
            TerminalOutcome::BudgetExhausted { limit: "tokens".into() }.status(),
            SessionStatus::NeedsReview
        );
    }
}

/// Token accounting reported by a provider for one request.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TokenUsage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_read_tokens: u64,
    pub cache_write_tokens: u64,
}

impl TokenUsage {
    pub fn total(&self) -> u64 {
        self.input_tokens + self.output_tokens
    }

    pub fn add(&mut self, other: &TokenUsage) {
        self.input_tokens += other.input_tokens;
        self.output_tokens += other.output_tokens;
        self.cache_read_tokens += other.cache_read_tokens;
        self.cache_write_tokens += other.cache_write_tokens;
    }
}
