//! Versioned local IPC contract between the daemon and its clients (TUI, CLI).
//!
//! Framing: one JSON document per line (`\n`-terminated), at most [`MAX_FRAME_BYTES`].
//! Each request carries a client-chosen id that the matching response echoes.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::domain::agent::{Session, SessionId, TranscriptEntry};
use crate::domain::asset::{Asset, AssetEdge};
use crate::domain::exploration::ReviewCoverage;
use crate::domain::finding::{Finding, FindingStatus, NoteSubject, ReviewNote, ReviewResult};
use crate::domain::record::{Dataset, ImportedRecord};
use crate::domain::report::{Report, ReportSummary};
use crate::domain::task::{NewTask, Task, TaskAction};
use crate::domain::work::{Approval, ApprovalStatus};
use crate::domain::{
    ApprovalId, AssetId, DatasetId, FindingId, RecordId, ReportId, ReviewResultId, TaskId,
};
use crate::events::EventEnvelope;
use crate::ports::{QueueCounts, RecordSummary};
use crate::{CoreError, ErrorCode, Page, Paged};

pub const PROTOCOL_VERSION: u16 = 1;
pub const MAX_FRAME_BYTES: usize = 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ClientFrame {
    pub v: u16,
    pub id: u64,
    pub request: Request,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "frame", rename_all = "snake_case")]
