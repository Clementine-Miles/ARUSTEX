use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use super::{Origin, ReportId, TaskId, str_enum};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReportStatus {
    /// Proposed by a model or generated but not yet approved for export.
    Draft,
    /// Approved by an operator.
    Final,
}

str_enum!(ReportStatus {
    Draft => "draft",
    Final => "final",
});

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Report {
    pub id: ReportId,
    pub task_id: TaskId,
    pub title: String,
    pub status: ReportStatus,
    pub origin: Origin,
    pub body_markdown: String,
    /// Machine-readable report document (see `docs/report-schema.md`).
    pub document: serde_json::Value,
    pub created_at: DateTime<Utc>,
    pub finalized_at: Option<DateTime<Utc>>,
}
