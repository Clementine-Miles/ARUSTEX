//! Request handling shared by every IPC connection.

use std::sync::Arc;

use arustex_core::domain::FindingId;
use arustex_core::domain::agent::AgentRole;
use arustex_core::domain::agent::SessionId;
use arustex_core::domain::finding::FindingStatus;
use arustex_core::domain::task::TaskAction;
use arustex_core::domain::work::{Approval, ApprovalStatus};
use arustex_core::events::{DomainEvent, EventEnvelope};
use arustex_core::ipc::{DashboardSnapshot, PROTOCOL_VERSION, Request, Response, SettingsView};
use arustex_core::ports::{RecordScope, SessionStore, TaskStore, WorkQueue};
use arustex_core::{CoreError, CoreResult, Page};
use arustex_llm::StreamDelta;
use arustex_operations::PROPOSE_STATUS_UPDATE;
use arustex_runtime::{Scheduler, SessionSpec, TaskHooks};
use arustex_store::Store;
use chrono::{DateTime, Utc};
use tokio::sync::{Semaphore, mpsc};
use tokio_util::sync::CancellationToken;

use crate::reports;

/// Longest accepted assistant message.
pub const MAX_ASSISTANT_TEXT: usize = 8 * 1024;
/// Assistant sessions that may run at once.
const MAX_ASSISTANT_SESSIONS: usize = 2;

/// Who is calling: the verified peer of a connection.
#[derive(Debug, Clone)]
pub struct Caller {
    pub uid: u32,
}

impl Caller {
    pub fn actor(&self) -> String {
        format!("operator:uid{}", self.uid)
    }
}

#[derive(Debug)]
pub struct Services {
    store: Arc<Store>,
    scheduler: Option<Arc<Scheduler>>,
    hooks: Arc<dyn TaskHooks>,
    settings: SettingsView,
    started_at: DateTime<Utc>,
    assistant_slots: Arc<Semaphore>,
    shutdown: CancellationToken,
}

impl Services {
    pub fn new(
        store: Arc<Store>,
        scheduler: Option<Arc<Scheduler>>,
        hooks: Arc<dyn TaskHooks>,
        settings: SettingsView,
        shutdown: CancellationToken,
    ) -> Self {
        Self {
            store,
            scheduler,
            hooks,
            settings,
            started_at: Utc::now(),
            assistant_slots: Arc::new(Semaphore::new(MAX_ASSISTANT_SESSIONS)),
            shutdown,
        }
    }

    pub fn store(&self) -> &Arc<Store> {
        &self.store
    }

