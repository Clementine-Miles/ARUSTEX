//! Work items (scheduler jobs) and operator approvals.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use super::agent::SessionId;
use super::{ApprovalId, NodeId, RecordId, TaskId, WorkItemId, str_enum};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkKind {
    /// A worker reviews a bounded batch of records.
    Review,
    /// A reviewer compares worker output with fixtures or review criteria.
    Compare,
}

str_enum!(WorkKind {
    Review => "review",
    Compare => "compare",
});

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkStatus {
    Queued,
    Leased,
    Done,
    Failed,
    Cancelled,
}

str_enum!(WorkStatus {
    Queued => "queued",
    Leased => "leased",
    Done => "done",
    Failed => "failed",
    Cancelled => "cancelled",
});

impl WorkStatus {
    pub fn is_terminal(self) -> bool {
        matches!(self, Self::Done | Self::Failed | Self::Cancelled)
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WorkItem {
    pub id: WorkItemId,
    pub task_id: TaskId,
    pub kind: WorkKind,
    pub record_ids: Vec<RecordId>,
    pub status: WorkStatus,
    pub priority: i32,
    pub attempts: u32,
    pub max_attempts: u32,
    pub lease_owner: Option<String>,
    pub lease_expires_at: Option<DateTime<Utc>>,
    /// Stable identifier for idempotent processing across retries and restarts.
    pub operation_id: Uuid,
    pub node_id: Option<NodeId>,
    pub session_id: Option<SessionId>,
    pub result: Option<serde_json::Value>,
    pub last_error: Option<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NewWorkItem {
    pub task_id: TaskId,
    pub kind: WorkKind,
    pub record_ids: Vec<RecordId>,
    pub priority: i32,
    pub max_attempts: u32,
    pub node_id: Option<NodeId>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalStatus {
    Pending,
    Approved,
    Rejected,
    Applied,
    Failed,
}

str_enum!(ApprovalStatus {
    Pending => "pending",
    Approved => "approved",
    Rejected => "rejected",
    Applied => "applied",
    Failed => "failed",
});

/// A proposed local change that only an operator can authorize.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Approval {
    pub id: ApprovalId,
    pub task_id: Option<TaskId>,
    pub session_id: Option<SessionId>,
    pub operation: String,
    pub summary: String,
    pub request: serde_json::Value,
    pub status: ApprovalStatus,
    pub decided_by: Option<String>,
    pub reason: Option<String>,
    pub created_at: DateTime<Utc>,
    pub decided_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NewApproval {
    pub task_id: Option<TaskId>,
    pub session_id: Option<SessionId>,
    pub operation: String,
    pub summary: String,
    pub request: serde_json::Value,
}
