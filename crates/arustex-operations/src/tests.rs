use std::sync::Mutex;

use arustex_core::domain::agent::AgentRole;
use arustex_core::domain::exploration::ExplorationSnapshot;
use arustex_core::domain::finding::{Finding, FindingStatus};
use arustex_core::domain::record::ImportedRecord;
use arustex_core::domain::report::{Report, ReportStatus};
use arustex_core::domain::work::{Approval, ApprovalStatus, NewApproval};
use arustex_core::domain::{
    ApprovalId, ContentHash, DatasetId, FindingId, Origin, Provenance, RecordId, ReportId, TaskId,
};
use arustex_core::ports::{RecordScope, RecordSummary, ScopeStats};
use serde_json::json;

use super::*;

#[derive(Default)]
struct Fake {
    records: Vec<ImportedRecord>,
    findings: Mutex<Vec<arustex_core::domain::finding::NewFinding>>,
    approvals: Mutex<Vec<NewApproval>>,
}

fn record(
    id: i64,
    dataset: i64,
    title: &str,
    fields: serde_json::Value,
    body: &str,
) -> ImportedRecord {
    ImportedRecord {
        id: RecordId(id),
        dataset_id: DatasetId(dataset),
        external_id: format!("ext-{id}"),
        title: title.into(),
        content_type: "text/plain".into(),
        body: body.into(),
        fields,
        content_hash: ContentHash::of(body.as_bytes()),
        redacted: false,
        provenance: Provenance::supplied("fixture", None),
        imported_at: chrono::Utc::now(),
    }
}

#[async_trait]
impl RecordSource for Fake {
    async fn search_records(
        &self,
        scope: &RecordScope,
        query: &str,
        limit: u32,
    ) -> CoreResult<Vec<RecordSummary>> {
        if scope.dataset_ids.is_empty() {
            return Err(CoreError::denied("empty scope"));
        }
        Ok(self
            .records
            .iter()
            .filter(|r| scope.allows(r) && r.title.contains(query))
            .take(limit as usize)
            .map(|r| RecordSummary {
                id: r.id,
                dataset_id: r.dataset_id,
                external_id: r.external_id.clone(),
                title: r.title.clone(),
                content_type: r.content_type.clone(),
                body_bytes: r.body.len() as u64,
            })
            .collect())
    }

    async fn get_records(
        &self,
        scope: &RecordScope,
        ids: &[RecordId],
    ) -> CoreResult<Vec<ImportedRecord>> {
        let mut out = Vec::new();
        for id in ids {
            match self.records.iter().find(|r| r.id == *id) {
                Some(r) if scope.allows(r) => out.push(r.clone()),
                _ => {
                    return Err(CoreError::denied(format!(
                        "record {id} is outside the approved scope"
                    )));
                }
            }
        }
        Ok(out)
    }

    async fn scope_stats(&self, scope: &RecordScope) -> CoreResult<ScopeStats> {
        let n = self.records.iter().filter(|r| scope.allows(r)).count() as u64;
        Ok(ScopeStats {
            records: n,
            total_body_bytes: 0,
            by_content_type: vec![("text/plain".into(), n)],
            findings: 0,
        })
    }

    async fn exploration_snapshot(&self, _task_id: TaskId) -> CoreResult<ExplorationSnapshot> {
        Ok(ExplorationSnapshot::default())
    }
}

#[async_trait]
impl ReviewSink for Fake {
    async fn record_finding(
        &self,
        f: arustex_core::domain::finding::NewFinding,
    ) -> CoreResult<Finding> {
        let now = chrono::Utc::now();
        let out = Finding {
            id: FindingId(1),
            task_id: f.task_id,
            node_id: None,
            title: f.title.clone(),
            category: f.category.clone(),
            severity: f.severity,
            status: FindingStatus::Pending,
            summary: f.summary.clone(),
            record_ids: f.record_ids.clone(),
            asset_ids: vec![],
            provenance: f.provenance.clone(),
            created_at: now,
            updated_at: now,
        };
        self.findings.lock().unwrap().push(f);
        Ok(out)
    }

