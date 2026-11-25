use arustex_core::domain::agent::SessionId;
use arustex_core::domain::finding::{
    ConfirmationStatus, Finding, FindingStatus, MAX_NOTE_BODY, NewFinding, NoteSubject,
    ReviewNote, ReviewResult, Verdict,
};
use arustex_core::domain::report::Report;
use arustex_core::domain::work::{Approval, ApprovalStatus, NewApproval};
use arustex_core::domain::{
    ApprovalId, AssetId, FindingId, NodeId, NoteId, Origin, Provenance, RecordId, ReviewResultId,
    TaskId, WorkItemId,
};
use arustex_core::events::DomainEvent;
use arustex_core::ports::ReviewSink;
use arustex_core::{CoreError, CoreResult, Page, Paged};
use async_trait::async_trait;
use sqlx::Row;
use sqlx::postgres::PgRow;

use crate::postgres::{Store, audit, db_err, insert_event, to_u64};

/// Upper bound on a stored approval request payload.
pub const MAX_APPROVAL_REQUEST_BYTES: usize = 64 * 1024;

fn finding_from_row(row: &PgRow) -> CoreResult<Finding> {
    let severity: String = row.try_get("severity").map_err(db_err)?;
    let status: String = row.try_get("status").map_err(db_err)?;
    let origin: String = row.try_get("origin").map_err(db_err)?;
    let record_ids: Vec<i64> = row.try_get("record_ids").map_err(db_err)?;
    let asset_ids: Vec<i64> = row.try_get("asset_ids").map_err(db_err)?;
    Ok(Finding {
        id: FindingId(row.try_get("id").map_err(db_err)?),
        task_id: row.try_get::<Option<i64>, _>("task_id").map_err(db_err)?.map(TaskId),
        node_id: row.try_get::<Option<i64>, _>("node_id").map_err(db_err)?.map(NodeId),
        title: row.try_get("title").map_err(db_err)?,
        category: row.try_get("category").map_err(db_err)?,
        severity: severity.parse()?,
        status: status.parse()?,
        summary: row.try_get("summary").map_err(db_err)?,
        record_ids: record_ids.into_iter().map(RecordId).collect(),
        asset_ids: asset_ids.into_iter().map(AssetId).collect(),
        provenance: Provenance {
            origin: origin.parse()?,
            source: row.try_get("source").map_err(db_err)?,
            locator: row.try_get("locator").map_err(db_err)?,
            session_id: row.try_get::<Option<uuid::Uuid>, _>("session_id").map_err(db_err)?,
        },
        created_at: row.try_get("created_at").map_err(db_err)?,
        updated_at: row.try_get("updated_at").map_err(db_err)?,
    })
}

const FINDING_COLUMNS: &str = "f.id, f.task_id, f.node_id, f.title, f.category, f.severity, f.status,
    f.summary, f.origin, f.source, f.locator, f.session_id, f.created_at, f.updated_at,
    COALESCE((SELECT array_agg(record_id ORDER BY record_id) FROM finding_records WHERE finding_id = f.id), '{}') AS record_ids,
    COALESCE((SELECT array_agg(asset_id ORDER BY asset_id) FROM finding_assets WHERE finding_id = f.id), '{}') AS asset_ids";

fn approval_from_row(row: &PgRow) -> CoreResult<Approval> {
    let status: String = row.try_get("status").map_err(db_err)?;
    Ok(Approval {
        id: ApprovalId(row.try_get("id").map_err(db_err)?),
        task_id: row.try_get::<Option<i64>, _>("task_id").map_err(db_err)?.map(TaskId),
        session_id: row
            .try_get::<Option<uuid::Uuid>, _>("session_id")
            .map_err(db_err)?
            .map(SessionId),
        operation: row.try_get("operation").map_err(db_err)?,
        summary: row.try_get("summary").map_err(db_err)?,
        request: row.try_get("request").map_err(db_err)?,
        status: status.parse()?,
        decided_by: row.try_get("decided_by").map_err(db_err)?,
        reason: row.try_get("reason").map_err(db_err)?,
        created_at: row.try_get("created_at").map_err(db_err)?,
        decided_at: row.try_get("decided_at").map_err(db_err)?,
    })
}

