use arustex_core::domain::agent::SessionId;
use arustex_core::domain::report::{Report, ReportStatus};
use arustex_core::domain::{Origin, ReportId, TaskId};
use arustex_core::events::DomainEvent;
use arustex_core::{CoreError, CoreResult, Page, Paged};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sqlx::Row;
use sqlx::postgres::PgRow;

use crate::postgres::{Store, audit, db_err, insert_event, to_u32, to_u64};

pub const MAX_REPORT_BYTES: usize = 4 * 1024 * 1024;
pub const MAX_NOTIFICATION_CHARS: usize = 1000;

fn report_from_row(row: &PgRow) -> CoreResult<Report> {
    let status: String = row.try_get("status").map_err(db_err)?;
    let origin: String = row.try_get("origin").map_err(db_err)?;
    Ok(Report {
        id: ReportId(row.try_get("id").map_err(db_err)?),
        task_id: TaskId(row.try_get("task_id").map_err(db_err)?),
        title: row.try_get("title").map_err(db_err)?,
        status: status.parse()?,
        origin: origin.parse()?,
        body_markdown: row.try_get("body_markdown").map_err(db_err)?,
        document: row.try_get("document").map_err(db_err)?,
        created_at: row.try_get("created_at").map_err(db_err)?,
        finalized_at: row.try_get("finalized_at").map_err(db_err)?,
    })
}

/// Summary row for report listings (no body).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ReportSummary {
    pub id: ReportId,
    pub task_id: TaskId,
    pub title: String,
    pub status: ReportStatus,
    pub origin: Origin,
    pub created_at: DateTime<Utc>,
}

/// One queued outbound notification.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Notification {
    pub id: i64,
    pub kind: String,
    pub task_id: Option<TaskId>,
    pub text: String,
    pub attempts: u32,
}

impl Store {
    pub(crate) async fn insert_report(
        &self,
        task_id: TaskId,
        title: &str,
        body_markdown: &str,
        document: serde_json::Value,
        origin: Origin,
        session_id: Option<SessionId>,
    ) -> CoreResult<Report> {
        let title = title.trim();
        if title.is_empty() || title.len() > 512 {
            return Err(CoreError::invalid("report title must be 1..=512 bytes"));
        }
        if body_markdown.len() > MAX_REPORT_BYTES {
            return Err(CoreError::LimitExceeded("report body too large".into()));
        }
        let mut tx = self.begin().await?;
        let row = sqlx::query(
            "INSERT INTO reports (task_id, title, origin, session_id, body_markdown, document)
             VALUES ($1, $2, $3, $4, $5, $6) RETURNING *",
        )
        .bind(task_id.0)
        .bind(title)
        .bind(origin.as_str())
        .bind(session_id.map(|s| s.0))
        .bind(body_markdown)
        .bind(document)
        .fetch_one(&mut *tx)
        .await
        .map_err(db_err)?;
        let report = report_from_row(&row)?;
        let event = insert_event(
            &mut tx,
            Some(task_id),
            session_id,
            DomainEvent::ReportGenerated {
                report_id: report.id,
                task_id,
            },
        )
        .await?;
        self.commit(tx, vec![event]).await?;
        Ok(report)
    }

    /// Stores a report generated deterministically from stored data.
    pub async fn save_generated_report(
        &self,
        task_id: TaskId,
        title: &str,
        body_markdown: &str,
        document: serde_json::Value,
    ) -> CoreResult<Report> {
        self.insert_report(
            task_id,
            title,
            body_markdown,
            document,
            Origin::Derived,
            None,
        )
        .await
    }

    pub async fn get_report(&self, id: ReportId) -> CoreResult<Report> {
        let row = sqlx::query("SELECT * FROM reports WHERE id = $1")
            .bind(id.0)
            .fetch_optional(self.pool())
            .await
            .map_err(db_err)?
            .ok_or_else(|| CoreError::not_found(format!("report {id}")))?;
        report_from_row(&row)
    }

