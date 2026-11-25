//! PostgreSQL integration tests. They run against `ARUSTEX_TEST_DATABASE_URL` and are
//! skipped when it is unset (CI sets `ARUSTEX_REQUIRE_PG=1`).

use std::collections::BTreeSet;
use std::time::Duration;

use arustex_core::domain::agent::{
    AgentRole, NewSession, SessionId, SessionStatus, TerminalOutcome, TokenUsage, TranscriptKind,
};
use arustex_core::domain::asset::{AssetEdge, AssetKind, AssetRelation, NewAsset};
use arustex_core::domain::finding::{FindingStatus, NewFinding, Severity, Verdict};
use arustex_core::domain::record::{AttachmentOwner, IngestLimits, RecordInput};
use arustex_core::domain::task::{NewTask, TaskStatus};
use arustex_core::domain::work::{NewApproval, NewWorkItem, WorkKind, WorkStatus};
use arustex_core::domain::{DatasetId, Provenance, RecordId};
use arustex_core::ports::{
    RecordScope, RecordSource, ReviewSink, SessionStore, TaskStore, WorkQueue,
};
use arustex_core::{CoreError, Page};
use arustex_store::attachments::{AttachmentLimits, NewAttachment};
use arustex_store::repository::records::IngestOptions;
use arustex_store::repository::work::TriggerSpec;
use arustex_store::testing::TestDb;
use arustex_store::Store;

macro_rules! test_db {
    () => {
        match TestDb::create().await {
            Some(db) => db,
            None => return,
        }
    };
}

fn input(id: &str, body: &str) -> RecordInput {
    RecordInput {
        external_id: id.into(),
        title: format!("record {id}"),
        content_type: "text/plain".into(),
        body: body.into(),
        fields: serde_json::json!({"n": id}),
    }
}

fn options() -> IngestOptions {
    IngestOptions { source: "fixture".into(), redact: true, limits: IngestLimits::default() }
}

async fn dataset_with_records(store: &Store, n: usize) -> DatasetId {
    let ds = store.create_dataset("synthetic", "", "fixture", 0, "test").await.unwrap();
    let batch = (0..n)
        .map(|i| (i as u64 + 1, input(&format!("r{i}"), &format!("synthetic body number {i} alpha"))))
        .collect();
    store.ingest_records(ds.id, batch, &options()).await.unwrap();
    ds.id
}

fn new_task(datasets: Vec<DatasetId>) -> NewTask {
    NewTask {
        name: "review".into(),
        objective: "Review synthetic records".into(),
        description: String::new(),
        dataset_ids: datasets,
        batch_size: 10,
        max_workers: 2,
        timeout_seconds: 0,
        token_budget: 0,
    }
}

#[tokio::test]
async fn migrations_apply_and_are_idempotent() {
    let db = test_db!();
    db.store.migrate().await.unwrap();
    let status = db.store.migration_status().await.unwrap();
    assert_eq!(status.len(), 5);
    assert!(status.iter().all(|m| m.applied));
    db.store.ensure_migrated().await.unwrap();
    db.drop_db().await;
}

