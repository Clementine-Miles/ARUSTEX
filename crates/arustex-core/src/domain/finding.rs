//! Review observations ("findings"), operator review notes and reviewer results.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use super::{
    AssetId, FindingId, NodeId, NoteId, Provenance, RecordId, ReviewResultId, TaskId, WorkItemId,
    str_enum,
};
use crate::{CoreError, CoreResult};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Severity {
    Info,
    Low,
    Medium,
    High,
    Critical,
}

str_enum!(Severity {
    Info => "info",
    Low => "low",
    Medium => "medium",
    High => "high",
    Critical => "critical",
});

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FindingStatus {
    Pending,
    Confirmed,
    Dismissed,
    Resolved,
    Duplicate,
}

str_enum!(FindingStatus {
    Pending => "pending",
    Confirmed => "confirmed",
    Dismissed => "dismissed",
    Resolved => "resolved",
    Duplicate => "duplicate",
});

impl FindingStatus {
    /// Legacy statuses map onto the reduced review vocabulary.
    pub fn from_legacy(value: &str) -> Self {
        match value {
            "confirmed" | "in_progress" => Self::Confirmed,
            "false_positive" | "ignored" | "risk_accepted" => Self::Dismissed,
            "resolved" | "fixed" => Self::Resolved,
            "duplicate" => Self::Duplicate,
            _ => Self::Pending,
        }
    }
}

/// An observation recorded during review. Model-generated findings always start as
/// `Pending` and require operator confirmation.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Finding {
    pub id: FindingId,
    pub task_id: Option<TaskId>,
    pub node_id: Option<NodeId>,
    pub title: String,
    pub category: String,
    pub severity: Severity,
    pub status: FindingStatus,
    pub summary: String,
    pub record_ids: Vec<RecordId>,
    pub asset_ids: Vec<AssetId>,
    pub provenance: Provenance,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NewFinding {
    pub task_id: Option<TaskId>,
    pub node_id: Option<NodeId>,
    pub title: String,
    pub category: String,
    pub severity: Severity,
    pub summary: String,
    pub record_ids: Vec<RecordId>,
    pub asset_ids: Vec<AssetId>,
    pub provenance: Provenance,
    #[serde(default)]
    pub status: Option<FindingStatus>,
    #[serde(default)]
    pub preserve_id: Option<FindingId>,
}

pub const MAX_FINDING_TITLE: usize = 512;
pub const MAX_FINDING_SUMMARY: usize = 16 * 1024;
pub const MAX_FINDING_REFERENCES: usize = 256;

impl NewFinding {
    pub fn validate(&self) -> CoreResult<()> {
        if self.title.trim().is_empty() {
            return Err(CoreError::invalid("finding title must not be empty"));
        }
        if self.title.len() > MAX_FINDING_TITLE || self.summary.len() > MAX_FINDING_SUMMARY {
            return Err(CoreError::LimitExceeded("finding text too long".into()));
        }
        if self.record_ids.len() + self.asset_ids.len() > MAX_FINDING_REFERENCES {
            return Err(CoreError::LimitExceeded("finding references too many items".into()));
        }
        Ok(())
    }

    /// The status a new finding receives: generated findings are always pending review.
    pub fn initial_status(&self) -> FindingStatus {
        match self.provenance.origin {
            super::Origin::ModelGenerated => FindingStatus::Pending,
            _ => self.status.unwrap_or(FindingStatus::Pending),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NoteSubject {
    Task,
    Finding,
    Record,
    ReviewResult,
}

str_enum!(NoteSubject {
    Task => "task",
    Finding => "finding",
    Record => "record",
    ReviewResult => "review_result",
});

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ReviewNote {
    pub id: NoteId,
    pub subject: NoteSubject,
    pub subject_id: i64,
    pub author: String,
    pub body: String,
    pub created_at: DateTime<Utc>,
}

pub const MAX_NOTE_BODY: usize = 32 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Verdict {
    Match,
    Mismatch,
    Inconclusive,
}

str_enum!(Verdict {
    Match => "match",
    Mismatch => "mismatch",
    Inconclusive => "inconclusive",
});

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConfirmationStatus {
    PendingConfirmation,
    Confirmed,
    Rejected,
}

str_enum!(ConfirmationStatus {
    PendingConfirmation => "pending_confirmation",
    Confirmed => "confirmed",
    Rejected => "rejected",
});

/// Reviewer output comparing a work item's results with fixtures or criteria.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ReviewResult {
    pub id: ReviewResultId,
    pub task_id: TaskId,
    pub work_item_id: Option<WorkItemId>,
    pub verdict: Verdict,
    pub details: serde_json::Value,
    pub status: ConfirmationStatus,
    pub decided_by: Option<String>,
    pub created_at: DateTime<Utc>,
    pub decided_at: Option<DateTime<Utc>>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::Provenance;

    #[test]
    fn generated_findings_start_pending() {
        let finding = NewFinding {
            task_id: None,
            node_id: None,
            title: "Missing total".into(),
            category: "consistency".into(),
            severity: Severity::Low,
            summary: String::new(),
            record_ids: vec![],
            asset_ids: vec![],
            provenance: Provenance::generated(uuid::Uuid::nil()),
            status: Some(FindingStatus::Confirmed),
            preserve_id: None,
        };
        assert_eq!(finding.initial_status(), FindingStatus::Pending);
        assert!(finding.validate().is_ok());
    }

    #[test]
    fn legacy_status_mapping() {
        assert_eq!(FindingStatus::from_legacy("false_positive"), FindingStatus::Dismissed);
        assert_eq!(FindingStatus::from_legacy("fixed"), FindingStatus::Resolved);
        assert_eq!(FindingStatus::from_legacy("whatever"), FindingStatus::Pending);
    }

    #[test]
    fn severity_orders() {
        assert!(Severity::Critical > Severity::Low);
    }
}