    pub async fn list_reports(
        &self,
        task: Option<TaskId>,
        page: Page,
    ) -> CoreResult<Paged<ReportSummary>> {
        let page = page.clamped();
        let total: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM reports WHERE ($1::bigint IS NULL OR task_id = $1)",
        )
        .bind(task.map(|t| t.0))
        .fetch_one(self.pool())
        .await
        .map_err(db_err)?;
        let rows = sqlx::query(
            "SELECT id, task_id, title, status, origin, created_at FROM reports
             WHERE ($1::bigint IS NULL OR task_id = $1) ORDER BY id DESC LIMIT $2 OFFSET $3",
        )
        .bind(task.map(|t| t.0))
        .bind(i64::from(page.limit))
        .bind(i64::from(page.offset))
        .fetch_all(self.pool())
        .await
        .map_err(db_err)?;
        let items = rows
            .iter()
            .map(|row| {
                let status: String = row.try_get("status").map_err(db_err)?;
                let origin: String = row.try_get("origin").map_err(db_err)?;
                Ok(ReportSummary {
                    id: ReportId(row.try_get("id").map_err(db_err)?),
                    task_id: TaskId(row.try_get("task_id").map_err(db_err)?),
                    title: row.try_get("title").map_err(db_err)?,
                    status: status.parse()?,
                    origin: origin.parse()?,
                    created_at: row.try_get("created_at").map_err(db_err)?,
                })
            })
            .collect::<CoreResult<_>>()?;
        Ok(Paged::new(items, to_u64(total), page))
    }

    /// Operator approval of a draft report.
    pub async fn finalize_report(&self, id: ReportId, actor: &str) -> CoreResult<Report> {
        let mut tx = self.begin().await?;
        let row = sqlx::query(
            "UPDATE reports SET status = 'final', finalized_at = now() WHERE id = $1 AND status = 'draft' RETURNING *",
        )
        .bind(id.0)
        .fetch_optional(&mut *tx)
        .await
        .map_err(db_err)?
        .ok_or_else(|| CoreError::Conflict(format!("report {id} is missing or already final")))?;
        audit(
            &mut tx,
            actor,
            "report.finalize",
            &format!("report:{id}"),
            serde_json::json!({}),
        )
        .await?;
        self.commit(tx, Vec::new()).await?;
        report_from_row(&row)
    }

    /// Queues a short notification. Callers pass only task status and non-sensitive
    /// summaries; the text is truncated to the outbox limit.
    pub async fn enqueue_notification(
        &self,
        kind: &str,
        task: Option<TaskId>,
        text: &str,
    ) -> CoreResult<i64> {
        let text: String = text.chars().take(MAX_NOTIFICATION_CHARS).collect();
        if text.trim().is_empty() {
            return Err(CoreError::invalid("notification text must not be empty"));
        }
        sqlx::query_scalar(
            "INSERT INTO notifications (kind, task_id, text) VALUES ($1, $2, $3) RETURNING id",
        )
        .bind(kind)
        .bind(task.map(|t| t.0))
        .bind(text)
        .fetch_one(self.pool())
        .await
        .map_err(db_err)
    }

    /// Claims due notifications for delivery. Items stuck in `sending` for more than
    /// five minutes (a crashed sender) become due again.
    pub async fn claim_notifications(&self, limit: u32) -> CoreResult<Vec<Notification>> {
        let rows = sqlx::query(
            "WITH due AS (
                 SELECT id FROM notifications
                 WHERE (state = 'pending' AND next_attempt_at <= now())
                    OR (state = 'sending' AND next_attempt_at <= now() - interval '5 minutes')
                 ORDER BY id FOR UPDATE SKIP LOCKED LIMIT $1)
             UPDATE notifications n SET state = 'sending', attempts = n.attempts + 1, next_attempt_at = now()
             FROM due WHERE n.id = due.id
             RETURNING n.id, n.kind, n.task_id, n.text, n.attempts",
        )
        .bind(i64::from(limit.clamp(1, 100)))
        .fetch_all(self.pool())
        .await
        .map_err(db_err)?;
        rows.iter()
            .map(|row| {
                Ok(Notification {
                    id: row.try_get("id").map_err(db_err)?,
                    kind: row.try_get("kind").map_err(db_err)?,
                    task_id: row
                        .try_get::<Option<i64>, _>("task_id")
                        .map_err(db_err)?
                        .map(TaskId),
                    text: row.try_get("text").map_err(db_err)?,
                    attempts: to_u32(row.try_get("attempts").map_err(db_err)?),
                })
            })
            .collect()
    }

    pub async fn notification_sent(&self, id: i64) -> CoreResult<()> {
        sqlx::query("UPDATE notifications SET state = 'sent', sent_at = now(), last_error = '' WHERE id = $1")
            .bind(id)
            .execute(self.pool())
            .await
            .map_err(db_err)?;
        Ok(())
    }

    /// Records a failed delivery. Retries back off exponentially until `max_attempts`.
    pub async fn notification_failed(
        &self,
        id: i64,
        error: &str,
        max_attempts: u32,
    ) -> CoreResult<()> {
        let error: String = error.chars().take(500).collect();
        sqlx::query(
            "UPDATE notifications
             SET state = CASE WHEN attempts >= $3 THEN 'failed' ELSE 'pending' END,
                 next_attempt_at = now() + make_interval(secs => least(3600, 15 * power(2, attempts)::int)),
                 last_error = $2
             WHERE id = $1",
        )
        .bind(id)
        .bind(error)
        .bind(i32::try_from(max_attempts).unwrap_or(i32::MAX))
        .execute(self.pool())
        .await
        .map_err(db_err)?;
        Ok(())
    }

    /// Marks every pending notification as skipped (delivery disabled).
    pub async fn skip_pending_notifications(&self) -> CoreResult<u64> {
        let r = sqlx::query(
            "UPDATE notifications SET state = 'skipped' WHERE state IN ('pending', 'sending')",
        )
        .execute(self.pool())
        .await
        .map_err(db_err)?;
        Ok(r.rows_affected())
    }
}
