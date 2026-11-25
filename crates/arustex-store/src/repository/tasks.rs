use arustex_core::domain::exploration::{NodeKind, ReviewCoverage};
use arustex_core::domain::task::{NewTask, Task, TaskStatus};
use arustex_core::domain::{DatasetId, ExplorationId, NodeId, RecordId, TaskId};
use arustex_core::events::DomainEvent;
use arustex_core::ipc::TaskDetail;
use arustex_core::ports::{TaskStore, WorkQueue};
use arustex_core::{CoreError, CoreResult, Page, Paged};
use async_trait::async_trait;
use sqlx::Row;
use sqlx::postgres::PgRow;

use crate::postgres::{Store, audit, db_err, insert_event, to_i32, to_i64, to_u32, to_u64};

const TASK_COLUMNS: &str = "t.id, t.name, t.objective, t.description, t.status, t.exploration_id,
    t.batch_size, t.max_workers, t.timeout_seconds, t.token_budget, t.tokens_used, t.legacy_id,
    t.created_at, t.updated_at, t.started_at, t.completed_at,
    COALESCE((SELECT array_agg(d.dataset_id ORDER BY d.dataset_id) FROM task_datasets d
              WHERE d.task_id = t.id), '{}') AS dataset_ids";

pub(crate) fn task_from_row(row: &PgRow) -> CoreResult<Task> {
    let status: String = row.try_get("status").map_err(db_err)?;
    let datasets: Vec<i64> = row.try_get("dataset_ids").map_err(db_err)?;
    Ok(Task {
        id: TaskId(row.try_get("id").map_err(db_err)?),
        name: row.try_get("name").map_err(db_err)?,
        objective: row.try_get("objective").map_err(db_err)?,
        description: row.try_get("description").map_err(db_err)?,
        status: status.parse()?,
        exploration_id: ExplorationId(row.try_get("exploration_id").map_err(db_err)?),
        dataset_ids: datasets.into_iter().map(DatasetId).collect(),
        batch_size: to_u32(row.try_get("batch_size").map_err(db_err)?),
        max_workers: to_u32(row.try_get("max_workers").map_err(db_err)?),
        timeout_seconds: to_u32(row.try_get("timeout_seconds").map_err(db_err)?),
        token_budget: to_u64(row.try_get("token_budget").map_err(db_err)?),
        tokens_used: to_u64(row.try_get("tokens_used").map_err(db_err)?),
        legacy_id: row.try_get("legacy_id").map_err(db_err)?,
        created_at: row.try_get("created_at").map_err(db_err)?,
        updated_at: row.try_get("updated_at").map_err(db_err)?,
        started_at: row.try_get("started_at").map_err(db_err)?,
        completed_at: row.try_get("completed_at").map_err(db_err)?,
    })
}

impl Store {
    /// Creates a task with its exploration graph (begin and goal nodes) in one transaction.
    pub async fn create_task(&self, input: &NewTask, actor: &str) -> CoreResult<Task> {
        self.create_task_with_legacy(input, actor, None).await
    }

