//! Durable, ordered lifecycle events. The store assigns a strictly increasing `seq`;
//! IPC clients resume a subscription from the last sequence number they saw.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::domain::agent::{SessionId, SessionStatus};
use crate::domain::finding::{FindingStatus, Severity};
use crate::domain::task::TaskStatus;
use crate::domain::{ApprovalId, DatasetId, FindingId, ReportId, TaskId, WorkItemId};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum DomainEvent {
    TaskCreated {
        task_id: TaskId,
        name: String,
    },
    TaskStatusChanged {
        task_id: TaskId,
        from: TaskStatus,
        to: TaskStatus,
        reason: Option<String>,
    },
    SessionStarted {
        session_id: SessionId,
        role: crate::domain::agent::AgentRole,
    },
    SessionStatusChanged {
        session_id: SessionId,
        status: SessionStatus,
    },
    TranscriptAppended {
        session_id: SessionId,
        seq: i64,
    },
    /// A streamed partial model output; not persisted as a transcript entry.
    OutputDelta {
        session_id: SessionId,
        text: String,
    },
    WorkItemsPlanned {
        task_id: TaskId,
        count: u32,
    },
    WorkItemClaimed {
        work_item_id: WorkItemId,
        worker: String,
    },
    WorkItemFinished {
        work_item_id: WorkItemId,
        succeeded: bool,
    },
    ApprovalRequested {
        approval_id: ApprovalId,
        operation: String,
    },
    ApprovalDecided {
        approval_id: ApprovalId,
        approved: bool,
    },
    FindingRecorded {
        finding_id: FindingId,
        severity: Severity,
    },
    FindingStatusChanged {
        finding_id: FindingId,
        status: FindingStatus,
    },
    RecordsImported {
        dataset_id: DatasetId,
        inserted: u64,
        duplicates: u64,
    },
    ReportGenerated {
        report_id: ReportId,
        task_id: TaskId,
    },
    DaemonStarted {
        version: String,
    },
}

impl DomainEvent {
    /// Stable storage discriminator.
    pub fn kind(&self) -> &'static str {
        match self {
            Self::TaskCreated { .. } => "task_created",
            Self::TaskStatusChanged { .. } => "task_status_changed",
            Self::SessionStarted { .. } => "session_started",
            Self::SessionStatusChanged { .. } => "session_status_changed",
            Self::TranscriptAppended { .. } => "transcript_appended",
            Self::OutputDelta { .. } => "output_delta",
            Self::WorkItemsPlanned { .. } => "work_items_planned",
            Self::WorkItemClaimed { .. } => "work_item_claimed",
            Self::WorkItemFinished { .. } => "work_item_finished",
            Self::ApprovalRequested { .. } => "approval_requested",
            Self::ApprovalDecided { .. } => "approval_decided",
            Self::FindingRecorded { .. } => "finding_recorded",
            Self::FindingStatusChanged { .. } => "finding_status_changed",
            Self::RecordsImported { .. } => "records_imported",
            Self::ReportGenerated { .. } => "report_generated",
            Self::DaemonStarted { .. } => "daemon_started",
        }
    }

    /// Transient events are broadcast to live subscribers but not stored.
    pub fn is_transient(&self) -> bool {
        matches!(self, Self::OutputDelta { .. })
    }
}

/// A stored event with its sequence number. Transient events carry `seq = 0`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EventEnvelope {
    pub seq: i64,
    pub at: DateTime<Utc>,
    pub task_id: Option<TaskId>,
    pub session_id: Option<SessionId>,
    pub event: DomainEvent,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn event_wire_format_is_tagged() {
        let event = DomainEvent::TaskCreated {
            task_id: TaskId(7),
            name: "n".into(),
        };
        let json = serde_json::to_value(&event).expect("serialize");
        assert_eq!(json["type"], "task_created");
        assert_eq!(json["task_id"], 7);
        assert_eq!(event.kind(), "task_created");
        let back: DomainEvent = serde_json::from_value(json).expect("deserialize");
        assert_eq!(back, event);
    }
}