    async fn request_approval(&self, a: NewApproval) -> CoreResult<Approval> {
        let out = Approval {
            id: ApprovalId(7),
            task_id: a.task_id,
            session_id: a.session_id,
            operation: a.operation.clone(),
            summary: a.summary.clone(),
            request: a.request.clone(),
            status: ApprovalStatus::Pending,
            decided_by: None,
            reason: None,
            created_at: chrono::Utc::now(),
            decided_at: None,
        };
        self.approvals.lock().unwrap().push(a);
        Ok(out)
    }

    async fn save_report_draft(
        &self,
        task_id: TaskId,
        title: &str,
        body: &str,
        _s: SessionId,
    ) -> CoreResult<Report> {
        Ok(Report {
            id: ReportId(3),
            task_id,
            title: title.into(),
            status: ReportStatus::Draft,
            origin: Origin::ModelGenerated,
            body_markdown: body.into(),
            document: json!({}),
            created_at: chrono::Utc::now(),
            finalized_at: None,
        })
    }
}

fn setup(role: AgentRole) -> (Registry, OperationContext, Arc<Fake>) {
    let fake = Arc::new(Fake {
        records: vec![
            record(
                1,
                10,
                "alpha login",
                json!({"level": "warn"}),
                "synthetic body one",
            ),
            record(
                2,
                10,
                "alpha logout",
                json!({"level": "info"}),
                "synthetic body two",
            ),
            record(3, 10, "beta", json!({"level": "warn"}), &"x".repeat(9000)),
            record(4, 99, "alpha outside", json!({}), "other dataset"),
        ],
        ..Default::default()
    });
    let ctx = OperationContext {
        session_id: SessionId(uuid::Uuid::nil()),
        policy: PolicyContext {
            role,
            scope: RecordScope {
                task_id: Some(TaskId(5)),
                dataset_ids: vec![DatasetId(10)],
                record_ids: None,
            },
        },
        records: fake.clone(),
        review: fake.clone(),
    };
    (Registry::standard(Policy::standard()), ctx, fake)
}

#[test]
fn registry_is_static_and_contains_no_execution_or_network_operations() {
    let (reg, _, _) = setup(AgentRole::Worker);
    let names: Vec<_> = reg.descriptors().map(|d| d.name).collect();
    assert_eq!(names.len(), 9);
    for banned in [
        "shell", "exec", "http", "fetch", "file", "command", "plugin", "browser", "scan",
    ] {
        assert!(
            names.iter().all(|n| !n.contains(banned)),
            "{banned} in {names:?}"
        );
    }
    for d in reg.descriptors() {
        assert!(d.timeout <= Duration::from_secs(30));
        assert!(d.max_output_bytes <= 64 * 1024);
    }
}

#[test]
fn tools_are_filtered_per_role_and_schemas_reject_unknown_fields() {
    let (reg, _, _) = setup(AgentRole::Worker);
    let worker: Vec<_> = reg
        .tools_for(AgentRole::Worker)
        .into_iter()
        .map(|t| t.name)
        .collect();
    assert!(worker.contains(&"record_finding".to_owned()));
    assert!(!worker.contains(&"propose_status_update".to_owned()));
    let reviewer: Vec<_> = reg
        .tools_for(AgentRole::Reviewer)
        .into_iter()
        .map(|t| t.name)
        .collect();
    assert!(reviewer.contains(&"compare_outputs".to_owned()));
    assert!(!reviewer.contains(&"record_finding".to_owned()));
    let tools = reg.tools_for(AgentRole::Assistant);
    let status = tools
        .iter()
        .find(|t| t.name == "propose_status_update")
        .unwrap();
    assert_eq!(status.input_schema["additionalProperties"], json!(false));
    assert_eq!(status.input_schema["type"], json!("object"));
}

#[tokio::test]
async fn unknown_disallowed_and_malformed_calls_fail_closed() {
    let (reg, ctx, _) = setup(AgentRole::Worker);
    let err = reg
        .invoke(&ctx, "run_shell", json!({}), 0)
        .await
        .unwrap_err();
    assert!(matches!(err, CoreError::Denied(_)), "{err:?}");
    let err = reg
        .invoke(&ctx, "propose_status_update", json!({}), 0)
        .await
        .unwrap_err();
    assert!(matches!(err, CoreError::Denied(_)), "{err:?}");
    let err = reg
        .invoke(
            &ctx,
            "search_records",
            json!({"query": "a", "url": "http://x"}),
            0,
        )
        .await
        .unwrap_err();
    assert!(matches!(err, CoreError::Invalid(_)), "{err:?}");
    let err = reg
        .invoke(&ctx, "search_records", json!({"query": "a"}), 64)
        .await
        .unwrap_err();
    assert!(matches!(err, CoreError::Denied(_)), "{err:?}");
    let big = "a".repeat(MAX_INPUT_BYTES + 1);
    let err = reg
        .invoke(
            &ctx,
            "compare_outputs",
            json!({"expected": big, "actual": 1}),
            0,
        )
        .await;
    assert!(err.is_err());
}