#[tokio::test]
async fn ingestion_dedups_redacts_and_searches() {
    let db = test_db!();
    let s = &db.store;
    let ds = s.create_dataset("d1", "desc", "fixture", 30, "test").await.unwrap();
    let batch = vec![
        (1, input("a", "plain text about otters")),
        (2, input("b", "contact jane@example.org token=abc123secret")),
    ];
    let first = s.ingest_records(ds.id, batch.clone(), &options()).await.unwrap();
    assert_eq!((first.inserted, first.duplicates, first.redacted), (2, 0, 1));
    let again = s.ingest_records(ds.id, batch, &options()).await.unwrap();
    assert_eq!((again.inserted, again.updated, again.duplicates), (0, 0, 2));
    let changed = s
        .ingest_records(ds.id, vec![(1, input("a", "plain text about beavers"))], &options())
        .await
        .unwrap();
    assert_eq!(changed.updated, 1);

    let found = s.list_records(Some(ds.id), Some("beavers"), Page::default()).await.unwrap();
    assert_eq!(found.total, 1);
    let rec = s.get_record(found.items[0].id).await.unwrap();
    assert_eq!(rec.external_id, "a");
    let all = s.list_records(Some(ds.id), None, Page::default()).await.unwrap();
    let b = all.items.iter().find(|r| r.external_id == "b").unwrap();
    let b = s.get_record(b.id).await.unwrap();
    assert!(b.redacted);
    assert!(!b.body.contains("jane@example.org"));
    assert!(!b.body.contains("abc123secret"));
    assert_eq!(s.get_dataset(ds.id).await.unwrap().record_count, 2);

    let bad = s
        .ingest_records(ds.id, vec![(7, RecordInput { content_type: "text/html".into(), ..input("c", "x") })], &options())
        .await
        .unwrap_err();
    assert!(matches!(bad, CoreError::Invalid(ref m) if m.contains("line 7")));
    db.drop_db().await;
}

#[tokio::test]
async fn task_lifecycle_graph_and_events() {
    let db = test_db!();
    let s = &db.store;
    let ds = dataset_with_records(s, 5).await;
    let mut live = s.subscribe_live();
    let before = s.latest_event_seq().await.unwrap();
    let task = s.create_task(&new_task(vec![ds]), "test").await.unwrap();
    assert_eq!(task.status, TaskStatus::Created);
    assert!(s.check_exploration(task.id).await.unwrap().is_empty());

    let err = s.transition_task(task.id, TaskStatus::Completed, None).await.unwrap_err();
    assert!(matches!(err, CoreError::Conflict(_) | CoreError::Invalid(_)));
    s.transition_task(task.id, TaskStatus::Queued, None).await.unwrap();
    let running = s.transition_task(task.id, TaskStatus::Running, None).await.unwrap();
    assert!(running.started_at.is_some());

    let records = s.task_record_ids(task.id).await.unwrap();
    assert_eq!(records.len(), 5);
    let node = s.plan_node(task.id, "batch 1", &records[..3]).await.unwrap();
    s.set_node_state(node, "done").await.unwrap();
    assert!(s.set_node_state(node, "met").await.is_err());
    assert!(s.check_exploration(task.id).await.unwrap().is_empty());
    let cov = s.task_coverage(task.id).await.unwrap();
    assert_eq!((cov.total, cov.reviewed), (5, 3));

    let events = s.events_after(before, 100).await.unwrap();
    assert!(events.windows(2).all(|w| w[0].seq < w[1].seq));
    assert!(events.iter().any(|e| e.event.kind() == "task_created"));
    let first_live = live.recv().await.unwrap();
    assert_eq!(first_live.seq, events[0].seq);
    db.drop_db().await;
}

