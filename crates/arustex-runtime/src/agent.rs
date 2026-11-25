//! The agent session loop.

use std::sync::Arc;

use arustex_core::domain::agent::{
    AgentDefinition, NewSession, SessionId, SessionStatus, TerminalOutcome, TokenUsage,
    TranscriptKind,
};
use arustex_core::domain::{TaskId, WorkItemId};
use arustex_core::ports::{RecordScope, RecordSource, ReviewSink, SessionStore};
use arustex_core::{CoreError, CoreResult};
use arustex_llm::{
    ChatRequest, ContentPart, DeltaSink, Message, Provider, ProviderRouter, Role, StopReason,
    ToolDefinition,
};
use arustex_operations::{OperationContext, Registry};
use arustex_policy::PolicyContext;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

use crate::context::{compact, history_tokens};
use crate::roles::{SUBMIT_RESULT, result_schema, validate_result};

const CHECKPOINT_VERSION: u32 = 1;
const MAX_NUDGES: u32 = 2;
/// Bound for one tool result handed back to the model.
const MAX_TOOL_RESULT_CHARS: usize = 64 * 1024;

/// A rendered prompt and where it came from.
#[derive(Debug, Clone)]
pub struct PromptRef {
    pub key: String,
    pub version: i32,
    pub system: String,
}

