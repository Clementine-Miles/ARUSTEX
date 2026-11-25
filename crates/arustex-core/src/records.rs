use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum View {
    Tasks,
    Assets,
    Findings,
    Sessions,
    Approvals,
    TrafficEvidence,
    Exploration { task_id: i64 },
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct TaskSummary {
    pub id: i64,
    pub name: String,
    pub status: String,
    pub exploration_id: i64,
    pub archived: bool,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct AssetSummary {
    pub id: i64,
    pub kind: String,
    pub company_id: Option<i64>,
    pub label: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct FindingSummary {
    pub id: i64,
    pub task_id: Option<i64>,
    pub name: String,
    pub severity: String,
    pub status: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct SessionSummary {
    pub id: i64,
    pub agent_key: String,
    pub title: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ApprovalSummary {
    pub id: i64,
    pub status: String,
    pub task_ref: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct TrafficEvidenceSummary {
    pub id: i64,
    pub finding_id: i64,
    pub method: String,
    pub status: i32,
    pub request_bytes: i64,
    pub response_bytes: i64,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ExplorationNodeSummary {
    pub id: i64,
    pub kind: String,
    pub state: String,
    pub priority: i32,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "view", content = "items", rename_all = "snake_case")]
pub enum Rows {
    Tasks(Vec<TaskSummary>),
    Assets(Vec<AssetSummary>),
    Findings(Vec<FindingSummary>),
    Sessions(Vec<SessionSummary>),
    Approvals(Vec<ApprovalSummary>),
    TrafficEvidence(Vec<TrafficEvidenceSummary>),
    Exploration(Vec<ExplorationNodeSummary>),
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct Page {
    pub rows: Rows,
    pub next_before: Option<i64>,
}