#[tokio::test]
async fn asset_graph_invariants() {
    let db = test_db!();
    let s = &db.store;
    let asset = |kind, label: &str| NewAsset {
        kind,
        label: label.into(),
        identity_key: None,
        attributes: serde_json::json!({"a": 1}),
        provenance: Provenance::operator("test"),
        preserve_id: None,
    };
    let org = s.upsert_asset(&asset(AssetKind::Organization, "Example Org")).await.unwrap().id();
    let col = s.upsert_asset(&asset(AssetKind::Collection, "Docs")).await.unwrap().id();
    let doc = s.upsert_asset(&asset(AssetKind::Document, "Doc 1")).await.unwrap().id();
    let merged = s
        .upsert_asset(&NewAsset { attributes: serde_json::json!({"b": 2}), ..asset(AssetKind::Organization, "  example   org. ") })
        .await
        .unwrap();
    assert_eq!(merged.id(), org);
    let attrs = s.get_asset(org).await.unwrap().attributes;
    assert_eq!(attrs, serde_json::json!({"a": 1, "b": 2}));

    let contains = |src, dst| AssetEdge { src, dst, rel: AssetRelation::Contains };
    s.add_asset_edge(contains(org, col)).await.unwrap();
    s.add_asset_edge(contains(col, doc)).await.unwrap();
    assert!(s.add_asset_edge(contains(doc, org)).await.is_err(), "cycle rejected");
    let other = s.upsert_asset(&asset(AssetKind::Collection, "Other")).await.unwrap().id();
    assert!(s.add_asset_edge(contains(other, doc)).await.is_err(), "second parent rejected");
    let dup = s.upsert_asset(&asset(AssetKind::Document, "Doc 1 copy")).await.unwrap().id();
    s.add_asset_edge(AssetEdge { src: dup, dst: doc, rel: AssetRelation::DuplicateOf }).await.unwrap();
    let dup2 = s.upsert_asset(&asset(AssetKind::Document, "Doc 1 copy 2")).await.unwrap().id();
    assert!(
        s.add_asset_edge(AssetEdge { src: dup2, dst: dup, rel: AssetRelation::DuplicateOf }).await.is_err(),
        "duplicate chains rejected"
    );
    let detail = s.asset_detail(doc).await.unwrap();
    assert_eq!(detail.ancestry.len(), 2);
    assert!(s.check_asset_graph().await.unwrap().is_empty());
    db.drop_db().await;
}

#[tokio::test]
async fn work_queue_leases_retries_and_recovery() {
    let db = test_db!();
    let s = &db.store;
    let ds = dataset_with_records(s, 4).await;
    let task = s.create_task(&new_task(vec![ds]), "test").await.unwrap();
    let records = s.task_record_ids(task.id).await.unwrap();
    let items: Vec<NewWorkItem> = records
        .iter()
        .map(|r| NewWorkItem { task_id: task.id, kind: WorkKind::Review, record_ids: vec![*r], priority: 0, max_attempts: 2, node_id: None })
        .collect();
    s.enqueue(items).await.unwrap();
    let lease = Duration::from_secs(30);
    assert!(s.claim("w1", None, lease).await.unwrap().is_none(), "task not running");
    s.transition_task(task.id, TaskStatus::Queued, None).await.unwrap();
    s.transition_task(task.id, TaskStatus::Running, None).await.unwrap();

    // Concurrent claims never hand out the same item, and max_workers (2) bounds leases.
    let mut handles = Vec::new();
    for i in 0..6 {
        let store = s.clone();
        handles.push(tokio::spawn(async move { store.claim(&format!("w{i}"), None, lease).await.unwrap() }));
    }
    let mut claimed = Vec::new();
    for h in handles {
        if let Some(item) = h.await.unwrap() {
            claimed.push(item);
        }
    }
    let ids: BTreeSet<_> = claimed.iter().map(|i| i.id).collect();
    assert_eq!(ids.len(), claimed.len());
    assert!(!claimed.is_empty() && claimed.len() <= 2, "claimed {}", claimed.len());

    let a = &claimed[0];
    assert!(s.heartbeat(a.id, &a.lease_owner.clone().unwrap(), lease).await.unwrap());
    assert!(!s.heartbeat(a.id, "intruder", lease).await.unwrap());
    let owner = a.lease_owner.clone().unwrap();
    assert_eq!(s.fail(a.id, &owner, "boom", true).await.unwrap(), WorkStatus::Queued);
    assert!(s.complete(a.id, &owner, serde_json::json!({})).await.is_err(), "lease lost");

    // Simulate a crashed worker: expire its lease and recover.
    if let Some(b) = claimed.get(1) {
        sqlx::query("UPDATE work_items SET lease_expires_at = now() - interval '1 minute' WHERE id = $1")
            .bind(b.id.0)
            .execute(s.pool())
            .await
            .unwrap();
        assert_eq!(s.recover_expired().await.unwrap(), 1);
    }
    let mut done = 0;
    while let Some(item) = s.claim("w9", Some(task.id), lease).await.unwrap() {
        s.complete(item.id, "w9", serde_json::json!({"ok": true})).await.unwrap();
        done += 1;
    }
    assert_eq!(done, 4);
    let counts = s.counts(task.id).await.unwrap();
    assert_eq!((counts.done, counts.queued, counts.leased), (4, 0, 0));
    db.drop_db().await;
}