    pub(crate) async fn create_task_with_legacy(
        &self,
        input: &NewTask,
        actor: &str,
        legacy: Option<(TaskId, TaskStatus)>,
    ) -> CoreResult<Task> {
        input.validate()?;
        let mut tx = self.begin().await?;
        let exploration_id: i64 =
            sqlx::query_scalar("INSERT INTO explorations (objective) VALUES ($1) RETURNING id")
                .bind(&input.objective)
                .fetch_one(&mut *tx)
                .await
                .map_err(db_err)?;
        let begin: i64 = sqlx::query_scalar(
            "INSERT INTO exploration_nodes (exploration_id, kind, state, title)
             VALUES ($1, 'begin', 'open', 'begin') RETURNING id",
        )
        .bind(exploration_id)
        .fetch_one(&mut *tx)
        .await
        .map_err(db_err)?;
        let goal: i64 = sqlx::query_scalar(
            "INSERT INTO exploration_nodes (exploration_id, kind, state, title)
             VALUES ($1, 'goal', 'open', $2) RETURNING id",
        )
        .bind(exploration_id)
        .bind(&input.objective)
        .fetch_one(&mut *tx)
        .await
        .map_err(db_err)?;
        sqlx::query(
            "INSERT INTO exploration_edges (exploration_id, src_id, dst_id, rel) VALUES ($1, $2, $3, 'derived_from')",
        )
        .bind(exploration_id)
        .bind(goal)
        .bind(begin)
        .execute(&mut *tx)
        .await
        .map_err(db_err)?;
        let (preserve_id, status) = match legacy {
            Some((id, status)) => (Some(id.0), status),
            None => (None, TaskStatus::Created),
        };
        let id: i64 = sqlx::query_scalar(
            "INSERT INTO tasks (id, name, objective, description, status, exploration_id, batch_size,
                                max_workers, timeout_seconds, token_budget, legacy_id)
             VALUES (COALESCE($1, nextval(pg_get_serial_sequence('tasks', 'id'))), $2, $3, $4, $5,
                     $6, $7, $8, $9, $10, $1)
             RETURNING id",
        )
        .bind(preserve_id)
        .bind(&input.name)
        .bind(&input.objective)
        .bind(&input.description)
        .bind(status.as_str())
        .bind(exploration_id)
        .bind(to_i32(input.batch_size)?)
        .bind(to_i32(input.max_workers)?)
        .bind(to_i32(input.timeout_seconds)?)
        .bind(to_i64(input.token_budget)?)
        .fetch_one(&mut *tx)
        .await
        .map_err(db_err)?;
        for dataset in &input.dataset_ids {
            sqlx::query("INSERT INTO task_datasets (task_id, dataset_id) VALUES ($1, $2)")
                .bind(id)
                .bind(dataset.0)
                .execute(&mut *tx)
                .await
                .map_err(db_err)?;
        }
        let event = insert_event(
            &mut tx,
            Some(TaskId(id)),
            None,
            DomainEvent::TaskCreated {
                task_id: TaskId(id),
                name: input.name.clone(),
            },
        )
        .await?;
        audit(
            &mut tx,
            actor,
            "task.create",
            &format!("task:{id}"),
            serde_json::json!({}),
        )
        .await?;
        self.commit(tx, vec![event]).await?;
        self.get_task(TaskId(id)).await
    }

    pub async fn list_tasks(&self, page: Page) -> CoreResult<Paged<Task>> {
        let page = page.clamped();
        let total: i64 = sqlx::query_scalar("SELECT count(*) FROM tasks")
            .fetch_one(self.pool())
            .await
            .map_err(db_err)?;
        let sql =
            format!("SELECT {TASK_COLUMNS} FROM tasks t ORDER BY t.id DESC LIMIT $1 OFFSET $2");
        let rows = sqlx::query(sqlx::AssertSqlSafe(sql))
            .bind(i64::from(page.limit))
            .bind(i64::from(page.offset))
            .fetch_all(self.pool())
            .await
            .map_err(db_err)?;
        let items = rows.iter().map(task_from_row).collect::<CoreResult<_>>()?;
        Ok(Paged::new(items, to_u64(total), page))
    }

    pub async fn task_detail(&self, id: TaskId) -> CoreResult<TaskDetail> {
        let task = self.get_task(id).await?;
        let queue = self.counts(id).await?;
        let coverage = self.task_coverage(id).await?;
        let findings: i64 = sqlx::query_scalar("SELECT count(*) FROM findings WHERE task_id = $1")
            .bind(id.0)
            .fetch_one(self.pool())
            .await
            .map_err(db_err)?;
        Ok(TaskDetail {
            task,
            queue,
            coverage,
            findings: to_u64(findings),
        })
    }

    /// Review coverage computed in SQL over the task's in-scope records.
    pub async fn task_coverage(&self, id: TaskId) -> CoreResult<ReviewCoverage> {
        let row = sqlx::query(
            "WITH scope AS (
                 SELECT r.id FROM records r JOIN task_datasets td ON td.dataset_id = r.dataset_id
                 WHERE td.task_id = $1
             ), reviewed AS (
                 SELECT DISTINCT a.record_id FROM node_record_anchors a
                 JOIN exploration_nodes n ON n.id = a.node_id
                 JOIN tasks t ON t.exploration_id = n.exploration_id AND t.id = $1
                 WHERE (n.kind = 'intent' AND n.state IN ('done', 'exhausted', 'stopped', 'deleted'))
                    OR (n.kind IN ('fact', 'finding') AND n.state IN ('confirmed', 'dismissed'))
             )
             SELECT (SELECT count(*) FROM scope) AS total,
                    (SELECT count(*) FROM reviewed WHERE record_id IN (SELECT id FROM scope)) AS reviewed",
        )
        .bind(id.0)
        .fetch_one(self.pool())
        .await
        .map_err(db_err)?;
        Ok(ReviewCoverage {
            total: to_u64(row.try_get("total").map_err(db_err)?),
            reviewed: to_u64(row.try_get("reviewed").map_err(db_err)?),
        })
    }

    pub async fn task_counts_by_status(&self) -> CoreResult<Vec<(String, u64)>> {
        let rows: Vec<(String, i64)> =
            sqlx::query_as("SELECT status, count(*) FROM tasks GROUP BY status ORDER BY status")
                .fetch_all(self.pool())
                .await
                .map_err(db_err)?;
        Ok(rows.into_iter().map(|(s, c)| (s, to_u64(c))).collect())
    }

    /// Running tasks whose wall-clock limit has elapsed.
    pub async fn overdue_tasks(&self) -> CoreResult<Vec<TaskId>> {
        let ids: Vec<i64> = sqlx::query_scalar(
            "SELECT id FROM tasks WHERE status = 'running' AND timeout_seconds > 0
               AND started_at + make_interval(secs => timeout_seconds) < now()",
        )
        .fetch_all(self.pool())
        .await
        .map_err(db_err)?;
        Ok(ids.into_iter().map(TaskId).collect())
    }

    async fn begin_node(&self, task_id: TaskId) -> CoreResult<(ExplorationId, NodeId)> {
        let row: (i64, i64) = sqlx::query_as(
            "SELECT t.exploration_id, n.id FROM tasks t
             JOIN exploration_nodes n ON n.exploration_id = t.exploration_id AND n.kind = 'begin'
             WHERE t.id = $1",
        )
        .bind(task_id.0)
        .fetch_optional(self.pool())
        .await
        .map_err(db_err)?
        .ok_or_else(|| CoreError::not_found(format!("task {task_id}")))?;
        Ok((ExplorationId(row.0), NodeId(row.1)))
    }
}