fn result_from_row(row: &PgRow) -> CoreResult<ReviewResult> {
    let verdict: String = row.try_get("verdict").map_err(db_err)?;
    let status: String = row.try_get("status").map_err(db_err)?;
    Ok(ReviewResult {
        id: ReviewResultId(row.try_get("id").map_err(db_err)?),
        task_id: TaskId(row.try_get("task_id").map_err(db_err)?),
        work_item_id: row
            .try_get::<Option<i64>, _>("work_item_id")
            .map_err(db_err)?
            .map(WorkItemId),
        verdict: verdict.parse()?,
        details: row.try_get("details").map_err(db_err)?,
        status: status.parse()?,
        decided_by: row.try_get("decided_by").map_err(db_err)?,
        created_at: row.try_get("created_at").map_err(db_err)?,
        decided_at: row.try_get("decided_at").map_err(db_err)?,
    })
}

fn note_from_row(row: &PgRow) -> CoreResult<ReviewNote> {
    let subject: String = row.try_get("subject").map_err(db_err)?;
    Ok(ReviewNote {
        id: NoteId(row.try_get("id").map_err(db_err)?),
        subject: subject.parse()?,
        subject_id: row.try_get("subject_id").map_err(db_err)?,
        author: row.try_get("author").map_err(db_err)?,
        body: row.try_get("body").map_err(db_err)?,
        created_at: row.try_get("created_at").map_err(db_err)?,
    })
}