#[tokio::test]
async fn triggers_fire_on_interval_and_import() {
    let db = test_db!();
    let s = &db.store;
    let ds = dataset_with_records(s, 1).await;
    let task = s.create_task(&new_task(vec![ds]), "test").await.unwrap();
    assert!(s.create_trigger(task.id, TriggerSpec::Interval { seconds: 10 }, "t").await.is_err());
    s.create_trigger(task.id, TriggerSpec::Interval { seconds: 60 }, "t").await.unwrap();
    s.create_trigger(task.id, TriggerSpec::RecordsImported, "t").await.unwrap();
    assert_eq!(s.fire_due_triggers().await.unwrap(), vec![task.id]);
    assert!(s.fire_due_triggers().await.unwrap().is_empty());
    s.ingest_records(ds, vec![(1, input("new", "new body"))], &options()).await.unwrap();
    assert_eq!(s.fire_due_triggers().await.unwrap(), vec![task.id]);
    assert!(s.fire_due_triggers().await.unwrap().is_empty());
    db.drop_db().await;
}

#[tokio::test]
async fn sessions_transcripts_findings_and_approvals() {
    let db = test_db!();
    let s = &db.store;
    let ds = dataset_with_records(s, 2).await;
    let task = s.create_task(&new_task(vec![ds]), "test").await.unwrap();
    let sid = SessionId::new();
    s.create_session(NewSession {
        id: sid,
        task_id: Some(task.id),
        work_item_id: None,
        role: AgentRole::Worker,
        agent_key: "worker".into(),
        prompt_key: "worker".into(),
        prompt_version: 1,
        model_profile: "mock".into(),
        model: "mock-1".into(),
        token_budget: 1000,
        max_turns: 4,
    })
    .await
    .unwrap();
    let e1 = s.append_transcript(sid, TranscriptKind::User, serde_json::json!({"text": "hi"}), TokenUsage::default()).await.unwrap();
    let e2 = s.append_transcript(sid, TranscriptKind::Assistant, serde_json::json!({"text": "ok"}), TokenUsage { input_tokens: 3, output_tokens: 4, ..Default::default() }).await.unwrap();
    assert_eq!((e1.seq, e2.seq), (1, 2));
    assert_eq!(s.transcript(sid, 1, 10).await.unwrap().len(), 1);
    s.save_checkpoint(sid, serde_json::json!({"turn": 1})).await.unwrap();
    assert_eq!(s.load_checkpoint(sid).await.unwrap(), Some(serde_json::json!({"turn": 1})));
    s.update_session_progress(sid, SessionStatus::Running, 1, 7).await.unwrap();
    assert_eq!(s.interrupted_sessions().await.unwrap().len(), 1);
    s.finish_session(sid, &TerminalOutcome::Completed).await.unwrap();
    s.finish_session(sid, &TerminalOutcome::Failed { reason: "late".into() }).await.unwrap();
    let session = s.get_session(sid).await.unwrap();
    assert_eq!(session.status, SessionStatus::Completed);
    assert!(s.save_checkpoint(sid, serde_json::json!({})).await.is_err());

    let records = s.task_record_ids(task.id).await.unwrap();
    let finding = s
        .record_finding(NewFinding {
            task_id: Some(task.id),
            node_id: None,
            title: "Mismatch in synthetic record".into(),
            category: "consistency".into(),
            severity: Severity::Medium,
            summary: "Fields disagree".into(),
            record_ids: records.clone(),
            asset_ids: vec![],
            provenance: Provenance::generated(sid.0),
            status: Some(FindingStatus::Confirmed),
            preserve_id: None,
        })
        .await
        .unwrap();
    assert_eq!(finding.status, FindingStatus::Pending, "generated findings start pending");
    assert_eq!(finding.record_ids, records);
    let confirmed = s.set_finding_status(finding.id, FindingStatus::Confirmed, "operator").await.unwrap();
    assert_eq!(confirmed.status, FindingStatus::Confirmed);

    let approval = s
        .request_approval(NewApproval {
            task_id: Some(task.id),
            session_id: Some(sid),
            operation: "propose_db_update".into(),
            summary: "set finding status".into(),
            request: serde_json::json!({"finding": finding.id}),
        })
        .await
        .unwrap();
    s.decide_approval(approval.id, true, "operator", None).await.unwrap();
    assert!(matches!(s.decide_approval(approval.id, false, "operator", None).await, Err(CoreError::Conflict(_))));
    s.finish_approval(approval.id, true, None).await.unwrap();

    let rr = s.record_review_result(task.id, None, Verdict::Match, serde_json::json!({})).await.unwrap();
    s.decide_review_result(rr.id, true, "operator").await.unwrap();
    assert!(s.decide_review_result(rr.id, false, "operator").await.is_err());

    let note = s.add_note(arustex_core::domain::finding::NoteSubject::Finding, finding.id.0, "operator", "looks right").await.unwrap();
    assert_eq!(note.body, "looks right");
    assert!(s.add_note(arustex_core::domain::finding::NoteSubject::Finding, 999_999, "operator", "x").await.is_err());

    let report = s.save_report_draft(task.id, "Draft", "# Draft", sid).await.unwrap();
    assert_eq!(s.finalize_report(report.id, "operator").await.unwrap().finalized_at.is_some(), true);
    db.drop_db().await;
}