#[async_trait]
impl TaskStore for Store {
    async fn get_task(&self, id: TaskId) -> CoreResult<Task> {
        let sql = format!("SELECT {TASK_COLUMNS} FROM tasks t WHERE t.id = $1");
        let row = sqlx::query(sqlx::AssertSqlSafe(sql))
            .bind(id.0)
            .fetch_optional(self.pool())
            .await
            .map_err(db_err)?
            .ok_or_else(|| CoreError::not_found(format!("task {id}")))?;
        task_from_row(&row)
    }

    async fn transition_task(
        &self,
        id: TaskId,
        to: TaskStatus,
        reason: Option<String>,
    ) -> CoreResult<Task> {
        let mut tx = self.begin().await?;
        let current: String =
            sqlx::query_scalar("SELECT status FROM tasks WHERE id = $1 FOR UPDATE")
                .bind(id.0)
                .fetch_optional(&mut *tx)
                .await
                .map_err(db_err)?
                .ok_or_else(|| CoreError::not_found(format!("task {id}")))?;
        let from: TaskStatus = current.parse()?;
        from.ensure_transition(to)?;
        sqlx::query(
            "UPDATE tasks SET status = $2,
                started_at = CASE WHEN $2 = 'running' AND started_at IS NULL THEN now() ELSE started_at END,
                completed_at = CASE WHEN $2 IN ('completed', 'failed', 'cancelled', 'timed_out')
                                    THEN now() ELSE completed_at END
             WHERE id = $1",
        )
        .bind(id.0)
        .bind(to.as_str())
        .execute(&mut *tx)
        .await
        .map_err(db_err)?;
        if to.is_terminal() {
            let exploration_status = if to == TaskStatus::Completed {
                "achieved"
            } else {
                "failed"
            };
            sqlx::query(
                "UPDATE explorations SET status = $2 WHERE id = (SELECT exploration_id FROM tasks WHERE id = $1)",
            )
            .bind(id.0)
            .bind(exploration_status)
            .execute(&mut *tx)
            .await
            .map_err(db_err)?;
        }
        let event = insert_event(
            &mut tx,
            Some(id),
            None,
            DomainEvent::TaskStatusChanged {
                task_id: id,
                from,
                to,
                reason,
            },
        )
        .await?;
        self.commit(tx, vec![event]).await?;
        self.get_task(id).await
    }

