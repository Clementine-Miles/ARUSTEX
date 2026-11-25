use std::time::Duration;

use arustex_core::domain::agent::SessionId;
use arustex_core::domain::work::{NewWorkItem, WorkItem, WorkStatus};
use arustex_core::domain::{NodeId, RecordId, TaskId, WorkItemId};
use arustex_core::events::DomainEvent;
use arustex_core::ports::{QueueCounts, WorkQueue};
use arustex_core::{CoreError, CoreResult};
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sqlx::Row;
use sqlx::postgres::PgRow;

use crate::postgres::{Store, audit, db_err, insert_event, to_i32, to_u32, to_u64};

/// Shortest and longest lease a worker may hold.
pub const MIN_LEASE: Duration = Duration::from_secs(5);
pub const MAX_LEASE: Duration = Duration::from_secs(3600);

fn lease_millis(lease: Duration) -> i64 {
    let clamped = lease.clamp(MIN_LEASE, MAX_LEASE);
    i64::try_from(clamped.as_millis()).unwrap_or(i64::MAX)
}

pub(crate) fn item_from_row(row: &PgRow) -> CoreResult<WorkItem> {
    let kind: String = row.try_get("kind").map_err(db_err)?;
    let status: String = row.try_get("status").map_err(db_err)?;
    let record_ids: Vec<i64> = row.try_get("record_ids").map_err(db_err)?;
    Ok(WorkItem {
        id: WorkItemId(row.try_get("id").map_err(db_err)?),
        task_id: TaskId(row.try_get("task_id").map_err(db_err)?),
        kind: kind.parse()?,
        record_ids: record_ids.into_iter().map(RecordId).collect(),
        status: status.parse()?,
        priority: row.try_get("priority").map_err(db_err)?,
        attempts: to_u32(row.try_get("attempts").map_err(db_err)?),
        max_attempts: to_u32(row.try_get("max_attempts").map_err(db_err)?),
        lease_owner: row.try_get("lease_owner").map_err(db_err)?,
        lease_expires_at: row.try_get("lease_expires_at").map_err(db_err)?,
        operation_id: row.try_get("operation_id").map_err(db_err)?,
        node_id: row
            .try_get::<Option<i64>, _>("node_id")
            .map_err(db_err)?
            .map(NodeId),
        session_id: row
            .try_get::<Option<uuid::Uuid>, _>("session_id")
            .map_err(db_err)?
            .map(SessionId),
        result: row.try_get("result").map_err(db_err)?,
        last_error: row.try_get("last_error").map_err(db_err)?,
        created_at: row.try_get("created_at").map_err(db_err)?,
        updated_at: row.try_get("updated_at").map_err(db_err)?,
    })
}

/// Kind of an operator-configured trigger.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum TriggerSpec {
    /// Re-queue local processing of the task every `seconds` (at least 60).
    Interval { seconds: u32 },
    /// Re-queue local processing when records are imported into one of the task's datasets.
    RecordsImported,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Trigger {
    pub id: i64,
    pub task_id: TaskId,
    pub spec: TriggerSpec,
    pub enabled: bool,
    pub last_fired_at: Option<DateTime<Utc>>,
    pub last_event_seq: i64,
}

fn trigger_from_row(row: &PgRow) -> CoreResult<Trigger> {
    let kind: String = row.try_get("kind").map_err(db_err)?;
    let spec = match kind.as_str() {
        "interval" => TriggerSpec::Interval {
            seconds: to_u32(
                row.try_get::<Option<i32>, _>("interval_seconds")
                    .map_err(db_err)?
                    .unwrap_or(0),
            ),
        },
        "event" => TriggerSpec::RecordsImported,
        other => {
            return Err(CoreError::Integrity(format!(
                "unknown trigger kind {other}"
            )));
        }
    };
    Ok(Trigger {
        id: row.try_get("id").map_err(db_err)?,
        task_id: TaskId(row.try_get("task_id").map_err(db_err)?),
        spec,
        enabled: row.try_get("enabled").map_err(db_err)?,
        last_fired_at: row.try_get("last_fired_at").map_err(db_err)?,
        last_event_seq: row.try_get("last_event_seq").map_err(db_err)?,
    })
}