    /// Handles one request. `Subscribe` is handled by the connection itself.
    pub async fn handle(&self, caller: &Caller, request: Request) -> CoreResult<Response> {
        let s = &self.store;
        Ok(match request {
            Request::Hello { .. } => Response::Hello {
                protocol: PROTOCOL_VERSION,
                server: format!("arustex {}", env!("CARGO_PKG_VERSION")),
                uid: caller.uid,
            },
            Request::Ping => Response::Pong,
            Request::Dashboard => Response::Dashboard(Box::new(self.dashboard().await?)),
            Request::ListTasks { page } => Response::Tasks(s.list_tasks(page).await?),
            Request::GetTask { id } => Response::Task(Box::new(s.task_detail(id).await?)),
            Request::CreateTask { task } => {
                let created = s.create_task(&task, &caller.actor()).await?;
                Response::Task(Box::new(s.task_detail(created.id).await?))
            }
            Request::ControlTask { id, action } => {
                let task = s.get_task(id).await?;
                let target = action.target(task.status)?;
                let task = s
                    .transition_task(
                        id,
                        target,
                        Some(format!("{action:?} by {}", caller.actor())),
                    )
                    .await?;
                if action == TaskAction::Cancel {
                    s.cancel_task_items(id).await?;
                }
                if matches!(action, TaskAction::Cancel | TaskAction::Accept) {
                    self.hooks.task_finished(&task).await;
                }
                Response::Task(Box::new(s.task_detail(id).await?))
            }
            Request::ListSessions { task_id, page } => {
                Response::Sessions(s.list_sessions(task_id, page).await?)
            }
            Request::GetTranscript {
                session_id,
                after_seq,
                limit,
            } => Response::Transcript(s.transcript(session_id, after_seq, limit.min(500)).await?),
            Request::ListApprovals { status, page } => {
                Response::Approvals(s.list_approvals(status, page).await?)
            }
            Request::DecideApproval {
                id,
                approve,
                reason,
            } => Response::Approval(self.decide_approval(caller, id, approve, reason).await?),
            Request::ListAssets { query, page } => {
                Response::Assets(s.list_assets(query.as_deref(), page).await?)
            }
            Request::GetAsset { id } => Response::Asset(Box::new(s.asset_detail(id).await?)),
            Request::ListDatasets { page } => Response::Datasets(s.list_datasets(page).await?),
            Request::ListRecords {
                dataset_id,
                query,
                page,
            } => Response::Records(s.list_records(dataset_id, query.as_deref(), page).await?),
            Request::GetRecord { id } => Response::Record(Box::new(s.get_record(id).await?)),
            Request::ListFindings { task_id, page } => {
                Response::Findings(s.list_findings(task_id, page).await?)
            }
            Request::SetFindingStatus { id, status } => Response::Finding(Box::new(
                s.set_finding_status(id, status, &caller.actor()).await?,
            )),
            Request::ListReviewResults { task_id, page } => {
                Response::ReviewResults(s.list_review_results(task_id, page).await?)
            }
            Request::DecideReviewResult { id, confirm } => Response::ReviewResult(Box::new(
                s.decide_review_result(id, confirm, &caller.actor()).await?,
            )),
            Request::AddNote {
                subject,
                subject_id,
                body,
            } => Response::Note(
                s.add_note(subject, subject_id, &caller.actor(), &body)
                    .await?,
            ),
            Request::ListNotes {
                subject,
                subject_id,
            } => Response::Notes(s.list_notes(subject, subject_id).await?),
            Request::ListReports { task_id, page } => {
                Response::Reports(s.list_reports(task_id, page).await?)
            }
            Request::GetReport { id } => Response::Report(Box::new(s.get_report(id).await?)),
            Request::GenerateReport { task_id } => {
                Response::Report(Box::new(reports::generate_report(s, task_id).await?))
            }
            Request::GetSettings => Response::Settings(self.settings.clone()),
            Request::AssistantMessage { task_id, text } => {
                self.assistant(task_id, text).await?;
                Response::Accepted
            }
            Request::Subscribe { .. } => {
                return Err(CoreError::invalid("subscribe is handled by the connection"));
            }
        })
    }

    pub async fn dashboard(&self) -> CoreResult<DashboardSnapshot> {
        let s = &self.store;
        Ok(DashboardSnapshot {
            latest_seq: s.latest_event_seq().await?,
            daemon_version: env!("CARGO_PKG_VERSION").into(),
            started_at: self.started_at,
            tasks_by_status: s.task_counts_by_status().await?,
            active_sessions: s.active_session_count().await?,
            pending_approvals: s
                .list_approvals(Some(ApprovalStatus::Pending), Page::new(0, 1))
                .await?
                .total,
            pending_findings: s.finding_count(Some(FindingStatus::Pending)).await?,
            datasets: s.list_datasets(Page::new(0, 1)).await?.total,
            records: s.record_count().await?,
            tokens_today: s.tokens_today().await?,
            recent_events: s.recent_events(20).await?,
        })
    }