#[tokio::test]
async fn record_scope_is_enforced() {
    let db = test_db!();
    let s = &db.store;
    let ds = dataset_with_records(s, 3).await;
    let other = s.create_dataset("other", "", "fixture", 0, "t").await.unwrap();
    s.ingest_records(other.id, vec![(1, input("x", "outside alpha"))], &options()).await.unwrap();
    let scope = RecordScope { task_id: None, dataset_ids: vec![ds], record_ids: None };
    let hits = s.search_records(&scope, "alpha", 50).await.unwrap();
    assert_eq!(hits.len(), 3);
    let outside = s.list_records(Some(other.id), None, Page::default()).await.unwrap().items[0].id;
    assert!(matches!(s.get_records(&scope, &[outside]).await, Err(CoreError::Denied(_))));
    let narrowed = RecordScope { record_ids: Some(vec![hits[0].id]), ..scope.clone() };
    assert!(s.get_records(&narrowed, &[hits[1].id]).await.is_err());
    assert_eq!(s.get_records(&narrowed, &[hits[0].id]).await.unwrap().len(), 1);
    let empty = RecordScope::default();
    assert!(s.search_records(&empty, "", 10).await.is_err());
    let stats = s.scope_stats(&scope).await.unwrap();
    assert_eq!(stats.records, 3);
    let _ = RecordId(0);
    db.drop_db().await;
}