impl Store {
    pub async fn list_work_items(&self, task: TaskId, limit: u32) -> CoreResult<Vec<WorkItem>> {
        let rows = sqlx::query("SELECT * FROM work_items WHERE task_id = $1 ORDER BY id LIMIT $2")
            .bind(task.0)
            .bind(i64::from(limit.clamp(1, 10_000)))
            .fetch_all(self.pool())
            .await
            .map_err(db_err)?;
        rows.iter().map(item_from_row).collect()
    }

    /// Number of currently leased items of a task (the task's live worker count).
    pub async fn leased_count(&self, task: TaskId) -> CoreResult<u64> {
        let n: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM work_items WHERE task_id = $1 AND status = 'leased'",
        )
        .bind(task.0)
        .fetch_one(self.pool())
        .await
        .map_err(db_err)?;
        Ok(to_u64(n))
    }

    pub async fn create_trigger(
        &self,
        task: TaskId,
        spec: TriggerSpec,
        actor: &str,
    ) -> CoreResult<Trigger> {
        let (kind, interval, event) = match &spec {
            TriggerSpec::Interval { seconds } => {
                if *seconds < 60 {
                    return Err(CoreError::invalid(
                        "interval triggers need at least 60 seconds",
                    ));
                }
                ("interval", Some(to_i32(*seconds)?), None)
            }
            TriggerSpec::RecordsImported => ("event", None, Some("records_imported")),
        };
        let mut tx = self.begin().await?;
        let row = sqlx::query(
            "INSERT INTO triggers (task_id, kind, interval_seconds, event_kind, last_event_seq)
             VALUES ($1, $2, $3, $4, COALESCE((SELECT max(seq) FROM events), 0)) RETURNING *",
        )
        .bind(task.0)
        .bind(kind)
        .bind(interval)
        .bind(event)
        .fetch_one(&mut *tx)
        .await
        .map_err(db_err)?;
        let trigger = trigger_from_row(&row)?;
        audit(
            &mut tx,
            actor,
            "trigger.create",
            &format!("task:{task}"),
            serde_json::to_value(&spec).unwrap_or_default(),
        )
        .await?;
        self.commit(tx, Vec::new()).await?;
        Ok(trigger)
    }

    pub async fn list_triggers(&self, task: Option<TaskId>) -> CoreResult<Vec<Trigger>> {
        let rows = sqlx::query(
            "SELECT * FROM triggers WHERE ($1::bigint IS NULL OR task_id = $1) ORDER BY id",
        )
        .bind(task.map(|t| t.0))
        .fetch_all(self.pool())
        .await
        .map_err(db_err)?;
        rows.iter().map(trigger_from_row).collect()
    }

    pub async fn set_trigger_enabled(&self, id: i64, enabled: bool) -> CoreResult<()> {
        let r = sqlx::query("UPDATE triggers SET enabled = $2 WHERE id = $1")
            .bind(id)
            .bind(enabled)
            .execute(self.pool())
            .await
            .map_err(db_err)?;
        if r.rows_affected() == 0 {
            return Err(CoreError::not_found(format!("trigger {id}")));
        }
        Ok(())
    }

    /// Atomically claims the triggers that should fire now and marks them fired.
    /// Interval triggers fire when their period elapsed; event triggers fire when a
    /// `records_imported` event for one of the task's datasets arrived after the last
    /// event they consumed. Returns the ids of the tasks to re-queue.
    pub async fn fire_due_triggers(&self) -> CoreResult<Vec<TaskId>> {
        let mut tx = self.begin().await?;
        let interval: Vec<i64> = sqlx::query_scalar(
            "UPDATE triggers SET last_fired_at = now()
             WHERE enabled AND kind = 'interval'
               AND (last_fired_at IS NULL OR last_fired_at + make_interval(secs => interval_seconds) <= now())
             RETURNING task_id",
        )
        .fetch_all(&mut *tx)
        .await
        .map_err(db_err)?;
        let event: Vec<i64> = sqlx::query_scalar(
            "WITH latest AS (SELECT COALESCE(max(seq), 0) AS seq FROM events),
             due AS (
                 SELECT t.id FROM triggers t
                 WHERE t.enabled AND t.kind = 'event' AND EXISTS (
                     SELECT 1 FROM events e
                     JOIN task_datasets d ON d.task_id = t.task_id
                         AND d.dataset_id = (e.payload->>'dataset_id')::bigint
                     WHERE e.kind = 'records_imported' AND e.seq > t.last_event_seq)
             )
             UPDATE triggers SET last_fired_at = now(), last_event_seq = (SELECT seq FROM latest)
             WHERE id IN (SELECT id FROM due) RETURNING task_id",
        )
        .fetch_all(&mut *tx)
        .await
        .map_err(db_err)?;
        // Event triggers that saw nothing relevant still advance their cursor.
        sqlx::query(
            "UPDATE triggers SET last_event_seq = (SELECT COALESCE(max(seq), 0) FROM events)
             WHERE enabled AND kind = 'event'",
        )
        .execute(&mut *tx)
        .await
        .map_err(db_err)?;
        self.commit(tx, Vec::new()).await?;
        let mut tasks: Vec<TaskId> = interval.into_iter().chain(event).map(TaskId).collect();
        tasks.sort();
        tasks.dedup();
        Ok(tasks)
    }
}

