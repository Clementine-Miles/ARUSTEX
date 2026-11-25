use arustex_core::domain::agent::{
    NewSession, PromptTemplate, Session, SessionId, SessionStatus, TerminalOutcome,
    TokenUsage, TranscriptEntry, TranscriptKind,
};
use arustex_core::domain::{TaskId, WorkItemId};
use arustex_core::events::DomainEvent;
use arustex_core::ports::SessionStore;
use arustex_core::{CoreError, CoreResult, Page, Paged};
use async_trait::async_trait;
use sqlx::Row;
use sqlx::postgres::PgRow;

use crate::postgres::{Store, db_err, insert_event, parse_json, to_i32, to_i64, to_u32, to_u64};

/// Upper bound on one transcript entry, so a runaway model reply cannot bloat storage.
pub const MAX_TRANSCRIPT_ENTRY_BYTES: usize = 256 * 1024;
/// Upper bound on a session checkpoint.
pub const MAX_CHECKPOINT_BYTES: usize = 1024 * 1024;

fn session_from_row(row: &PgRow) -> CoreResult<Session> {
    let role: String = row.try_get("role").map_err(db_err)?;
    let status: String = row.try_get("status").map_err(db_err)?;
    let outcome: Option<serde_json::Value> = row.try_get("outcome").map_err(db_err)?;
    Ok(Session {
        id: SessionId(row.try_get("id").map_err(db_err)?),
        task_id: row.try_get::<Option<i64>, _>("task_id").map_err(db_err)?.map(TaskId),
        work_item_id: row
            .try_get::<Option<i64>, _>("work_item_id")
            .map_err(db_err)?
            .map(WorkItemId),
        role: role.parse()?,
        agent_key: row.try_get("agent_key").map_err(db_err)?,
        status: status.parse()?,
        prompt_key: row.try_get("prompt_key").map_err(db_err)?,
        prompt_version: row.try_get("prompt_version").map_err(db_err)?,
        model_profile: row.try_get("model_profile").map_err(db_err)?,
        model: row.try_get("model").map_err(db_err)?,
        token_budget: to_u64(row.try_get("token_budget").map_err(db_err)?),
        tokens_used: to_u64(row.try_get("tokens_used").map_err(db_err)?),
        turn: to_u32(row.try_get("turn").map_err(db_err)?),
        max_turns: to_u32(row.try_get("max_turns").map_err(db_err)?),
        outcome: outcome.map(parse_json).transpose()?,
        created_at: row.try_get("created_at").map_err(db_err)?,
        updated_at: row.try_get("updated_at").map_err(db_err)?,
    })
}

fn entry_from_row(row: &PgRow) -> CoreResult<TranscriptEntry> {
    let kind: String = row.try_get("kind").map_err(db_err)?;
    Ok(TranscriptEntry {
        session_id: SessionId(row.try_get("session_id").map_err(db_err)?),
        seq: row.try_get("seq").map_err(db_err)?,
        kind: kind.parse()?,
        content: row.try_get("content").map_err(db_err)?,
        input_tokens: to_u32(row.try_get("input_tokens").map_err(db_err)?),
        output_tokens: to_u32(row.try_get("output_tokens").map_err(db_err)?),
        created_at: row.try_get("created_at").map_err(db_err)?,
    })
}

fn saturating_i32(value: u64) -> i32 {
    i32::try_from(value).unwrap_or(i32::MAX)
}

const SESSION_COLUMNS: &str = "id, task_id, work_item_id, role, agent_key, status, prompt_key,
    prompt_version, model_profile, model, token_budget, tokens_used, turn, max_turns, outcome,
    created_at, updated_at";