#[tokio::test]
async fn attachments_dedup_verify_and_gc() {
    let db = test_db!();
    let s = &db.store;
    let payload: Vec<u8> = (0..700_000u32).map(|i| (i % 251) as u8).collect();
    let meta = NewAttachment {
        owner: AttachmentOwner::Task,
        owner_id: 1,
        name: "../../etc/evidence.bin".into(),
        media_type: "application/octet-stream".into(),
        retention_days: 0,
    };
    let limits = AttachmentLimits { max_bytes: 1 << 20, chunk_size: 64 * 1024 };
    let a = s.put_attachment(&meta, payload.as_slice(), limits, "t").await.unwrap();
    let b = s.put_attachment(&meta, payload.as_slice(), limits, "t").await.unwrap();
    assert_eq!(a.name, "evidence.bin");
    assert_eq!(a.content_hash, b.content_hash);
    let blobs: i64 = sqlx::query_scalar("SELECT count(*) FROM blobs").fetch_one(s.pool()).await.unwrap();
    assert_eq!(blobs, 1);
    assert_eq!(s.read_attachment_bytes(a.id).await.unwrap(), payload);

    let too_big = AttachmentLimits { max_bytes: 1000, chunk_size: 4096 };
    assert!(matches!(s.put_attachment(&meta, payload.as_slice(), too_big, "t").await, Err(CoreError::LimitExceeded(_))));
    let bad_type = NewAttachment { media_type: "text/html".into(), ..meta.clone() };
    assert!(s.put_attachment(&bad_type, &b"x"[..], limits, "t").await.is_err());

    sqlx::query("UPDATE blob_chunks SET data = '\\x00' WHERE idx = 3").execute(s.pool()).await.unwrap();
    assert!(matches!(s.read_attachment_bytes(a.id).await, Err(CoreError::Integrity(_))));
    assert_eq!(s.verify_blobs().await.unwrap().len(), 1);

    s.delete_attachment(a.id, "t").await.unwrap();
    assert_eq!(s.collect_garbage().await.unwrap(), (0, 0));
    s.delete_attachment(b.id, "t").await.unwrap();
    assert_eq!(s.collect_garbage().await.unwrap(), (0, 1));
    db.drop_db().await;
}

#[tokio::test]
async fn backup_restore_round_trip() {
    let source = test_db!();
    let s = &source.store;
    let ds = dataset_with_records(s, 7).await;
    let task = s.create_task(&new_task(vec![ds]), "test").await.unwrap();
    let meta = NewAttachment {
        owner: AttachmentOwner::Task,
        owner_id: task.id.0,
        name: "a.txt".into(),
        media_type: "text/plain".into(),
        retention_days: 0,
    };
    s.put_attachment(&meta, &b"hello attachment"[..], AttachmentLimits::default(), "t").await.unwrap();
    let dir = tempfile::tempdir().unwrap();
    let manifest = s.backup_to(dir.path()).await.unwrap();
    assert_eq!(manifest.tables.iter().find(|t| t.table == "records").unwrap().rows, 7);
    assert!(s.backup_to(dir.path()).await.is_err(), "refuses non-empty directory");
    Store::verify_backup(dir.path()).await.unwrap();

    let target = TestDb::create().await.unwrap();
    target.store.restore_from(dir.path()).await.unwrap();
    let t = &target.store;
    assert_eq!(t.record_count().await.unwrap(), 7);
    assert_eq!(t.get_task(task.id).await.unwrap().objective, task.objective);
    let att = t.list_attachments(AttachmentOwner::Task, task.id.0).await.unwrap();
    assert_eq!(t.read_attachment_bytes(att[0].id).await.unwrap(), b"hello attachment");
    assert_eq!(t.latest_event_seq().await.unwrap(), s.latest_event_seq().await.unwrap());
    // Identity sequences continue after restored ids.
    let next = t.create_task(&new_task(vec![ds]), "test").await.unwrap();
    assert!(next.id > task.id);
    assert!(t.restore_from(dir.path()).await.is_err(), "refuses non-empty database");

    // A tampered file fails verification.
    let path = dir.path().join("tasks.jsonl");
    let mut data = std::fs::read(&path).unwrap();
    data[2] ^= 1;
    std::fs::write(&path, data).unwrap();
    assert!(matches!(Store::verify_backup(dir.path()).await, Err(CoreError::Integrity(_))));
    target.drop_db().await;
    source.drop_db().await;
}