#[async_trait]
impl WorkQueue for Store {
    async fn enqueue(&self, items: Vec<NewWorkItem>) -> CoreResult<Vec<WorkItem>> {
        if items.is_empty() {
            return Ok(Vec::new());
        }
        let mut tx = self.begin().await?;
        let mut out = Vec::with_capacity(items.len());
        let mut per_task: std::collections::BTreeMap<TaskId, u32> = Default::default();
        for item in &items {
            if item.record_ids.len() > 1000 {
                return Err(CoreError::LimitExceeded(
                    "a work item covers at most 1000 records".into(),
                ));
            }
            if item.max_attempts == 0 {
                return Err(CoreError::invalid("max_attempts must be at least 1"));
            }
            let records: Vec<i64> = item.record_ids.iter().map(|r| r.0).collect();
            let row = sqlx::query(
                "INSERT INTO work_items (task_id, kind, record_ids, priority, max_attempts, operation_id, node_id)
                 VALUES ($1, $2, $3, $4, $5, $6, $7) RETURNING *",
            )
            .bind(item.task_id.0)
            .bind(item.kind.as_str())
            .bind(&records)
            .bind(item.priority)
            .bind(to_i32(item.max_attempts)?)
            .bind(uuid::Uuid::now_v7())
            .bind(item.node_id.map(|n| n.0))
            .fetch_one(&mut *tx)
            .await
            .map_err(db_err)?;
            out.push(item_from_row(&row)?);
            *per_task.entry(item.task_id).or_default() += 1;
        }
        let mut events = Vec::new();
        for (task, count) in per_task {
            events.push(
                insert_event(
                    &mut tx,
                    Some(task),
                    None,
                    DomainEvent::WorkItemsPlanned {
                        task_id: task,
                        count,
                    },
                )
                .await?,
            );
        }
        self.commit(tx, events).await?;
        Ok(out)
    }