impl Store {
    /// Stores a new version of a prompt template and returns it.
    pub async fn put_prompt_template(&self, key: &str, body: &str) -> CoreResult<PromptTemplate> {
        arustex_core::domain::agent::AgentDefinition::validate_key(key)?;
        if body.is_empty() || body.len() > 65_536 {
            return Err(CoreError::invalid("prompt template body must be 1..=65536 bytes"));
        }
        let mut tx = self.begin().await?;
        sqlx::query("SELECT pg_advisory_xact_lock(hashtext($1))")
            .bind(key)
            .execute(&mut *tx)
            .await
            .map_err(db_err)?;
        let row = sqlx::query(
            "INSERT INTO prompt_templates (key, version, body)
             VALUES ($1, COALESCE((SELECT max(version) FROM prompt_templates WHERE key = $1), 0) + 1, $2)
             RETURNING key, version, body, created_at",
        )
        .bind(key)
        .bind(body)
        .fetch_one(&mut *tx)
        .await
        .map_err(db_err)?;
        self.commit(tx, Vec::new()).await?;
        Ok(PromptTemplate {
            key: row.try_get("key").map_err(db_err)?,
            version: row.try_get("version").map_err(db_err)?,
            body: row.try_get("body").map_err(db_err)?,
            created_at: row.try_get("created_at").map_err(db_err)?,
        })
    }

    /// Returns the latest version of a template, if any.
    pub async fn latest_prompt_template(&self, key: &str) -> CoreResult<Option<PromptTemplate>> {
        let row = sqlx::query(
            "SELECT key, version, body, created_at FROM prompt_templates WHERE key = $1
             ORDER BY version DESC LIMIT 1",
        )
        .bind(key)
        .fetch_optional(self.pool())
        .await
        .map_err(db_err)?;
        row.map(|row| {
            Ok(PromptTemplate {
                key: row.try_get("key").map_err(db_err)?,
                version: row.try_get("version").map_err(db_err)?,
                body: row.try_get("body").map_err(db_err)?,
                created_at: row.try_get("created_at").map_err(db_err)?,
            })
        })
        .transpose()
    }

    pub async fn list_sessions(&self, task: Option<TaskId>, page: Page) -> CoreResult<Paged<Session>> {
        let page = page.clamped();
        let total: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM sessions WHERE ($1::bigint IS NULL OR task_id = $1)",
        )
        .bind(task.map(|t| t.0))
        .fetch_one(self.pool())
        .await
        .map_err(db_err)?;
        let sql = format!(
            "SELECT {SESSION_COLUMNS} FROM sessions WHERE ($1::bigint IS NULL OR task_id = $1)
             ORDER BY created_at DESC, id LIMIT $2 OFFSET $3"
        );
        let rows = sqlx::query(sqlx::AssertSqlSafe(sql))
            .bind(task.map(|t| t.0))
            .bind(i64::from(page.limit))
            .bind(i64::from(page.offset))
            .fetch_all(self.pool())
            .await
            .map_err(db_err)?;
        let items = rows.iter().map(session_from_row).collect::<CoreResult<_>>()?;
        Ok(Paged::new(items, to_u64(total), page))
    }

    /// Sessions left in a non-terminal state by a crashed daemon.
    pub async fn interrupted_sessions(&self) -> CoreResult<Vec<Session>> {
        let sql = format!(
            "SELECT {SESSION_COLUMNS} FROM sessions
             WHERE status IN ('pending', 'running', 'awaiting_approval') ORDER BY created_at"
        );
        let rows = sqlx::query(sqlx::AssertSqlSafe(sql))
            .fetch_all(self.pool())
            .await
            .map_err(db_err)?;
        rows.iter().map(session_from_row).collect()
    }

    pub async fn active_session_count(&self) -> CoreResult<u64> {
        let n: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM sessions WHERE status IN ('pending', 'running', 'awaiting_approval')",
        )
        .fetch_one(self.pool())
        .await
        .map_err(db_err)?;
        Ok(to_u64(n))
    }

    /// Total tokens recorded today (UTC), for the dashboard.
    pub async fn tokens_today(&self) -> CoreResult<u64> {
        let n: i64 = sqlx::query_scalar(
            "SELECT COALESCE(sum(input_tokens + output_tokens), 0)::bigint FROM llm_usage
             WHERE at >= date_trunc('day', now() AT TIME ZONE 'utc') AT TIME ZONE 'utc'",
        )
        .fetch_one(self.pool())
        .await
        .map_err(db_err)?;
        Ok(to_u64(n))
    }
}