#[tokio::test]
async fn reads_stay_in_scope_and_are_bounded() {
    let (reg, ctx, _) = setup(AgentRole::Worker);
    let out = reg
        .invoke(&ctx, "search_records", json!({"query": "alpha"}), 0)
        .await
        .unwrap();
    let ids: Vec<i64> = out.output["results"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["id"].as_i64().unwrap())
        .collect();
    assert_eq!(ids, vec![1, 2]);
    let out = reg
        .invoke(
            &ctx,
            "read_records",
            json!({"ids": [3], "max_body_chars": 100}),
            0,
        )
        .await
        .unwrap();
    assert_eq!(out.output["records"][0]["body_truncated"], json!(true));
    assert!(out.output["note"].as_str().unwrap().contains("untrusted"));
    let err = reg
        .invoke(&ctx, "read_records", json!({"ids": [4]}), 0)
        .await
        .unwrap_err();
    assert!(matches!(err, CoreError::Denied(_)));
    let out = reg
        .invoke(
            &ctx,
            "field_statistics",
            json!({"ids": [1, 2, 3], "field": "level"}),
            0,
        )
        .await
        .unwrap();
    assert_eq!(out.output["top"][0], json!(["warn", 2]));
}

#[tokio::test]
async fn proposals_are_recorded_as_pending_and_scope_checked() {
    let (reg, ctx, fake) = setup(AgentRole::Worker);
    let input = json!({"title": "t", "severity": "low", "summary": "s", "record_ids": [1, 4]});
    assert!(reg.invoke(&ctx, "record_finding", input, 0).await.is_err());
    assert!(fake.findings.lock().unwrap().is_empty());
    let input = json!({"title": "t", "severity": "low", "summary": "s", "record_ids": [1]});
    let out = reg.invoke(&ctx, "record_finding", input, 0).await.unwrap();
    assert_eq!(out.effect, EffectClass::Proposal);
    let saved = fake.findings.lock().unwrap();
    assert_eq!(saved[0].provenance.origin, Origin::ModelGenerated);
    assert_eq!(saved[0].task_id, Some(TaskId(5)));
}

#[tokio::test]
async fn status_updates_only_create_approval_requests() {
    let (reg, ctx, fake) = setup(AgentRole::Assistant);
    let input = json!({"finding_id": 1, "status": "resolved", "justification": "fixed upstream"});
    let out = reg
        .invoke(&ctx, "propose_status_update", input, 0)
        .await
        .unwrap();
    assert_eq!(out.effect, EffectClass::ApprovalRequired);
    assert_eq!(out.output["status"], json!("pending_operator_approval"));
    let approvals = fake.approvals.lock().unwrap();
    assert_eq!(approvals[0].operation, PROPOSE_STATUS_UPDATE);
    assert_eq!(
        approvals[0].request,
        json!({"finding_id": 1, "status": "resolved"})
    );
}

#[tokio::test]
async fn operations_without_a_task_context_are_denied() {
    let (reg, mut ctx, _) = setup(AgentRole::Assistant);
    ctx.policy.scope.task_id = None;
    let out = reg
        .invoke(
            &ctx,
            "propose_report_draft",
            json!({"title": "r", "body_markdown": "b"}),
            0,
        )
        .await;
    assert!(out.is_err());
}

#[test]
fn json_diff_reports_paths() {
    let d = json_diff(
        &json!({"a": 1, "b": [1, 2]}),
        &json!({"a": 2, "b": [1], "c": true}),
        10,
    );
    let paths: Vec<_> = d.iter().map(|(p, _, _)| p.as_str()).collect();
    assert_eq!(paths, vec!["/a", "/b/1", "/c"]);
}