    async fn active_tasks(&self, limit: u32) -> CoreResult<Vec<Task>> {
        let sql = format!(
            "SELECT {TASK_COLUMNS} FROM tasks t WHERE t.status IN ('queued', 'running')
             ORDER BY t.id LIMIT $1"
        );
        let rows = sqlx::query(sqlx::AssertSqlSafe(sql))
            .bind(i64::from(limit.min(1000)))
            .fetch_all(self.pool())
            .await
            .map_err(db_err)?;
        rows.iter().map(task_from_row).collect()
    }

    async fn add_task_tokens(&self, id: TaskId, tokens: u64) -> CoreResult<Task> {
        sqlx::query("UPDATE tasks SET tokens_used = tokens_used + $2 WHERE id = $1")
            .bind(id.0)
            .bind(to_i64(tokens)?)
            .execute(self.pool())
            .await
            .map_err(db_err)?;
        self.get_task(id).await
    }

    async fn task_record_ids(&self, id: TaskId) -> CoreResult<Vec<RecordId>> {
        let ids: Vec<i64> = sqlx::query_scalar(
            "SELECT r.id FROM records r JOIN task_datasets td ON td.dataset_id = r.dataset_id
             WHERE td.task_id = $1 ORDER BY r.id",
        )
        .bind(id.0)
        .fetch_all(self.pool())
        .await
        .map_err(db_err)?;
        Ok(ids.into_iter().map(RecordId).collect())
    }

    async fn plan_node(
        &self,
        task_id: TaskId,
        title: &str,
        records: &[RecordId],
    ) -> CoreResult<NodeId> {
        let (exploration, begin) = self.begin_node(task_id).await?;
        let mut tx = self.begin().await?;
        let node: i64 = sqlx::query_scalar(
            "INSERT INTO exploration_nodes (exploration_id, kind, state, title, origin)
             VALUES ($1, $2, 'open', $3, 'planner') RETURNING id",
        )
        .bind(exploration.0)
        .bind(NodeKind::Intent.as_str())
        .bind(title)
        .fetch_one(&mut *tx)
        .await
        .map_err(db_err)?;
        sqlx::query(
            "INSERT INTO exploration_edges (exploration_id, src_id, dst_id, rel) VALUES ($1, $2, $3, 'spawns')",
        )
        .bind(exploration.0)
        .bind(begin.0)
        .bind(node)
        .execute(&mut *tx)
        .await
        .map_err(db_err)?;
        let ids: Vec<i64> = records.iter().map(|r| r.0).collect();
        sqlx::query(
            "INSERT INTO node_record_anchors (node_id, record_id) SELECT $1, unnest($2::bigint[])
             ON CONFLICT DO NOTHING",
        )
        .bind(node)
        .bind(&ids)
        .execute(&mut *tx)
        .await
        .map_err(db_err)?;
        sqlx::query("UPDATE explorations SET round = round + 1 WHERE id = $1")
            .bind(exploration.0)
            .execute(&mut *tx)
            .await
            .map_err(db_err)?;
        self.commit(tx, Vec::new()).await?;
        Ok(NodeId(node))
    }

    async fn set_node_state(&self, node: NodeId, state: &str) -> CoreResult<()> {
        let kind: String = sqlx::query_scalar("SELECT kind FROM exploration_nodes WHERE id = $1")
            .bind(node.0)
            .fetch_optional(self.pool())
            .await
            .map_err(db_err)?
            .ok_or_else(|| CoreError::not_found(format!("node {node}")))?;
        let kind: NodeKind = kind.parse()?;
        kind.validate_state(state)?;
        let closed = kind.is_closed(state);
        sqlx::query(
            "UPDATE exploration_nodes SET state = $2,
                completed_at = CASE WHEN $3 THEN COALESCE(completed_at, now()) ELSE NULL END
             WHERE id = $1",
        )
        .bind(node.0)
        .bind(state)
        .bind(closed)
        .execute(self.pool())
        .await
        .map_err(db_err)?;
        Ok(())
    }
}
