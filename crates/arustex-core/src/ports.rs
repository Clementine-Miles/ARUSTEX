//! Interfaces consumed by the runtime and implemented by `arustex-store` (and by
//! in-memory fakes in tests). Model-selected operations only ever reach these through
//! the policy-checked operation registry.

use std::time::Duration;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::CoreResult;
use crate::domain::agent::{
    NewSession, Session, SessionId, SessionStatus, TerminalOutcome, TokenUsage, TranscriptEntry,
    TranscriptKind,
};
use crate::domain::exploration::ExplorationSnapshot;
use crate::domain::finding::{Finding, NewFinding};
use crate::domain::record::ImportedRecord;
use crate::domain::report::Report;
use crate::domain::task::{Task, TaskStatus};
use crate::domain::work::{Approval, NewApproval, NewWorkItem, WorkItem, WorkStatus};
use crate::domain::{DatasetId, NodeId, RecordId, TaskId, WorkItemId};
use crate::events::{DomainEvent, EventEnvelope};

#[async_trait]
pub trait EventSink: Send + Sync {
    /// Persists (unless transient) and broadcasts an event.
    async fn publish(
        &self,
        task_id: Option<TaskId>,
        session_id: Option<SessionId>,
        event: DomainEvent,
    ) -> CoreResult<EventEnvelope>;
}

#[async_trait]
pub trait SessionStore: Send + Sync {
    async fn create_session(&self, session: NewSession) -> CoreResult<Session>;
    async fn get_session(&self, id: SessionId) -> CoreResult<Session>;
    async fn update_session_progress(
        &self,
        id: SessionId,
        status: SessionStatus,
        turn: u32,
        tokens_used: u64,
    ) -> CoreResult<()>;
    async fn finish_session(&self, id: SessionId, outcome: &TerminalOutcome) -> CoreResult<()>;
    async fn append_transcript(
        &self,
        id: SessionId,
        kind: TranscriptKind,
        content: serde_json::Value,
        usage: TokenUsage,
    ) -> CoreResult<TranscriptEntry>;
    async fn transcript(
        &self,
        id: SessionId,
        after_seq: i64,
        limit: u32,
    ) -> CoreResult<Vec<TranscriptEntry>>;
    async fn save_checkpoint(&self, id: SessionId, checkpoint: serde_json::Value)
    -> CoreResult<()>;
    async fn load_checkpoint(&self, id: SessionId) -> CoreResult<Option<serde_json::Value>>;
    async fn record_usage(
        &self,
        id: SessionId,
        profile: &str,
        model: &str,
        usage: TokenUsage,
    ) -> CoreResult<()>;
}

/// The data a session is approved to see. Operations reject anything outside it.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecordScope {
    pub task_id: Option<TaskId>,
    pub dataset_ids: Vec<DatasetId>,
    /// When set, narrows the scope to these records (a work item's batch).
    pub record_ids: Option<Vec<RecordId>>,
}

impl RecordScope {
    pub fn allows(&self, record: &ImportedRecord) -> bool {
        self.dataset_ids.contains(&record.dataset_id)
            && self
                .record_ids
                .as_ref()
                .is_none_or(|ids| ids.contains(&record.id))
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecordSummary {
    pub id: RecordId,
    pub dataset_id: DatasetId,
    pub external_id: String,
    pub title: String,
    pub content_type: String,
    pub body_bytes: u64,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ScopeStats {
    pub records: u64,
    pub total_body_bytes: u64,
    pub by_content_type: Vec<(String, u64)>,
    pub findings: u64,
}

#[async_trait]
pub trait RecordSource: Send + Sync {
    async fn search_records(
        &self,
        scope: &RecordScope,
        query: &str,
        limit: u32,
    ) -> CoreResult<Vec<RecordSummary>>;
    async fn get_records(
        &self,
        scope: &RecordScope,
        ids: &[RecordId],
    ) -> CoreResult<Vec<ImportedRecord>>;
    async fn scope_stats(&self, scope: &RecordScope) -> CoreResult<ScopeStats>;
    async fn exploration_snapshot(&self, task_id: TaskId) -> CoreResult<ExplorationSnapshot>;
}

#[async_trait]
pub trait ReviewSink: Send + Sync {
    async fn record_finding(&self, finding: NewFinding) -> CoreResult<Finding>;
    async fn request_approval(&self, approval: NewApproval) -> CoreResult<Approval>;
    async fn save_report_draft(
        &self,
        task_id: TaskId,
        title: &str,
        body_markdown: &str,
        session_id: SessionId,
    ) -> CoreResult<Report>;
}

#[async_trait]
pub trait TaskStore: Send + Sync {
    async fn get_task(&self, id: TaskId) -> CoreResult<Task>;
    /// Validates and applies a state-machine transition atomically, emitting an event.
    async fn transition_task(
        &self,
        id: TaskId,
        to: TaskStatus,
        reason: Option<String>,
    ) -> CoreResult<Task>;
    /// Tasks in `Queued` or `Running` status, oldest first.
    async fn active_tasks(&self, limit: u32) -> CoreResult<Vec<Task>>;
    async fn add_task_tokens(&self, id: TaskId, tokens: u64) -> CoreResult<Task>;
    /// Record ids in the task's datasets, ascending, for deterministic planning.
    async fn task_record_ids(&self, id: TaskId) -> CoreResult<Vec<RecordId>>;
    /// Marks a planned work-item node and anchors it to its records.
    async fn plan_node(&self, task_id: TaskId, title: &str, records: &[RecordId])
    -> CoreResult<NodeId>;
    async fn set_node_state(&self, node: NodeId, state: &str) -> CoreResult<()>;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct QueueCounts {
    pub queued: u64,
    pub leased: u64,
    pub done: u64,
    pub failed: u64,
    pub cancelled: u64,
}

#[async_trait]
pub trait WorkQueue: Send + Sync {
    async fn enqueue(&self, items: Vec<NewWorkItem>) -> CoreResult<Vec<WorkItem>>;
    /// Transactionally claims the highest-priority queued item of a running task.
    async fn claim(
        &self,
        worker: &str,
        task_id: Option<TaskId>,
        lease: Duration,
    ) -> CoreResult<Option<WorkItem>>;
    /// Extends a lease. Returns false when the lease was lost.
    async fn heartbeat(&self, id: WorkItemId, worker: &str, lease: Duration) -> CoreResult<bool>;
    async fn attach_session(
        &self,
        id: WorkItemId,
        worker: &str,
        session: SessionId,
    ) -> CoreResult<()>;
    async fn complete(
        &self,
        id: WorkItemId,
        worker: &str,
        result: serde_json::Value,
    ) -> CoreResult<()>;
    /// Records a failure; the item is re-queued while attempts remain and `retry` is set.
    async fn fail(
        &self,
        id: WorkItemId,
        worker: &str,
        error: &str,
        retry: bool,
    ) -> CoreResult<WorkStatus>;
    /// Returns a leased item to the queue without counting an attempt (graceful shutdown).
    async fn release(&self, id: WorkItemId, worker: &str) -> CoreResult<()>;
    /// Re-queues items whose lease expired (crash recovery). Returns the count.
    async fn recover_expired(&self) -> CoreResult<u64>;
    async fn cancel_task_items(&self, task_id: TaskId) -> CoreResult<u64>;
    async fn counts(&self, task_id: TaskId) -> CoreResult<QueueCounts>;
    async fn get_item(&self, id: WorkItemId) -> CoreResult<WorkItem>;
}
