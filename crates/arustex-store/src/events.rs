use arustex_core::domain::TaskId;
use arustex_core::domain::agent::SessionId;
use arustex_core::events::{DomainEvent, EventEnvelope};
use arustex_core::ports::EventSink;
use arustex_core::{CoreError, CoreResult};
use async_trait::async_trait;
use chrono::Utc;
use sqlx::Row;

use crate::postgres::{Store, db_err, insert_event, parse_json};

impl Store {
    /// Stored events with `seq > after_seq`, ascending, at most `limit`.
    pub async fn events_after(&self, after_seq: i64, limit: u32) -> CoreResult<Vec<EventEnvelope>> {
        let rows = sqlx::query(
            "SELECT seq, at, task_id, session_id, payload FROM events
             WHERE seq > $1 ORDER BY seq LIMIT $2",
        )
        .bind(after_seq)
        .bind(i64::from(limit.min(10_000)))
        .fetch_all(self.pool())
        .await
        .map_err(db_err)?;
        rows.into_iter().map(|row| envelope_from_row(&row)).collect()
    }

    pub async fn recent_events(&self, limit: u32) -> CoreResult<Vec<EventEnvelope>> {
        let rows = sqlx::query(
            "SELECT seq, at, task_id, session_id, payload FROM events ORDER BY seq DESC LIMIT $1",
        )
        .bind(i64::from(limit.min(500)))
        .fetch_all(self.pool())
        .await
        .map_err(db_err)?;
        let mut events: Vec<_> = rows
            .into_iter()
            .map(|row| envelope_from_row(&row))
            .collect::<CoreResult<_>>()?;
        events.reverse();
        Ok(events)
    }

    pub async fn latest_event_seq(&self) -> CoreResult<i64> {
        let seq: Option<i64> = sqlx::query_scalar("SELECT max(seq) FROM events")
            .fetch_one(self.pool())
            .await
            .map_err(db_err)?;
        Ok(seq.unwrap_or(0))
    }

    /// Records and broadcasts one event in its own transaction.
    pub async fn record_event(
        &self,
        task_id: Option<TaskId>,
        session_id: Option<SessionId>,
        event: DomainEvent,
    ) -> CoreResult<EventEnvelope> {
        let mut tx = self.begin().await?;
        let envelope = insert_event(&mut tx, task_id, session_id, event).await?;
        self.commit(tx, vec![envelope.clone()]).await?;
        Ok(envelope)
    }
}

fn envelope_from_row(row: &sqlx::postgres::PgRow) -> CoreResult<EventEnvelope> {
    let payload: serde_json::Value = row.try_get("payload").map_err(db_err)?;
    Ok(EventEnvelope {
        seq: row.try_get("seq").map_err(db_err)?,
        at: row.try_get("at").map_err(db_err)?,
        task_id: row.try_get::<Option<i64>, _>("task_id").map_err(db_err)?.map(TaskId),
        session_id: row
            .try_get::<Option<uuid::Uuid>, _>("session_id")
            .map_err(db_err)?
            .map(SessionId),
        event: parse_json(payload)?,
    })
}

#[async_trait]
impl EventSink for Store {
    async fn publish(
        &self,
        task_id: Option<TaskId>,
        session_id: Option<SessionId>,
        event: DomainEvent,
    ) -> CoreResult<EventEnvelope> {
        if event.is_transient() {
            let envelope = EventEnvelope {
                seq: 0,
                at: Utc::now(),
                task_id,
                session_id,
                event,
            };
            self.publish_transient(envelope.clone());
            return Ok(envelope);
        }
        self.record_event(task_id, session_id, event)
            .await
            .map_err(|e| match e {
                CoreError::Invalid(m) => CoreError::Internal(m),
                other => other,
            })
    }
}