pub enum ServerFrame {
    Response {
        v: u16,
        id: u64,
        result: Result<Response, WireError>,
    },
    Event {
        v: u16,
        event: EventEnvelope,
    },
    /// The subscriber fell behind the bounded live queue; it must take a fresh snapshot
    /// and resubscribe from `latest_seq`.
    Resync {
        v: u16,
        latest_seq: i64,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WireError {
    pub code: ErrorCode,
    pub message: String,
}

impl From<CoreError> for WireError {
    fn from(err: CoreError) -> Self {
        Self {
            code: err.code(),
            message: err.to_string(),
        }
    }
}

impl From<WireError> for CoreError {
    fn from(err: WireError) -> Self {
        CoreError::from_code(err.code, err.message)
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "method", content = "params", rename_all = "snake_case")]
pub enum Request {
    Hello {
        client: String,
    },
    Ping,
    Dashboard,
    ListTasks {
        page: Page,
    },
    GetTask {
        id: TaskId,
    },
    CreateTask {
        task: NewTask,
    },
    ControlTask {
        id: TaskId,
        action: TaskAction,
    },
    ListSessions {
        task_id: Option<TaskId>,
        page: Page,
    },
    GetTranscript {
        session_id: SessionId,
        after_seq: i64,
        limit: u32,
    },
    ListApprovals {
        status: Option<ApprovalStatus>,
        page: Page,
    },
    DecideApproval {
        id: ApprovalId,
        approve: bool,
        reason: Option<String>,
    },
    ListAssets {
        query: Option<String>,
        page: Page,
    },
    GetAsset {
        id: AssetId,
    },
    ListDatasets {
        page: Page,
    },
    ListRecords {
        dataset_id: Option<DatasetId>,
        query: Option<String>,
        page: Page,
    },
    GetRecord {
        id: RecordId,
    },
    ListFindings {
        task_id: Option<TaskId>,
        page: Page,
    },
    SetFindingStatus {
        id: FindingId,
        status: FindingStatus,
    },
    ListReviewResults {
        task_id: Option<TaskId>,
        page: Page,
    },
    DecideReviewResult {
        id: ReviewResultId,
        confirm: bool,
    },
    AddNote {
        subject: NoteSubject,
        subject_id: i64,
        body: String,
    },
    ListNotes {
        subject: NoteSubject,
        subject_id: i64,
    },
    ListReports {
        task_id: Option<TaskId>,
        page: Page,
    },
    GetReport {
        id: ReportId,
    },
    GenerateReport {
        task_id: TaskId,
    },
    GetSettings,
    /// Assistant chat scoped to a task; replies arrive as transcript events.
    AssistantMessage {
        task_id: TaskId,
        text: String,
    },
    /// Replays stored events after `after_seq`, then streams live events.
    Subscribe {
        after_seq: i64,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DashboardSnapshot {
    pub latest_seq: i64,
    pub daemon_version: String,
    pub started_at: DateTime<Utc>,
    pub tasks_by_status: Vec<(String, u64)>,
    pub active_sessions: u64,
    pub pending_approvals: u64,
    pub pending_findings: u64,
    pub datasets: u64,
    pub records: u64,
    pub tokens_today: u64,
    pub recent_events: Vec<EventEnvelope>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TaskDetail {
    pub task: Task,
    pub queue: QueueCounts,
    pub coverage: ReviewCoverage,
    pub findings: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AssetDetail {
    pub asset: Asset,
    pub ancestry: Vec<Asset>,
    pub edges: Vec<AssetEdge>,
}

/// Redacted, display-safe view of the effective configuration.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SettingsView {
    pub entries: Vec<(String, String)>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "data", rename_all = "snake_case")]
pub enum Response {
    Hello {
        protocol: u16,
        server: String,
        uid: u32,
    },
    Pong,
    Dashboard(Box<DashboardSnapshot>),
    Tasks(Paged<Task>),
    Task(Box<TaskDetail>),
    Sessions(Paged<Session>),
    Transcript(Vec<TranscriptEntry>),
    Approvals(Paged<Approval>),
    Approval(Approval),
    Assets(Paged<Asset>),
    Asset(Box<AssetDetail>),
    Datasets(Paged<Dataset>),
    Records(Paged<RecordSummary>),
    Record(Box<ImportedRecord>),
    Findings(Paged<Finding>),
    Finding(Box<Finding>),
    ReviewResults(Paged<ReviewResult>),
    ReviewResult(Box<ReviewResult>),
    Note(ReviewNote),
    Notes(Vec<ReviewNote>),
    Reports(Paged<ReportSummary>),
    Report(Box<Report>),
    Settings(SettingsView),
    Accepted,
    Subscribed {
        latest_seq: i64,
    },
}

/// Encodes one frame as a single line (without the trailing newline).
pub fn encode<T: Serialize>(frame: &T) -> Result<String, CoreError> {
    let text = serde_json::to_string(frame).map_err(|e| CoreError::Internal(e.to_string()))?;
    if text.len() > MAX_FRAME_BYTES {
        return Err(CoreError::LimitExceeded(format!(
            "frame of {} bytes exceeds {MAX_FRAME_BYTES}",
            text.len()
        )));
    }
    Ok(text)
}

/// Decodes one line, rejecting oversized frames and unsupported protocol versions.
pub fn decode_client(line: &str) -> Result<ClientFrame, CoreError> {
    if line.len() > MAX_FRAME_BYTES {
        return Err(CoreError::LimitExceeded("frame too large".into()));
    }
    let frame: ClientFrame =
        serde_json::from_str(line).map_err(|e| CoreError::invalid(format!("bad frame: {e}")))?;
    if frame.v != PROTOCOL_VERSION {
        return Err(CoreError::invalid(format!(
            "unsupported protocol version {} (expected {PROTOCOL_VERSION})",
            frame.v
        )));
    }
    Ok(frame)
}

pub fn decode_server(line: &str) -> Result<ServerFrame, CoreError> {
    if line.len() > MAX_FRAME_BYTES {
        return Err(CoreError::LimitExceeded("frame too large".into()));
    }
    serde_json::from_str(line).map_err(|e| CoreError::invalid(format!("bad frame: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_round_trip() {
        let frame = ClientFrame {
            v: PROTOCOL_VERSION,
            id: 9,
            request: Request::ListTasks {
                page: Page::new(0, 20),
            },
        };
        let line = encode(&frame).expect("encode");
        assert!(!line.contains('\n'));
        assert_eq!(decode_client(&line).expect("decode"), frame);
    }

    #[test]
    fn rejects_wrong_version_and_unknown_fields() {
        let bad_version = r#"{"v":2,"id":1,"request":{"method":"ping"}}"#;
        assert!(decode_client(bad_version).is_err());
        let extra = r#"{"v":1,"id":1,"request":{"method":"ping"},"admin":true}"#;
        assert!(decode_client(extra).is_err());
        let unknown_method = r#"{"v":1,"id":1,"request":{"method":"run_shell","params":{}}}"#;
        assert!(decode_client(unknown_method).is_err());
    }

    #[test]
    fn errors_cross_the_wire() {
        let frame = ServerFrame::Response {
            v: PROTOCOL_VERSION,
            id: 3,
            result: Err(CoreError::denied("nope").into()),
        };
        let back = decode_server(&encode(&frame).expect("encode")).expect("decode");
        match back {
            ServerFrame::Response {
                result: Err(err), ..
            } => {
                assert_eq!(CoreError::from(err).code(), ErrorCode::Denied)
            }
            other => panic!("unexpected {other:?}"),
        }
    }
}