    async fn claim(
        &self,
        worker: &str,
        task_id: Option<TaskId>,
        lease: Duration,
    ) -> CoreResult<Option<WorkItem>> {
        if worker.is_empty() || worker.len() > 128 {
            return Err(CoreError::invalid("worker id must be 1..=128 bytes"));
        }
        let mut tx = self.begin().await?;
        // Claims are serialized by a short advisory lock so the per-task worker limit
        // holds exactly under concurrency; the claim itself is a single statement.
        sqlx::query("SELECT pg_advisory_xact_lock(7405623344)")
            .execute(&mut *tx)
            .await
            .map_err(db_err)?;
        // SKIP LOCKED lets concurrent workers claim different items without blocking.
        // Only items of running tasks below their worker limit are eligible.
        let row = sqlx::query(
            "WITH candidate AS (
                 SELECT w.id FROM work_items w
                 JOIN tasks t ON t.id = w.task_id
                 WHERE w.status = 'queued' AND t.status = 'running'
                   AND ($2::bigint IS NULL OR w.task_id = $2)
                   AND (SELECT count(*) FROM work_items l
                        WHERE l.task_id = w.task_id AND l.status = 'leased') < t.max_workers
                 ORDER BY w.priority DESC, w.id
                 FOR UPDATE OF w SKIP LOCKED
                 LIMIT 1
             )
             UPDATE work_items SET status = 'leased', lease_owner = $1, attempts = attempts + 1,
                    lease_expires_at = now() + make_interval(secs => $3::double precision / 1000.0)
             FROM candidate WHERE work_items.id = candidate.id
             RETURNING work_items.*",
        )
        .bind(worker)
        .bind(task_id.map(|t| t.0))
        .bind(lease_millis(lease))
        .fetch_optional(&mut *tx)
        .await
        .map_err(db_err)?;
        let Some(row) = row else {
            self.commit(tx, Vec::new()).await?;
            return Ok(None);
        };
        let item = item_from_row(&row)?;
        let event = insert_event(
            &mut tx,
            Some(item.task_id),
            None,
            DomainEvent::WorkItemClaimed {
                work_item_id: item.id,
                worker: worker.to_owned(),
            },
        )
        .await?;
        self.commit(tx, vec![event]).await?;
        Ok(Some(item))
    }

    async fn heartbeat(&self, id: WorkItemId, worker: &str, lease: Duration) -> CoreResult<bool> {
        let r = sqlx::query(
            "UPDATE work_items SET lease_expires_at = now() + make_interval(secs => $3::double precision / 1000.0)
             WHERE id = $1 AND status = 'leased' AND lease_owner = $2",
        )
        .bind(id.0)
        .bind(worker)
        .bind(lease_millis(lease))
        .execute(self.pool())
        .await
        .map_err(db_err)?;
        Ok(r.rows_affected() == 1)
    }

    async fn attach_session(
        &self,
        id: WorkItemId,
        worker: &str,
        session: SessionId,
    ) -> CoreResult<()> {
        let r = sqlx::query(
            "UPDATE work_items SET session_id = $3 WHERE id = $1 AND status = 'leased' AND lease_owner = $2",
        )
        .bind(id.0)
        .bind(worker)
        .bind(session.0)
        .execute(self.pool())
        .await
        .map_err(db_err)?;
        if r.rows_affected() == 0 {
            return Err(CoreError::Conflict(format!(
                "lease on work item {id} was lost"
            )));
        }
        Ok(())
    }

    async fn complete(
        &self,
        id: WorkItemId,
        worker: &str,
        result: serde_json::Value,
    ) -> CoreResult<()> {
        if result.to_string().len() > 256 * 1024 {
            return Err(CoreError::LimitExceeded(
                "work item result too large".into(),
            ));
        }
        let mut tx = self.begin().await?;
        let task: Option<i64> = sqlx::query_scalar(
            "UPDATE work_items SET status = 'done', result = $3, lease_owner = NULL, lease_expires_at = NULL,
                    last_error = NULL
             WHERE id = $1 AND status = 'leased' AND lease_owner = $2 RETURNING task_id",
        )
        .bind(id.0)
        .bind(worker)
        .bind(result)
        .fetch_optional(&mut *tx)
        .await
        .map_err(db_err)?;
        let task =
            task.ok_or_else(|| CoreError::Conflict(format!("lease on work item {id} was lost")))?;
        let event = insert_event(
            &mut tx,
            Some(TaskId(task)),
            None,
            DomainEvent::WorkItemFinished {
                work_item_id: id,
                succeeded: true,
            },
        )
        .await?;
        self.commit(tx, vec![event]).await
    }

    async fn fail(
        &self,
        id: WorkItemId,
        worker: &str,
        error: &str,
        retry: bool,
    ) -> CoreResult<WorkStatus> {
        let error: String = error.chars().take(2000).collect();
        let mut tx = self.begin().await?;
        let row = sqlx::query(
            "UPDATE work_items
             SET status = CASE WHEN $4 AND attempts < max_attempts THEN 'queued' ELSE 'failed' END,
                 lease_owner = NULL, lease_expires_at = NULL, last_error = $3
             WHERE id = $1 AND status = 'leased' AND lease_owner = $2
             RETURNING task_id, status",
        )
        .bind(id.0)
        .bind(worker)
        .bind(&error)
        .bind(retry)
        .fetch_optional(&mut *tx)
        .await
        .map_err(db_err)?
        .ok_or_else(|| CoreError::Conflict(format!("lease on work item {id} was lost")))?;
        let task: i64 = row.try_get("task_id").map_err(db_err)?;
        let status: WorkStatus = row
            .try_get::<String, _>("status")
            .map_err(db_err)?
            .parse()?;
        let mut events = Vec::new();
        if status == WorkStatus::Failed {
            events.push(
                insert_event(
                    &mut tx,
                    Some(TaskId(task)),
                    None,
                    DomainEvent::WorkItemFinished {
                        work_item_id: id,
                        succeeded: false,
                    },
                )
                .await?,
            );
        }
        self.commit(tx, events).await?;
        Ok(status)
    }

    async fn release(&self, id: WorkItemId, worker: &str) -> CoreResult<()> {
        sqlx::query(
            "UPDATE work_items SET status = 'queued', lease_owner = NULL, lease_expires_at = NULL,
                    attempts = GREATEST(attempts - 1, 0)
             WHERE id = $1 AND status = 'leased' AND lease_owner = $2",
        )
        .bind(id.0)
        .bind(worker)
        .execute(self.pool())
        .await
        .map_err(db_err)?;
        Ok(())
    }

    async fn recover_expired(&self) -> CoreResult<u64> {
        let r = sqlx::query(
            "UPDATE work_items
             SET status = CASE WHEN attempts < max_attempts THEN 'queued' ELSE 'failed' END,
                 last_error = CASE WHEN attempts < max_attempts THEN last_error ELSE 'lease expired' END,
                 lease_owner = NULL, lease_expires_at = NULL
             WHERE status = 'leased' AND lease_expires_at < now()",
        )
        .execute(self.pool())
        .await
        .map_err(db_err)?;
        Ok(r.rows_affected())
    }

    async fn cancel_task_items(&self, task_id: TaskId) -> CoreResult<u64> {
        let r = sqlx::query(
            "UPDATE work_items SET status = 'cancelled', lease_owner = NULL, lease_expires_at = NULL
             WHERE task_id = $1 AND status IN ('queued', 'leased')",
        )
        .bind(task_id.0)
        .execute(self.pool())
        .await
        .map_err(db_err)?;
        Ok(r.rows_affected())
    }

    async fn counts(&self, task_id: TaskId) -> CoreResult<QueueCounts> {
        let rows: Vec<(String, i64)> = sqlx::query_as(
            "SELECT status, count(*) FROM work_items WHERE task_id = $1 GROUP BY status",
        )
        .bind(task_id.0)
        .fetch_all(self.pool())
        .await
        .map_err(db_err)?;
        let mut counts = QueueCounts::default();
        for (status, n) in rows {
            let n = to_u64(n);
            match status.parse::<WorkStatus>()? {
                WorkStatus::Queued => counts.queued = n,
                WorkStatus::Leased => counts.leased = n,
                WorkStatus::Done => counts.done = n,
                WorkStatus::Failed => counts.failed = n,
                WorkStatus::Cancelled => counts.cancelled = n,
            }
        }
        Ok(counts)
    }

    async fn get_item(&self, id: WorkItemId) -> CoreResult<WorkItem> {
        let row = sqlx::query("SELECT * FROM work_items WHERE id = $1")
            .bind(id.0)
            .fetch_optional(self.pool())
            .await
            .map_err(db_err)?
            .ok_or_else(|| CoreError::not_found(format!("work item {id}")))?;
        item_from_row(&row)
    }
}