/// Everything needed to run (or resume) one session.
#[derive(Debug, Clone)]
pub struct SessionSpec {
    pub id: SessionId,
    pub task_id: Option<TaskId>,
    pub work_item_id: Option<WorkItemId>,
    pub agent: AgentDefinition,
    pub prompt: PromptRef,
    pub brief: String,
    pub scope: RecordScope,
    pub resume: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionReport {
    pub id: SessionId,
    pub outcome: TerminalOutcome,
    pub result: Option<Value>,
    pub turns: u32,
    pub tokens_used: u64,
    pub model: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Checkpoint {
    version: u32,
    turn: u32,
    calls: u32,
    nudges: u32,
    tokens_used: u64,
    history: Vec<Message>,
}

/// Runs sessions against the configured providers and the static registry.
#[derive(Debug, Clone)]
pub struct AgentRunner {
    sessions: Arc<dyn SessionStoreDyn>,
    records: Arc<dyn RecordSourceDyn>,
    review: Arc<dyn ReviewSinkDyn>,
    registry: Arc<Registry>,
    providers: ProviderRouter,
    max_output_tokens: u32,
}

// Debug-capable wrappers so the runner can derive Debug over trait objects.
trait SessionStoreDyn: SessionStore + std::fmt::Debug {}
impl<T: SessionStore + std::fmt::Debug> SessionStoreDyn for T {}
trait RecordSourceDyn: RecordSource + std::fmt::Debug {}
impl<T: RecordSource + std::fmt::Debug> RecordSourceDyn for T {}
trait ReviewSinkDyn: ReviewSink + std::fmt::Debug {}
impl<T: ReviewSink + std::fmt::Debug> ReviewSinkDyn for T {}

/// Adapters from the debug wrappers back to the ports used by operations.
#[derive(Debug)]
struct Records(Arc<dyn RecordSourceDyn>);
#[derive(Debug)]
struct Review(Arc<dyn ReviewSinkDyn>);

#[async_trait::async_trait]
impl RecordSource for Records {
    async fn search_records(
        &self,
        scope: &RecordScope,
        query: &str,
        limit: u32,
    ) -> CoreResult<Vec<arustex_core::ports::RecordSummary>> {
        self.0.search_records(scope, query, limit).await
    }
    async fn get_records(
        &self,
        scope: &RecordScope,
        ids: &[arustex_core::domain::RecordId],
    ) -> CoreResult<Vec<arustex_core::domain::record::ImportedRecord>> {
        self.0.get_records(scope, ids).await
    }
    async fn scope_stats(
        &self,
        scope: &RecordScope,
    ) -> CoreResult<arustex_core::ports::ScopeStats> {
        self.0.scope_stats(scope).await
    }
    async fn exploration_snapshot(
        &self,
        task_id: TaskId,
    ) -> CoreResult<arustex_core::domain::exploration::ExplorationSnapshot> {
        self.0.exploration_snapshot(task_id).await
    }
}

#[async_trait::async_trait]
impl ReviewSink for Review {
    async fn record_finding(
        &self,
        finding: arustex_core::domain::finding::NewFinding,
    ) -> CoreResult<arustex_core::domain::finding::Finding> {
        self.0.record_finding(finding).await
    }
    async fn request_approval(
        &self,
        approval: arustex_core::domain::work::NewApproval,
    ) -> CoreResult<arustex_core::domain::work::Approval> {
        self.0.request_approval(approval).await
    }
    async fn save_report_draft(
        &self,
        task_id: TaskId,
        title: &str,
        body_markdown: &str,
        session_id: SessionId,
    ) -> CoreResult<arustex_core::domain::report::Report> {
        self.0
            .save_report_draft(task_id, title, body_markdown, session_id)
            .await
    }
}

fn usage(u: arustex_llm::Usage) -> TokenUsage {
    TokenUsage {
        input_tokens: u.input_tokens,
        output_tokens: u.output_tokens,
        ..TokenUsage::default()
    }
}

fn bounded(text: String) -> String {
    if text.len() <= MAX_TOOL_RESULT_CHARS {
        return text;
    }
    let mut end = MAX_TOOL_RESULT_CHARS;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…[truncated]", &text[..end])
}

impl AgentRunner {
    pub fn new<S, R, V>(
        sessions: Arc<S>,
        records: Arc<R>,
        review: Arc<V>,
        registry: Arc<Registry>,
        providers: ProviderRouter,
    ) -> Self
    where
        S: SessionStore + std::fmt::Debug + 'static,
        R: RecordSource + std::fmt::Debug + 'static,
        V: ReviewSink + std::fmt::Debug + 'static,
    {
        Self {
            sessions,
            records,
            review,
            registry,
            providers,
            max_output_tokens: 4096,
        }
    }

    pub fn with_max_output_tokens(mut self, tokens: u32) -> Self {
        self.max_output_tokens = tokens.clamp(256, 64_000);
        self
    }

    pub fn registry(&self) -> &Registry {
        &self.registry
    }

    pub fn providers(&self) -> &ProviderRouter {
        &self.providers
    }

    fn tools(&self, spec: &SessionSpec) -> Vec<ToolDefinition> {
        let mut tools: Vec<ToolDefinition> = self
            .registry
            .tools_for(spec.agent.role)
            .into_iter()
            .map(|t| ToolDefinition {
                name: t.name,
                description: t.description,
                input_schema: t.input_schema,
            })
            .collect();
        tools.push(ToolDefinition {
            name: SUBMIT_RESULT.into(),
            description: "Finish the session with your structured result.".into(),
            input_schema: result_schema(spec.agent.role),
        });
        tools
    }

    /// Runs a session to an explicit outcome. Provider and storage failures finish the
    /// session as failed and are returned so the caller can decide on a retry.
    pub async fn run(
        &self,
        spec: SessionSpec,
        cancel: CancellationToken,
        deltas: DeltaSink,
    ) -> CoreResult<SessionReport> {
        let provider = self.providers.get(spec.agent.model_profile.as_deref())?;
        let profile = spec
            .agent
            .model_profile
            .clone()
            .unwrap_or_else(|| provider.name().to_owned());
        let mut state = self.open(&spec, provider.as_ref(), &profile).await?;
        let result = self
            .drive(
                &spec,
                provider.as_ref(),
                &profile,
                &mut state,
                &cancel,
                deltas,
            )
            .await;
        let (outcome, value, error) = match result {
            Ok((outcome, value)) => (outcome, value, None),
            Err(CoreError::Cancelled) => (TerminalOutcome::Cancelled, None, None),
            Err(e) => (
                TerminalOutcome::Failed {
                    reason: e.to_string().chars().take(500).collect(),
                },
                None,
                Some(e),
            ),
        };
        self.sessions
            .append_transcript(
                spec.id,
                TranscriptKind::Note,
                json!({"outcome": outcome}),
                TokenUsage::default(),
            )
            .await?;
        self.sessions.finish_session(spec.id, &outcome).await?;
        if let Some(e) = error {
            return Err(e);
        }
        Ok(SessionReport {
            id: spec.id,
            outcome,
            result: value,
            turns: state.turn,
            tokens_used: state.tokens_used,
            model: provider.model().to_owned(),
        })
    }

    async fn open(
        &self,
        spec: &SessionSpec,
        provider: &dyn Provider,
        profile: &str,
    ) -> CoreResult<Checkpoint> {
        if spec.resume
            && let Ok(session) = self.sessions.get_session(spec.id).await
        {
            if session.status.is_terminal() {
                return Err(CoreError::Conflict(format!(
                    "session {} already finished",
                    spec.id
                )));
            }
            if let Some(cp) = self.sessions.load_checkpoint(spec.id).await? {
                let cp: Checkpoint = serde_json::from_value(cp)
                    .map_err(|e| CoreError::invalid(format!("checkpoint: {e}")))?;
                if cp.version == CHECKPOINT_VERSION {
                    self.sessions
                        .append_transcript(
                            spec.id,
                            TranscriptKind::Note,
                            json!({"resumed_at_turn": cp.turn}),
                            TokenUsage::default(),
                        )
                        .await?;
                    return Ok(cp);
                }
            }
        } else {
            self.sessions
                .create_session(NewSession {
                    id: spec.id,
                    task_id: spec.task_id,
                    work_item_id: spec.work_item_id,
                    role: spec.agent.role,
                    agent_key: spec.agent.key.clone(),
                    prompt_key: spec.prompt.key.clone(),
                    prompt_version: spec.prompt.version,
                    model_profile: profile.to_owned(),
                    model: provider.model().to_owned(),
                    token_budget: spec.agent.token_budget,
                    max_turns: spec.agent.max_turns,
                })
                .await?;
            self.sessions
                .append_transcript(
                    spec.id,
                    TranscriptKind::System,
                    json!({"prompt_key": spec.prompt.key, "prompt_version": spec.prompt.version, "text": spec.prompt.system}),
                    TokenUsage::default(),
                )
                .await?;
            self.sessions
                .append_transcript(
                    spec.id,
                    TranscriptKind::User,
                    json!({"text": spec.brief}),
                    TokenUsage::default(),
                )
                .await?;
        }
        Ok(Checkpoint {
            version: CHECKPOINT_VERSION,
            turn: 0,
            calls: 0,
            nudges: 0,
            tokens_used: 0,
            history: vec![Message::user(spec.brief.clone())],
        })
    }

    async fn drive(
        &self,
        spec: &SessionSpec,
        provider: &dyn Provider,
        profile: &str,
        state: &mut Checkpoint,
        cancel: &CancellationToken,
        deltas: DeltaSink,
    ) -> CoreResult<(TerminalOutcome, Option<Value>)> {
        let tools = self.tools(spec);
        let tools_chars: usize = tools
            .iter()
            .map(|t| t.description.len() + t.input_schema.to_string().len())
            .sum();
        let window = u64::from(provider.context_window());
        let budget = (window * 3 / 4)
            .saturating_sub(u64::from(self.max_output_tokens))
            .max(2_000);
        let op_ctx = OperationContext {
            session_id: spec.id,
            policy: PolicyContext {
                role: spec.agent.role,
                scope: spec.scope.clone(),
            },
            records: Arc::new(Records(self.records.clone())),
            review: Arc::new(Review(self.review.clone())),
        };
        loop {
            if cancel.is_cancelled() {
                return Err(CoreError::Cancelled);
            }
            if state.turn >= spec.agent.max_turns {
                return Ok((
                    TerminalOutcome::BudgetExhausted {
                        limit: format!("{} turns", spec.agent.max_turns),
                    },
                    None,
                ));
            }
            if spec.agent.token_budget > 0 && state.tokens_used >= spec.agent.token_budget {
                return Ok((
                    TerminalOutcome::BudgetExhausted {
                        limit: format!("{} tokens", spec.agent.token_budget),
                    },
                    None,
                ));
            }
            let fixed = history_tokens(&spec.prompt.system, tools_chars, &[]);
            let dropped = compact(&mut state.history, fixed, budget, 2);
            if dropped > 0 {
                self.sessions
                    .append_transcript(
                        spec.id,
                        TranscriptKind::Note,
                        json!({"compacted_messages": dropped}),
                        TokenUsage::default(),
                    )
                    .await?;
            }
            self.sessions
                .update_session_progress(
                    spec.id,
                    SessionStatus::Running,
                    state.turn,
                    state.tokens_used,
                )
                .await?;

            let request = ChatRequest {
                system: spec.prompt.system.clone(),
                messages: state.history.clone(),
                tools: tools.clone(),
                max_output_tokens: self.max_output_tokens,
            };
            let response = provider
                .complete(&request, cancel, deltas.clone())
                .await
                .map_err(CoreError::from)?;
            state.turn += 1;
            state.tokens_used += response.usage.total();
            let turn_usage = usage(response.usage);
            self.sessions
                .record_usage(spec.id, profile, &response.model, turn_usage)
                .await?;
            self.sessions
                .append_transcript(
                    spec.id,
                    TranscriptKind::Assistant,
                    serde_json::to_value(&response.message).unwrap_or_default(),
                    turn_usage,
                )
                .await?;
            let calls: Vec<(String, String, Value)> = response
                .message
                .tool_uses()
                .map(|(id, n, i)| (id.to_owned(), n.to_owned(), i.clone()))
                .collect();
            state.history.push(response.message);

            if calls.is_empty() {
                if state.nudges >= MAX_NUDGES {
                    return Ok((
                        TerminalOutcome::NeedsReview {
                            reason: "the model stopped without submitting a result".into(),
                        },
                        None,
                    ));
                }
                state.nudges += 1;
                let note = if response.stop_reason == StopReason::MaxTokens {
                    "Your reply was cut off. Continue, and finish by calling submit_result."
                } else {
                    "Finish by calling submit_result with your structured result."
                };
                state.history.push(Message::user(note));
                self.checkpoint(spec.id, state).await?;
                continue;
            }

            let mut results = Vec::new();
            let mut submitted = None;
            for (id, name, input) in calls {
                if cancel.is_cancelled() {
                    return Err(CoreError::Cancelled);
                }
                if submitted.is_some() {
                    results.push((
                        id,
                        "ignored: the result was already submitted".to_owned(),
                        true,
                    ));
                    continue;
                }
                self.sessions
                    .append_transcript(
                        spec.id,
                        TranscriptKind::OperationCall,
                        json!({"id": id, "name": name, "input": input}),
                        TokenUsage::default(),
                    )
                    .await?;
                let (text, is_error) = if name == SUBMIT_RESULT {
                    match validate_result(spec.agent.role, &input, &spec.scope) {
                        Ok(value) => {
                            submitted = Some(value);
                            ("accepted".to_owned(), false)
                        }
                        Err(msg) => (msg, true),
                    }
                } else {
                    state.calls += 1;
                    match self
                        .registry
                        .invoke(&op_ctx, &name, input, state.calls - 1)
                        .await
                    {
                        Ok(inv) => (bounded(inv.output.to_string()), false),
                        Err(CoreError::Cancelled) => return Err(CoreError::Cancelled),
                        Err(e @ (CoreError::Storage(_) | CoreError::Unavailable(_))) => {
                            return Err(e);
                        }
                        Err(e) => (format!("operation failed: {e}"), true),
                    }
                };
                self.sessions
                    .append_transcript(
                        spec.id,
                        TranscriptKind::OperationResult,
                        json!({"id": id, "name": name, "is_error": is_error, "output": text}),
                        TokenUsage::default(),
                    )
                    .await?;
                results.push((id, text, is_error));
            }
            state.history.push(Message {
                role: Role::Tool,
                content: results
                    .into_iter()
                    .map(|(id, content, is_error)| ContentPart::ToolResult {
                        tool_use_id: id,
                        content,
                        is_error,
                    })
                    .collect(),
            });
            if let Some(value) = submitted {
                return Ok((TerminalOutcome::Completed, Some(value)));
            }
            self.checkpoint(spec.id, state).await?;
        }
    }

    async fn checkpoint(&self, id: SessionId, state: &mut Checkpoint) -> CoreResult<()> {
        let mut value = serde_json::to_value(&*state).unwrap_or_default();
        // Keep checkpoints under the store limit by compacting further if needed.
        while value.to_string().len() > 900 * 1024 {
            let target = history_tokens("", 0, &state.history) / 2;
            if compact(&mut state.history, 0, target, 1) == 0 {
                break;
            }
            value = serde_json::to_value(&*state).unwrap_or_default();
        }
        self.sessions.save_checkpoint(id, value).await
    }
}
