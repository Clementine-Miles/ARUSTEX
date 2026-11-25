//! Runtime tests against PostgreSQL with offline providers (mock and scripted).

use std::sync::Arc;
use std::time::Duration;

use arustex_core::domain::agent::{AgentRole, SessionId, SessionStatus, TerminalOutcome};
use arustex_core::domain::record::{IngestLimits, RecordInput};
use arustex_core::domain::task::{NewTask, TaskStatus};
use arustex_core::domain::{DatasetId, RecordId};
use arustex_core::ports::{RecordScope, SessionStore, TaskStore};
use arustex_core::{CoreError, Page};
use arustex_llm::{LlmError, MockProvider, Provider, ProviderRouter, ScriptedProvider};
use arustex_operations::Registry;
use arustex_policy::Policy;
use arustex_runtime::scheduler::TickReport;
use arustex_runtime::{
    AgentRunner, PromptRef, Scheduler, SchedulerSettings, SessionSpec, builtin_agents,
};
use arustex_store::Store;
use arustex_store::repository::records::IngestOptions;
use arustex_store::repository::work::TriggerSpec;
use arustex_store::testing::TestDb;
use serde_json::json;
use tokio_util::sync::CancellationToken;

macro_rules! test_db {
    () => {
        match TestDb::create().await {
            Some(db) => db,
            None => return,
        }
    };
}

async fn dataset(store: &Store, n: usize) -> DatasetId {
    let ds = store
        .create_dataset("synthetic", "", "fixture", 0, "test")
        .await
        .unwrap();
    let batch = (0..n)
        .map(|i| {
            (
                i as u64 + 1,
                RecordInput {
                    external_id: format!("r{i}"),
                    title: format!("synthetic record {i}"),
                    content_type: "text/plain".into(),
                    body: format!("synthetic body {i}"),
                    fields: json!({"level": if i % 2 == 0 { "warn" } else { "info" }}),
                },
            )
        })
        .collect();
    let opts = IngestOptions {
        source: "fixture".into(),
        redact: true,
        limits: IngestLimits::default(),
    };
    store.ingest_records(ds.id, batch, &opts).await.unwrap();
    ds.id
}

fn task(ds: DatasetId, batch: u32) -> NewTask {
    NewTask {
        name: "synthetic review".into(),
        objective: "Review synthetic records".into(),
        description: String::new(),
        dataset_ids: vec![ds],
        batch_size: batch,
        max_workers: 2,
        timeout_seconds: 0,
        token_budget: 0,
    }
}

fn runner(store: &Store, provider: Arc<dyn Provider>) -> AgentRunner {
    let mut router = ProviderRouter::new();
    router.insert(provider, true);
    let s = Arc::new(store.clone());
    AgentRunner::new(
        s.clone(),
        s.clone(),
        s,
        Arc::new(Registry::standard(Policy::standard())),
        router,
    )
}

fn worker_spec(
    task_id: arustex_core::domain::TaskId,
    ds: DatasetId,
    records: Vec<RecordId>,
) -> SessionSpec {
    let agent = builtin_agents()
        .into_iter()
        .find(|a| a.role == AgentRole::Worker)
        .unwrap();
    SessionSpec {
        id: SessionId::new(),
        task_id: Some(task_id),
        work_item_id: None,
        agent,
        prompt: PromptRef {
            key: "worker".into(),
            version: 0,
            system: "test".into(),
        },
        brief: "Review records".into(),
        scope: RecordScope {
            task_id: Some(task_id),
            dataset_ids: vec![ds],
            record_ids: Some(records),
        },
        resume: false,
    }
}

/// Answers from a script, then never answers again (a stalled provider).
#[derive(Debug)]
struct Hanging(ScriptedProvider);