    /// Records the decision and, for an approved status proposal, applies it.
    async fn decide_approval(
        &self,
        caller: &Caller,
        id: arustex_core::domain::ApprovalId,
        approve: bool,
        reason: Option<String>,
    ) -> CoreResult<Approval> {
        let actor = caller.actor();
        let approval = self
            .store
            .decide_approval(id, approve, &actor, reason.as_deref())
            .await?;
        if !approve {
            return Ok(approval);
        }
        let outcome = match approval.operation.as_str() {
            PROPOSE_STATUS_UPDATE => self.apply_status_update(&approval, &actor).await,
            other => Err(CoreError::denied(format!(
                "no handler applies approvals for {other:?}"
            ))),
        };
        match outcome {
            Ok(()) => self.store.finish_approval(id, true, None).await?,
            Err(e) => {
                self.store
                    .finish_approval(id, false, Some(&e.to_string()))
                    .await?
            }
        }
        self.store.get_approval(id).await
    }

    async fn apply_status_update(&self, approval: &Approval, actor: &str) -> CoreResult<()> {
        #[derive(serde::Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Proposal {
            finding_id: FindingId,
            status: FindingStatus,
        }
        let p: Proposal = serde_json::from_value(approval.request.clone())
            .map_err(|e| CoreError::invalid(format!("malformed proposal: {e}")))?;
        let finding = self.store.get_finding(p.finding_id).await?;
        if approval.task_id.is_some() && finding.task_id != approval.task_id {
            return Err(CoreError::denied("the finding belongs to another task"));
        }
        self.store
            .set_finding_status(p.finding_id, p.status, actor)
            .await
            .map(|_| ())
    }

    /// Starts an assistant session for a task. Output streams as transient deltas and
    /// the transcript; the call returns as soon as the session is accepted.
    async fn assistant(
        &self,
        task_id: arustex_core::domain::TaskId,
        text: String,
    ) -> CoreResult<()> {
        let text = text.trim().to_owned();
        if text.is_empty() || text.len() > MAX_ASSISTANT_TEXT {
            return Err(CoreError::invalid(format!(
                "assistant message must be 1..={MAX_ASSISTANT_TEXT} bytes"
            )));
        }
        let Some(scheduler) = self.scheduler.clone() else {
            return Err(CoreError::Unavailable("the scheduler is disabled".into()));
        };
        let Some(agent) = scheduler.agent(AgentRole::Assistant).cloned() else {
            return Err(CoreError::denied("the assistant agent is disabled"));
        };
        let task = self.store.get_task(task_id).await?;
        let permit = self
            .assistant_slots
            .clone()
            .try_acquire_owned()
            .map_err(|_| CoreError::LimitExceeded("the assistant is busy".into()))?;
        let spec = SessionSpec {
            id: SessionId::new(),
            task_id: Some(task.id),
            work_item_id: None,
            prompt: scheduler.prompt(&agent, &task).await?,
            agent,
            brief: format!(
                "The operator asks about this task. Answer from the task's records using \
                 the available operations, then submit your answer.\n\nOperator message:\n{text}"
            ),
            scope: RecordScope {
                task_id: Some(task.id),
                dataset_ids: task.dataset_ids.clone(),
                record_ids: None,
            },
            resume: false,
        };
        let session_id = spec.id;
        let (tx, mut rx) = mpsc::channel::<StreamDelta>(256);
        let store = self.store.clone();
        tokio::spawn(async move {
            while let Some(delta) = rx.recv().await {
                if let StreamDelta::Text(text) = delta {
                    store.publish_transient(EventEnvelope {
                        seq: 0,
                        at: Utc::now(),
                        task_id: Some(task_id),
                        session_id: Some(session_id),
                        event: DomainEvent::OutputDelta { session_id, text },
                    });
                }
            }
        });
        let cancel = self.shutdown.child_token();
        tokio::spawn(async move {
            let _permit = permit;
            match scheduler.runner().run(spec, cancel, Some(tx)).await {
                Ok(r) => {
                    tracing::info!(session = %r.id, outcome = ?r.outcome, "assistant session finished")
                }
                Err(e) => tracing::warn!(error = %e, "assistant session failed"),
            }
        });
        Ok(())
    }
}