impl Store {
    /// Inserts a finding with its record and asset references. Model-generated
    /// findings always start as `pending` regardless of the requested status.
    pub async fn create_finding(&self, input: &NewFinding, legacy_id: Option<i64>) -> CoreResult<Finding> {
        input.validate()?;
        let status = input.initial_status();
        let mut tx = self.begin().await?;
        let id: i64 = sqlx::query_scalar(
            "INSERT INTO findings (id, task_id, node_id, title, category, severity, status, summary,
                                   origin, source, locator, session_id, legacy_id)
             VALUES (COALESCE($1, nextval(pg_get_serial_sequence('findings', 'id'))),
                     $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13)
             RETURNING id",
        )
        .bind(input.preserve_id.map(|f| f.0))
        .bind(input.task_id.map(|t| t.0))
        .bind(input.node_id.map(|n| n.0))
        .bind(input.title.trim())
        .bind(&input.category)
        .bind(input.severity.as_str())
        .bind(status.as_str())
        .bind(&input.summary)
        .bind(input.provenance.origin.as_str())
        .bind(&input.provenance.source)
        .bind(&input.provenance.locator)
        .bind(input.provenance.session_id)
        .bind(legacy_id)
        .fetch_one(&mut *tx)
        .await
        .map_err(db_err)?;
        let records: Vec<i64> = input.record_ids.iter().map(|r| r.0).collect();
        let assets: Vec<i64> = input.asset_ids.iter().map(|a| a.0).collect();
        sqlx::query(
            "INSERT INTO finding_records (finding_id, record_id)
             SELECT $1, unnest($2::bigint[]) ON CONFLICT DO NOTHING",
        )
        .bind(id)
        .bind(&records)
        .execute(&mut *tx)
        .await
        .map_err(db_err)?;
        sqlx::query(
            "INSERT INTO finding_assets (finding_id, asset_id)
             SELECT $1, unnest($2::bigint[]) ON CONFLICT DO NOTHING",
        )
        .bind(id)
        .bind(&assets)
        .execute(&mut *tx)
        .await
        .map_err(db_err)?;
        let event = insert_event(
            &mut tx,
            input.task_id,
            input.provenance.session_id.map(SessionId),
            DomainEvent::FindingRecorded { finding_id: FindingId(id), severity: input.severity },
        )
        .await?;
        self.commit(tx, vec![event]).await?;
        self.get_finding(FindingId(id)).await
    }

    pub async fn get_finding(&self, id: FindingId) -> CoreResult<Finding> {
        let sql = format!("SELECT {FINDING_COLUMNS} FROM findings f WHERE f.id = $1");
        let row = sqlx::query(sqlx::AssertSqlSafe(sql))
            .bind(id.0)
            .fetch_optional(self.pool())
            .await
            .map_err(db_err)?
            .ok_or_else(|| CoreError::not_found(format!("finding {id}")))?;
        finding_from_row(&row)
    }

    pub async fn list_findings(&self, task: Option<TaskId>, page: Page) -> CoreResult<Paged<Finding>> {
        let page = page.clamped();
        let total: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM findings WHERE ($1::bigint IS NULL OR task_id = $1)",
        )
        .bind(task.map(|t| t.0))
        .fetch_one(self.pool())
        .await
        .map_err(db_err)?;
        let sql = format!(
            "SELECT {FINDING_COLUMNS} FROM findings f WHERE ($1::bigint IS NULL OR f.task_id = $1)
             ORDER BY f.id DESC LIMIT $2 OFFSET $3"
        );
        let rows = sqlx::query(sqlx::AssertSqlSafe(sql))
            .bind(task.map(|t| t.0))
            .bind(i64::from(page.limit))
            .bind(i64::from(page.offset))
            .fetch_all(self.pool())
            .await
            .map_err(db_err)?;
        let items = rows.iter().map(finding_from_row).collect::<CoreResult<_>>()?;
        Ok(Paged::new(items, to_u64(total), page))
    }

    /// Findings of a task, ordered by severity (highest first), for reports.
    pub async fn task_findings(&self, task: TaskId) -> CoreResult<Vec<Finding>> {
        let sql = format!(
            "SELECT {FINDING_COLUMNS} FROM findings f WHERE f.task_id = $1
             ORDER BY array_position(ARRAY['critical','high','medium','low','info'], f.severity), f.id"
        );
        let rows = sqlx::query(sqlx::AssertSqlSafe(sql))
            .bind(task.0)
            .fetch_all(self.pool())
            .await
            .map_err(db_err)?;
        rows.iter().map(finding_from_row).collect()
    }

    pub async fn finding_count(&self, status: Option<FindingStatus>) -> CoreResult<u64> {
        let n: i64 = sqlx::query_scalar("SELECT count(*) FROM findings WHERE ($1::text IS NULL OR status = $1)")
            .bind(status.map(|s| s.as_str()))
            .fetch_one(self.pool())
            .await
            .map_err(db_err)?;
        Ok(to_u64(n))
    }

    /// Operator decision on a finding.
    pub async fn set_finding_status(&self, id: FindingId, status: FindingStatus, actor: &str) -> CoreResult<Finding> {
        let mut tx = self.begin().await?;
        let task: Option<Option<i64>> =
            sqlx::query_scalar("UPDATE findings SET status = $2 WHERE id = $1 RETURNING task_id")
                .bind(id.0)
                .bind(status.as_str())
                .fetch_optional(&mut *tx)
                .await
                .map_err(db_err)?;
        let task = task.ok_or_else(|| CoreError::not_found(format!("finding {id}")))?;
        audit(&mut tx, actor, "finding.status", &format!("finding:{id}"), serde_json::json!({"status": status})).await?;
        let event = insert_event(
            &mut tx,
            task.map(TaskId),
            None,
            DomainEvent::FindingStatusChanged { finding_id: id, status },
        )
        .await?;
        self.commit(tx, vec![event]).await?;
        self.get_finding(id).await
    }

    pub async fn add_note(&self, subject: NoteSubject, subject_id: i64, author: &str, body: &str) -> CoreResult<ReviewNote> {
        let body = body.trim();
        if body.is_empty() || body.len() > MAX_NOTE_BODY {
            return Err(CoreError::invalid(format!("note body must be 1..={MAX_NOTE_BODY} bytes")));
        }
        let table = match subject {
            NoteSubject::Task => "tasks",
            NoteSubject::Finding => "findings",
            NoteSubject::Record => "records",
            NoteSubject::ReviewResult => "review_results",
        };
        let exists: bool = sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
            "SELECT EXISTS (SELECT 1 FROM {table} WHERE id = $1)"
        )))
        .bind(subject_id)
        .fetch_one(self.pool())
        .await
        .map_err(db_err)?;
        if !exists {
            return Err(CoreError::not_found(format!("{subject} {subject_id}")));
        }
        let row = sqlx::query(
            "INSERT INTO review_notes (subject, subject_id, author, body) VALUES ($1, $2, $3, $4)
             RETURNING id, subject, subject_id, author, body, created_at",
        )
        .bind(subject.as_str())
        .bind(subject_id)
        .bind(author)
        .bind(body)
        .fetch_one(self.pool())
        .await
        .map_err(db_err)?;
        note_from_row(&row)
    }

    pub async fn list_notes(&self, subject: NoteSubject, subject_id: i64) -> CoreResult<Vec<ReviewNote>> {
        let rows = sqlx::query(
            "SELECT id, subject, subject_id, author, body, created_at FROM review_notes
             WHERE subject = $1 AND subject_id = $2 ORDER BY id LIMIT 500",
        )
        .bind(subject.as_str())
        .bind(subject_id)
        .fetch_all(self.pool())
        .await
        .map_err(db_err)?;
        rows.iter().map(note_from_row).collect()
    }

    pub async fn get_approval(&self, id: ApprovalId) -> CoreResult<Approval> {
        let row = sqlx::query("SELECT * FROM approvals WHERE id = $1")
            .bind(id.0)
            .fetch_optional(self.pool())
            .await
            .map_err(db_err)?
            .ok_or_else(|| CoreError::not_found(format!("approval {id}")))?;
        approval_from_row(&row)
    }

    pub async fn list_approvals(&self, status: Option<ApprovalStatus>, page: Page) -> CoreResult<Paged<Approval>> {
        let page = page.clamped();
        let status = status.map(|s| s.as_str());
        let total: i64 = sqlx::query_scalar("SELECT count(*) FROM approvals WHERE ($1::text IS NULL OR status = $1)")
            .bind(status)
            .fetch_one(self.pool())
            .await
            .map_err(db_err)?;
        let rows = sqlx::query(
            "SELECT * FROM approvals WHERE ($1::text IS NULL OR status = $1) ORDER BY id DESC LIMIT $2 OFFSET $3",
        )
        .bind(status)
        .bind(i64::from(page.limit))
        .bind(i64::from(page.offset))
        .fetch_all(self.pool())
        .await
        .map_err(db_err)?;
        let items = rows.iter().map(approval_from_row).collect::<CoreResult<_>>()?;
        Ok(Paged::new(items, to_u64(total), page))
    }

    /// Records the operator's decision on a pending approval. Deciding twice fails.
    pub async fn decide_approval(
        &self,
        id: ApprovalId,
        approve: bool,
        actor: &str,
        reason: Option<&str>,
    ) -> CoreResult<Approval> {
        let status = if approve { ApprovalStatus::Approved } else { ApprovalStatus::Rejected };
        let mut tx = self.begin().await?;
        let row = sqlx::query(
            "UPDATE approvals SET status = $2, decided_by = $3, reason = $4, decided_at = now()
             WHERE id = $1 AND status = 'pending' RETURNING *",
        )
        .bind(id.0)
        .bind(status.as_str())
        .bind(actor)
        .bind(reason)
        .fetch_optional(&mut *tx)
        .await
        .map_err(db_err)?;
        let Some(row) = row else {
            let current = self.get_approval(id).await?;
            return Err(CoreError::Conflict(format!("approval {id} is already {}", current.status)));
        };
        let approval = approval_from_row(&row)?;
        audit(&mut tx, actor, "approval.decide", &format!("approval:{id}"), serde_json::json!({"approve": approve})).await?;
        let event = insert_event(
            &mut tx,
            approval.task_id,
            approval.session_id,
            DomainEvent::ApprovalDecided { approval_id: id, approved: approve },
        )
        .await?;
        self.commit(tx, vec![event]).await?;
        Ok(approval)
    }

    /// Marks an approved request as applied or failed. Only approved requests move on.
    pub async fn finish_approval(&self, id: ApprovalId, applied: bool, error: Option<&str>) -> CoreResult<()> {
        let status = if applied { ApprovalStatus::Applied } else { ApprovalStatus::Failed };
        let result = sqlx::query(
            "UPDATE approvals SET status = $2, reason = COALESCE($3, reason) WHERE id = $1 AND status = 'approved'",
        )
        .bind(id.0)
        .bind(status.as_str())
        .bind(error)
        .execute(self.pool())
        .await
        .map_err(db_err)?;
        if result.rows_affected() == 0 {
            return Err(CoreError::Conflict(format!("approval {id} is not approved")));
        }
        Ok(())
    }

    /// Stores a reviewer verdict for a work item. One result per work item; a retry
    /// of the same item replaces the earlier pending result.
    pub async fn record_review_result(
        &self,
        task: TaskId,
        work_item: Option<WorkItemId>,
        verdict: Verdict,
        details: serde_json::Value,
    ) -> CoreResult<ReviewResult> {
        if details.to_string().len() > 64 * 1024 {
            return Err(CoreError::LimitExceeded("review details too large".into()));
        }
        let row = sqlx::query(
            "INSERT INTO review_results (task_id, work_item_id, verdict, details)
             VALUES ($1, $2, $3, $4)
             ON CONFLICT (work_item_id) WHERE work_item_id IS NOT NULL DO UPDATE
                 SET verdict = EXCLUDED.verdict, details = EXCLUDED.details
                 WHERE review_results.status = 'pending_confirmation'
             RETURNING *",
        )
        .bind(task.0)
        .bind(work_item.map(|w| w.0))
        .bind(verdict.as_str())
        .bind(details)
        .fetch_optional(self.pool())
        .await
        .map_err(db_err)?
        .ok_or_else(|| CoreError::Conflict("the review result was already decided".into()))?;
        result_from_row(&row)
    }

    pub async fn list_review_results(&self, task: Option<TaskId>, page: Page) -> CoreResult<Paged<ReviewResult>> {
        let page = page.clamped();
        let total: i64 = sqlx::query_scalar("SELECT count(*) FROM review_results WHERE ($1::bigint IS NULL OR task_id = $1)")
            .bind(task.map(|t| t.0))
            .fetch_one(self.pool())
            .await
            .map_err(db_err)?;
        let rows = sqlx::query(
            "SELECT * FROM review_results WHERE ($1::bigint IS NULL OR task_id = $1)
             ORDER BY id DESC LIMIT $2 OFFSET $3",
        )
        .bind(task.map(|t| t.0))
        .bind(i64::from(page.limit))
        .bind(i64::from(page.offset))
        .fetch_all(self.pool())
        .await
        .map_err(db_err)?;
        let items = rows.iter().map(result_from_row).collect::<CoreResult<_>>()?;
        Ok(Paged::new(items, to_u64(total), page))
    }

    pub async fn pending_review_results(&self, task: TaskId) -> CoreResult<u64> {
        let n: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM review_results WHERE task_id = $1 AND status = 'pending_confirmation'",
        )
        .bind(task.0)
        .fetch_one(self.pool())
        .await
        .map_err(db_err)?;
        Ok(to_u64(n))
    }

    pub async fn decide_review_result(&self, id: ReviewResultId, confirm: bool, actor: &str) -> CoreResult<ReviewResult> {
        let status = if confirm { ConfirmationStatus::Confirmed } else { ConfirmationStatus::Rejected };
        let mut tx = self.begin().await?;
        let row = sqlx::query(
            "UPDATE review_results SET status = $2, decided_by = $3, decided_at = now()
             WHERE id = $1 AND status = 'pending_confirmation' RETURNING *",
        )
        .bind(id.0)
        .bind(status.as_str())
        .bind(actor)
        .fetch_optional(&mut *tx)
        .await
        .map_err(db_err)?
        .ok_or_else(|| CoreError::Conflict(format!("review result {id} is missing or already decided")))?;
        audit(&mut tx, actor, "review_result.decide", &format!("review_result:{id}"), serde_json::json!({"confirm": confirm})).await?;
        self.commit(tx, Vec::new()).await?;
        result_from_row(&row)
    }
}