#[async_trait::async_trait]
impl Provider for Hanging {
    fn name(&self) -> &str {
        "hanging"
    }
    fn model(&self) -> &str {
        "hanging"
    }
    fn context_window(&self) -> u32 {
        100_000
    }
    async fn complete(
        &self,
        request: &arustex_llm::ChatRequest,
        cancel: &CancellationToken,
        deltas: arustex_llm::DeltaSink,
    ) -> arustex_llm::LlmResult<arustex_llm::ChatResponse> {
        if self.0.remaining() == 0 {
            std::future::pending::<()>().await;
        }
        self.0.complete(request, cancel, deltas).await
    }
}

async fn run_until_idle(s: &Arc<Scheduler>, max_ticks: usize) -> Vec<TickReport> {
    let mut reports = Vec::new();
    for _ in 0..max_ticks {
        let r = s.tick().await.unwrap();
        let done = !r.finished_tasks.is_empty();
        reports.push(r);
        if done {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    reports
}

#[tokio::test]
async fn session_validates_results_and_returns_operation_errors_to_the_model() {
    let db = test_db!();
    let ds = dataset(&db.store, 4).await;
    let t = db.store.create_task(&task(ds, 10), "test").await.unwrap();
    let ids = db.store.task_record_ids(t.id).await.unwrap();
    let provider = Arc::new(ScriptedProvider::new([
        ScriptedProvider::tool_call("a", "propose_status_update", json!({})),
        ScriptedProvider::tool_call("b", "read_records", json!({"ids": [ids[0].0, 999_999]})),
        ScriptedProvider::tool_call(
            "c",
            "submit_result",
            json!({"summary": "x", "reviewed_record_ids": [ids[3].0 + 100], "confidence": "low"}),
        ),
        ScriptedProvider::tool_call(
            "d",
            "submit_result",
            json!({"summary": "ok", "reviewed_record_ids": [ids[0].0], "confidence": "high"}),
        ),
    ]));
    let r = runner(&db.store, provider.clone());
    let spec = worker_spec(t.id, ds, ids[..2].to_vec());
    let report = r
        .run(spec.clone(), CancellationToken::new(), None)
        .await
        .unwrap();
    assert_eq!(report.outcome, TerminalOutcome::Completed);
    assert_eq!(report.result.unwrap()["summary"], json!("ok"));
    assert_eq!(report.turns, 4);
    let reqs = provider.requests();
    // The worker never sees approval-gated operations.
    assert!(
        reqs[0]
            .tools
            .iter()
            .all(|t| t.name != "propose_status_update")
    );
    let last = reqs.last().unwrap().messages.last().unwrap();
    assert!(
        serde_json::to_string(last)
            .unwrap()
            .contains("not part of this session")
    );
    let denied = serde_json::to_string(&reqs[1].messages.last().unwrap()).unwrap();
    assert!(denied.contains("operation failed"), "{denied}");
    let session = db.store.get_session(spec.id).await.unwrap();
    assert_eq!(session.status, SessionStatus::Completed);
    let transcript = db.store.transcript(spec.id, 0, 100).await.unwrap();
    assert!(transcript.len() >= 10);
    db.drop_db().await;
}

#[tokio::test]
async fn sessions_end_on_budgets_cancellation_and_provider_errors() {
    let db = test_db!();
    let ds = dataset(&db.store, 2).await;
    let t = db.store.create_task(&task(ds, 10), "test").await.unwrap();
    let ids = db.store.task_record_ids(t.id).await.unwrap();

    let looping = Arc::new(ScriptedProvider::new((0..20).map(|i| {
        ScriptedProvider::tool_call(&format!("c{i}"), "summarize_scope", json!({}))
    })));
    let mut spec = worker_spec(t.id, ds, ids.clone());
    spec.agent.max_turns = 3;
    let report = runner(&db.store, looping)
        .run(spec, CancellationToken::new(), None)
        .await
        .unwrap();
    assert!(matches!(
        report.outcome,
        TerminalOutcome::BudgetExhausted { .. }
    ));

    let chatty = Arc::new(ScriptedProvider::new(
        (0..5).map(|_| ScriptedProvider::text("thinking")),
    ));
    let report = runner(&db.store, chatty)
        .run(
            worker_spec(t.id, ds, ids.clone()),
            CancellationToken::new(),
            None,
        )
        .await
        .unwrap();
    assert!(matches!(
        report.outcome,
        TerminalOutcome::NeedsReview { .. }
    ));

    let cancel = CancellationToken::new();
    cancel.cancel();
    let report = runner(&db.store, Arc::new(MockProvider::new("m", "m")))
        .run(worker_spec(t.id, ds, ids.clone()), cancel, None)
        .await
        .unwrap();
    assert_eq!(report.outcome, TerminalOutcome::Cancelled);

    let failing = Arc::new(ScriptedProvider::new([Err(LlmError::Http {
        status: 503,
        message: "x".into(),
        retry_after: None,
    })]));
    let spec = worker_spec(t.id, ds, ids);
    let err = runner(&db.store, failing)
        .run(spec.clone(), CancellationToken::new(), None)
        .await
        .unwrap_err();
    assert!(matches!(err, CoreError::Provider(_)) && err.is_transient());
    assert_eq!(
        db.store.get_session(spec.id).await.unwrap().status,
        SessionStatus::Failed
    );
    db.drop_db().await;
}

#[tokio::test]
async fn sessions_resume_from_checkpoints() {
    let db = test_db!();
    let ds = dataset(&db.store, 2).await;
    let t = db.store.create_task(&task(ds, 10), "test").await.unwrap();
    let ids = db.store.task_record_ids(t.id).await.unwrap();
    // First process: one successful turn, then the process dies while waiting on the
    // provider (the future is dropped, as on a crash).
    let first = Arc::new(Hanging(ScriptedProvider::new([
        ScriptedProvider::tool_call("a", "summarize_scope", json!({})),
    ])));
    let spec = worker_spec(t.id, ds, ids.clone());
    let r1 = runner(&db.store, first);
    let s1 = spec.clone();
    let handle = tokio::spawn(async move { r1.run(s1, CancellationToken::new(), None).await });
    for _ in 0..100 {
        if matches!(db.store.load_checkpoint(spec.id).await, Ok(Some(_))) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    handle.abort();
    let _ = handle.await;
    assert_eq!(
        db.store.get_session(spec.id).await.unwrap().status,
        SessionStatus::Running
    );
    let second = Arc::new(ScriptedProvider::new([ScriptedProvider::tool_call(
        "b",
        "submit_result",
        json!({"summary": "resumed", "reviewed_record_ids": [], "confidence": "medium"}),
    )]));
    let resumed = SessionSpec {
        resume: true,
        ..spec.clone()
    };
    let report = runner(&db.store, second.clone())
        .run(resumed, CancellationToken::new(), None)
        .await
        .unwrap();
    assert_eq!(report.outcome, TerminalOutcome::Completed);
    // The resumed request carries the history from the checkpoint.
    let msgs = &second.requests()[0].messages;
    assert!(msgs.len() >= 3, "{msgs:?}");
    db.drop_db().await;
}

#[tokio::test]
async fn scheduler_plans_reviews_and_completes_tasks_with_the_mock_provider() {
    let db = test_db!();
    let ds = dataset(&db.store, 7).await;
    let t = db.store.create_task(&task(ds, 3), "test").await.unwrap();
    db.store
        .transition_task(t.id, TaskStatus::Running, None)
        .await
        .unwrap();
    let store = Arc::new(db.store.clone());
    let settings = SchedulerSettings {
        max_concurrent_sessions: 3,
        ..Default::default()
    };
    let s = Arc::new(Scheduler::new(
        store.clone(),
        runner(&db.store, Arc::new(MockProvider::new("mock", "mock-1"))),
        settings,
    ));
    let reports = run_until_idle(&s, 200).await;
    let finished: Vec<_> = reports
        .iter()
        .flat_map(|r| r.finished_tasks.clone())
        .collect();
    assert_eq!(finished, vec![(t.id, TaskStatus::Completed)], "{reports:?}");
    let items = db.store.list_work_items(t.id, 100).await.unwrap();
    assert_eq!(
        items
            .iter()
            .filter(|i| i.kind == arustex_core::domain::work::WorkKind::Review)
            .count(),
        3
    );
    assert_eq!(
        items
            .iter()
            .filter(|i| i.kind == arustex_core::domain::work::WorkKind::Compare)
            .count(),
        3
    );
    let results = db
        .store
        .list_review_results(Some(t.id), Page::new(0, 50))
        .await
        .unwrap();
    assert_eq!(results.total, 3);
    let task = db.store.get_task(t.id).await.unwrap();
    assert!(task.tokens_used > 0);
    // Every record was planned exactly once.
    assert!(
        db.store
            .unplanned_record_ids(t.id, 100)
            .await
            .unwrap()
            .is_empty()
    );
    db.drop_db().await;
}

#[tokio::test]
async fn paused_tasks_release_work_and_resume() {
    let db = test_db!();
    let ds = dataset(&db.store, 4).await;
    let t = db.store.create_task(&task(ds, 2), "test").await.unwrap();
    db.store
        .transition_task(t.id, TaskStatus::Running, None)
        .await
        .unwrap();
    let store = Arc::new(db.store.clone());
    let settings = SchedulerSettings {
        planner_enabled: false,
        reviewer_enabled: false,
        ..Default::default()
    };
    let s = Arc::new(Scheduler::new(
        store,
        runner(&db.store, Arc::new(MockProvider::new("mock", "mock-1"))),
        settings,
    ));
    // First pass plans; then pause before any work is claimed.
    s.tick().await.unwrap();
    db.store
        .transition_task(t.id, TaskStatus::Paused, Some("operator".into()))
        .await
        .unwrap();
    let r = s.tick().await.unwrap();
    assert_eq!(r.started, 0, "paused tasks are not claimed");
    db.store
        .transition_task(t.id, TaskStatus::Running, Some("resume".into()))
        .await
        .unwrap();
    let reports = run_until_idle(&s, 200).await;
    assert!(
        reports
            .iter()
            .any(|r| r.finished_tasks == vec![(t.id, TaskStatus::Completed)]),
        "{reports:?}"
    );
    db.drop_db().await;
}

#[tokio::test]
async fn standing_tasks_plan_new_records_on_triggers() {
    let db = test_db!();
    let ds = dataset(&db.store, 2).await;
    let t = db.store.create_task(&task(ds, 10), "test").await.unwrap();
    db.store
        .create_trigger(t.id, TriggerSpec::RecordsImported, "test")
        .await
        .unwrap();
    db.store
        .transition_task(t.id, TaskStatus::Running, None)
        .await
        .unwrap();
    let settings = SchedulerSettings {
        planner_enabled: false,
        reviewer_enabled: false,
        ..Default::default()
    };
    let s = Arc::new(Scheduler::new(
        Arc::new(db.store.clone()),
        runner(&db.store, Arc::new(MockProvider::new("mock", "mock-1"))),
        settings,
    ));
    for _ in 0..40 {
        s.tick().await.unwrap();
        tokio::time::sleep(Duration::from_millis(30)).await;
    }
    // Idle standing task stays running.
    assert_eq!(
        db.store.get_task(t.id).await.unwrap().status,
        TaskStatus::Running
    );
    let before = db.store.list_work_items(t.id, 100).await.unwrap().len();
    let opts = IngestOptions {
        source: "fixture".into(),
        redact: true,
        limits: IngestLimits::default(),
    };
    db.store
        .ingest_records(
            ds,
            vec![(
                1,
                RecordInput {
                    external_id: "late".into(),
                    title: "late record".into(),
                    content_type: "text/plain".into(),
                    body: "arrived later".into(),
                    fields: json!({}),
                },
            )],
            &opts,
        )
        .await
        .unwrap();
    let mut planned = 0;
    for _ in 0..20 {
        planned += s.tick().await.unwrap().planned_items;
        tokio::time::sleep(Duration::from_millis(30)).await;
    }
    assert_eq!(planned, 1);
    assert_eq!(
        db.store.list_work_items(t.id, 100).await.unwrap().len(),
        before + 1
    );
    db.drop_db().await;
}