#[async_trait]
impl SessionStore for Store {
    async fn create_session(&self, s: NewSession) -> CoreResult<Session> {
        arustex_core::domain::agent::AgentDefinition::validate_key(&s.agent_key)?;
        if s.max_turns == 0 {
            return Err(CoreError::invalid("max_turns must be at least 1"));
        }
        let mut tx = self.begin().await?;
        sqlx::query(
            "INSERT INTO sessions (id, task_id, work_item_id, role, agent_key, prompt_key,
                                   prompt_version, model_profile, model, token_budget, max_turns)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11)",
        )
        .bind(s.id.0)
        .bind(s.task_id.map(|t| t.0))
        .bind(s.work_item_id.map(|w| w.0))
        .bind(s.role.as_str())
        .bind(&s.agent_key)
        .bind(&s.prompt_key)
        .bind(s.prompt_version)
        .bind(&s.model_profile)
        .bind(&s.model)
        .bind(to_i64(s.token_budget)?)
        .bind(to_i32(s.max_turns)?)
        .execute(&mut *tx)
        .await
        .map_err(db_err)?;
        let event = insert_event(
            &mut tx,
            s.task_id,
            Some(s.id),
            DomainEvent::SessionStarted { session_id: s.id, role: s.role },
        )
        .await?;
        self.commit(tx, vec![event]).await?;
        self.get_session(s.id).await
    }

    async fn get_session(&self, id: SessionId) -> CoreResult<Session> {
        let sql = format!("SELECT {SESSION_COLUMNS} FROM sessions WHERE id = $1");
        let row = sqlx::query(sqlx::AssertSqlSafe(sql))
            .bind(id.0)
            .fetch_optional(self.pool())
            .await
            .map_err(db_err)?
            .ok_or_else(|| CoreError::not_found(format!("session {id}")))?;
        session_from_row(&row)
    }

    async fn update_session_progress(
        &self,
        id: SessionId,
        status: SessionStatus,
        turn: u32,
        tokens_used: u64,
    ) -> CoreResult<()> {
        if status.is_terminal() {
            return Err(CoreError::invalid("use finish_session for terminal states"));
        }
        let mut tx = self.begin().await?;
        let row = sqlx::query(
            "SELECT status, task_id FROM sessions WHERE id = $1 FOR UPDATE",
        )
        .bind(id.0)
        .fetch_optional(&mut *tx)
        .await
        .map_err(db_err)?
        .ok_or_else(|| CoreError::not_found(format!("session {id}")))?;
        let current: SessionStatus = row.try_get::<String, _>("status").map_err(db_err)?.parse()?;
        let task: Option<i64> = row.try_get("task_id").map_err(db_err)?;
        if current.is_terminal() {
            return Err(CoreError::Conflict(format!("session {id} already finished")));
        }
        sqlx::query("UPDATE sessions SET status = $2, turn = $3, tokens_used = $4 WHERE id = $1")
            .bind(id.0)
            .bind(status.as_str())
            .bind(to_i32(turn)?)
            .bind(to_i64(tokens_used)?)
            .execute(&mut *tx)
            .await
            .map_err(db_err)?;
        let mut events = Vec::new();
        if current != status {
            events.push(
                insert_event(
                    &mut tx,
                    task.map(TaskId),
                    Some(id),
                    DomainEvent::SessionStatusChanged { session_id: id, status },
                )
                .await?,
            );
        }
        self.commit(tx, events).await
    }

    async fn finish_session(&self, id: SessionId, outcome: &TerminalOutcome) -> CoreResult<()> {
        let status = outcome.status();
        let payload = serde_json::to_value(outcome).map_err(|e| CoreError::Internal(e.to_string()))?;
        let mut tx = self.begin().await?;
        let task: Option<Option<i64>> = sqlx::query_scalar(
            "UPDATE sessions SET status = $2, outcome = $3, checkpoint = NULL
             WHERE id = $1 AND status NOT IN ('completed', 'needs_review', 'failed', 'cancelled')
             RETURNING task_id",
        )
        .bind(id.0)
        .bind(status.as_str())
        .bind(payload)
        .fetch_optional(&mut *tx)
        .await
        .map_err(db_err)?;
        let Some(task) = task else {
            // Either unknown or already finished; finishing twice is a no-op.
            self.get_session(id).await?;
            return Ok(());
        };
        let event = insert_event(
            &mut tx,
            task.map(TaskId),
            Some(id),
            DomainEvent::SessionStatusChanged { session_id: id, status },
        )
        .await?;
        self.commit(tx, vec![event]).await
    }

    async fn append_transcript(
        &self,
        id: SessionId,
        kind: TranscriptKind,
        content: serde_json::Value,
        usage: TokenUsage,
    ) -> CoreResult<TranscriptEntry> {
        if content.to_string().len() > MAX_TRANSCRIPT_ENTRY_BYTES {
            return Err(CoreError::LimitExceeded("transcript entry too large".into()));
        }
        let mut tx = self.begin().await?;
        let task: Option<i64> = sqlx::query_scalar("SELECT task_id FROM sessions WHERE id = $1 FOR UPDATE")
            .bind(id.0)
            .fetch_optional(&mut *tx)
            .await
            .map_err(db_err)?
            .ok_or_else(|| CoreError::not_found(format!("session {id}")))?;
        let row = sqlx::query(
            "INSERT INTO transcript_entries (session_id, seq, kind, content, input_tokens, output_tokens)
             VALUES ($1, COALESCE((SELECT max(seq) FROM transcript_entries WHERE session_id = $1), 0) + 1,
                     $2, $3, $4, $5)
             RETURNING session_id, seq, kind, content, input_tokens, output_tokens, created_at",
        )
        .bind(id.0)
        .bind(kind.as_str())
        .bind(&content)
        .bind(saturating_i32(usage.input_tokens))
        .bind(saturating_i32(usage.output_tokens))
        .fetch_one(&mut *tx)
        .await
        .map_err(db_err)?;
        let entry = entry_from_row(&row)?;
        let event = insert_event(
            &mut tx,
            task.map(TaskId),
            Some(id),
            DomainEvent::TranscriptAppended { session_id: id, seq: entry.seq },
        )
        .await?;
        self.commit(tx, vec![event]).await?;
        Ok(entry)
    }

    async fn transcript(&self, id: SessionId, after_seq: i64, limit: u32) -> CoreResult<Vec<TranscriptEntry>> {
        let rows = sqlx::query(
            "SELECT session_id, seq, kind, content, input_tokens, output_tokens, created_at
             FROM transcript_entries WHERE session_id = $1 AND seq > $2 ORDER BY seq LIMIT $3",
        )
        .bind(id.0)
        .bind(after_seq)
        .bind(i64::from(limit.clamp(1, 1000)))
        .fetch_all(self.pool())
        .await
        .map_err(db_err)?;
        rows.iter().map(entry_from_row).collect()
    }

    async fn save_checkpoint(&self, id: SessionId, checkpoint: serde_json::Value) -> CoreResult<()> {
        if checkpoint.to_string().len() > MAX_CHECKPOINT_BYTES {
            return Err(CoreError::LimitExceeded("checkpoint too large".into()));
        }
        let result = sqlx::query(
            "UPDATE sessions SET checkpoint = $2
             WHERE id = $1 AND status NOT IN ('completed', 'needs_review', 'failed', 'cancelled')",
        )
        .bind(id.0)
        .bind(checkpoint)
        .execute(self.pool())
        .await
        .map_err(db_err)?;
        if result.rows_affected() == 0 {
            self.get_session(id).await?;
            return Err(CoreError::Conflict(format!("session {id} already finished")));
        }
        Ok(())
    }

    async fn load_checkpoint(&self, id: SessionId) -> CoreResult<Option<serde_json::Value>> {
        let row: Option<Option<serde_json::Value>> =
            sqlx::query_scalar("SELECT checkpoint FROM sessions WHERE id = $1")
                .bind(id.0)
                .fetch_optional(self.pool())
                .await
                .map_err(db_err)?;
        row.ok_or_else(|| CoreError::not_found(format!("session {id}")))
    }

    async fn record_usage(&self, id: SessionId, profile: &str, model: &str, usage: TokenUsage) -> CoreResult<()> {
        sqlx::query(
            "INSERT INTO llm_usage (session_id, profile, model, input_tokens, output_tokens,
                                    cache_read_tokens, cache_write_tokens)
             VALUES ($1, $2, $3, $4, $5, $6, $7)",
        )
        .bind(id.0)
        .bind(profile)
        .bind(model)
        .bind(to_i64(usage.input_tokens)?)
        .bind(to_i64(usage.output_tokens)?)
        .bind(to_i64(usage.cache_read_tokens)?)
        .bind(to_i64(usage.cache_write_tokens)?)
        .execute(self.pool())
        .await
        .map_err(db_err)?;
        Ok(())
    }
}