#[async_trait]
impl ReviewSink for Store {
    async fn record_finding(&self, finding: NewFinding) -> CoreResult<Finding> {
        self.create_finding(&finding, None).await
    }

    async fn request_approval(&self, a: NewApproval) -> CoreResult<Approval> {
        if a.summary.len() > 4096 {
            return Err(CoreError::LimitExceeded("approval summary too long".into()));
        }
        if a.request.to_string().len() > MAX_APPROVAL_REQUEST_BYTES {
            return Err(CoreError::LimitExceeded("approval request too large".into()));
        }
        let mut tx = self.begin().await?;
        let row = sqlx::query(
            "INSERT INTO approvals (task_id, session_id, operation, summary, request)
             VALUES ($1, $2, $3, $4, $5) RETURNING *",
        )
        .bind(a.task_id.map(|t| t.0))
        .bind(a.session_id.map(|s| s.0))
        .bind(&a.operation)
        .bind(&a.summary)
        .bind(&a.request)
        .fetch_one(&mut *tx)
        .await
        .map_err(db_err)?;
        let approval = approval_from_row(&row)?;
        let event = insert_event(
            &mut tx,
            a.task_id,
            a.session_id,
            DomainEvent::ApprovalRequested { approval_id: approval.id, operation: a.operation.clone() },
        )
        .await?;
        self.commit(tx, vec![event]).await?;
        Ok(approval)
    }

    async fn save_report_draft(
        &self,
        task_id: TaskId,
        title: &str,
        body_markdown: &str,
        session_id: SessionId,
    ) -> CoreResult<Report> {
        let document = serde_json::json!({
            "schema": "arustex.report.v1",
            "origin": Origin::ModelGenerated,
            "session_id": session_id,
            "draft": true,
        });
        self.insert_report(task_id, title, body_markdown, document, Origin::ModelGenerated, Some(session_id))
            .await
    }
}
