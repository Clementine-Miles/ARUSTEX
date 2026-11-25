use std::time::Duration;

use arustex_core::events::{DomainEvent, EventEnvelope};
use arustex_core::{CoreError, CoreResult};
use sqlx::postgres::{PgConnectOptions, PgPoolOptions};
use sqlx::{PgPool, Postgres, Transaction};
use tokio::sync::broadcast;

/// Connection and resource limits for the store.
#[derive(Debug, Clone)]
pub struct StoreOptions {
    pub max_connections: u32,
    pub acquire_timeout: Duration,
    pub statement_timeout: Duration,
    /// Capacity of the in-process live event channel; slow subscribers resync.
    pub event_buffer: usize,
}

impl Default for StoreOptions {
    fn default() -> Self {
        Self {
            max_connections: 16,
            acquire_timeout: Duration::from_secs(10),
            statement_timeout: Duration::from_secs(30),
            event_buffer: 1024,
        }
    }
}

/// Shared handle to the database pool and the live event channel. Cheap to clone.
#[derive(Debug, Clone)]
pub struct Store {
    pool: PgPool,
    events: broadcast::Sender<EventEnvelope>,
}

impl Store {
    pub async fn connect(url: &str, options: &StoreOptions) -> CoreResult<Self> {
        let connect: PgConnectOptions = url
            .parse()
            .map_err(|e: sqlx::Error| CoreError::invalid(format!("invalid database url: {e}")))?;
        let statement_ms = options.statement_timeout.as_millis().to_string();
        let connect = connect
            .application_name("arustex")
            .options([("statement_timeout", statement_ms.as_str())]);
        let pool = PgPoolOptions::new()
            .max_connections(options.max_connections)
            .acquire_timeout(options.acquire_timeout)
            .connect_with(connect)
            .await
            .map_err(|e| CoreError::Unavailable(format!("cannot connect to PostgreSQL: {e}")))?;
        Ok(Self::from_pool(pool, options.event_buffer))
    }

    pub fn from_pool(pool: PgPool, event_buffer: usize) -> Self {
        let (events, _) = broadcast::channel(event_buffer.max(16));
        Self { pool, events }
    }

    pub fn pool(&self) -> &PgPool {
        &self.pool
    }

    pub fn subscribe_live(&self) -> broadcast::Receiver<EventEnvelope> {
        self.events.subscribe()
    }

    pub(crate) fn broadcast(&self, envelopes: impl IntoIterator<Item = EventEnvelope>) {
        for envelope in envelopes {
            // No receivers is fine; durable events can be replayed from the table.
            let _ = self.events.send(envelope);
        }
    }

    pub async fn begin(&self) -> CoreResult<Transaction<'static, Postgres>> {
        self.pool.begin().await.map_err(db_err)
    }

    /// Commits `tx` and then broadcasts the events it recorded.
    pub(crate) async fn commit(
        &self,
        tx: Transaction<'static, Postgres>,
        events: Vec<EventEnvelope>,
    ) -> CoreResult<()> {
        tx.commit().await.map_err(db_err)?;
        self.broadcast(events);
        Ok(())
    }

    /// Publishes a transient event (live only, never stored).
    pub fn publish_transient(&self, envelope: EventEnvelope) {
        debug_assert!(envelope.event.is_transient());
        self.broadcast([envelope]);
    }

    pub async fn ping(&self) -> CoreResult<()> {
        sqlx::query("SELECT 1")
            .execute(&self.pool)
            .await
            .map(|_| ())
            .map_err(db_err)
    }

    pub async fn close(&self) {
        self.pool.close().await;
    }
}

/// Classifies database errors into domain error classes.
pub fn db_err(err: sqlx::Error) -> CoreError {
    match &err {
        sqlx::Error::RowNotFound => CoreError::NotFound("row not found".into()),
        sqlx::Error::Database(db) => {
            let message = db.message().to_owned();
            match db.code().as_deref() {
                Some("23505") => CoreError::Conflict(message),
                Some("23503") => CoreError::Invalid(format!("reference violation: {message}")),
                Some("23514") | Some("23502") | Some("22001") | Some("22P02") => {
                    CoreError::Invalid(message)
                }
                Some("23000") => CoreError::Integrity(message),
                Some("57014") => CoreError::Timeout(message),
                Some("40001") | Some("40P01") => CoreError::Conflict(format!("retry: {message}")),
                _ => CoreError::Storage(message),
            }
        }
        sqlx::Error::PoolTimedOut => CoreError::Unavailable("database pool timed out".into()),
        sqlx::Error::Io(_) | sqlx::Error::Tls(_) | sqlx::Error::PoolClosed => {
            CoreError::Unavailable(err.to_string())
        }
        _ => CoreError::Storage(err.to_string()),
    }
}

pub(crate) fn to_u64(value: i64) -> u64 {
    u64::try_from(value).unwrap_or(0)
}

pub(crate) fn to_u32(value: i32) -> u32 {
    u32::try_from(value).unwrap_or(0)
}

pub(crate) fn to_i64(value: u64) -> CoreResult<i64> {
    i64::try_from(value).map_err(|_| CoreError::invalid("value exceeds i64 range"))
}

pub(crate) fn to_i32(value: u32) -> CoreResult<i32> {
    i32::try_from(value).map_err(|_| CoreError::invalid("value exceeds i32 range"))
}

pub(crate) fn parse_json<T: serde::de::DeserializeOwned>(
    value: serde_json::Value,
) -> CoreResult<T> {
    serde_json::from_value(value)
        .map_err(|e| CoreError::Integrity(format!("stored JSON is invalid: {e}")))
}

/// Records a durable event inside an open transaction. The returned envelope must be
/// broadcast after the transaction commits.
pub(crate) async fn insert_event(
    tx: &mut Transaction<'static, Postgres>,
    task_id: Option<arustex_core::domain::TaskId>,
    session_id: Option<arustex_core::domain::agent::SessionId>,
    event: DomainEvent,
) -> CoreResult<EventEnvelope> {
    if event.is_transient() {
        return Err(CoreError::invalid("transient events are not stored"));
    }
    // Serialize event writers so that sequence order equals commit order.
    sqlx::query("SELECT pg_advisory_xact_lock(7405623341)")
        .execute(&mut **tx)
        .await
        .map_err(db_err)?;
    let payload = serde_json::to_value(&event).map_err(|e| CoreError::Internal(e.to_string()))?;
    let row: (i64, chrono::DateTime<chrono::Utc>) = sqlx::query_as(
        "INSERT INTO events (kind, task_id, session_id, payload) VALUES ($1, $2, $3, $4)
         RETURNING seq, at",
    )
    .bind(event.kind())
    .bind(task_id.map(|t| t.0))
    .bind(session_id.map(|s| s.0))
    .bind(payload)
    .fetch_one(&mut **tx)
    .await
    .map_err(db_err)?;
    Ok(EventEnvelope {
        seq: row.0,
        at: row.1,
        task_id,
        session_id,
        event,
    })
}

/// Appends an audit-log row inside an open transaction.
pub(crate) async fn audit(
    tx: &mut Transaction<'static, Postgres>,
    actor: &str,
    action: &str,
    subject: &str,
    detail: serde_json::Value,
) -> CoreResult<()> {
    sqlx::query("INSERT INTO audit_log (actor, action, subject, detail) VALUES ($1, $2, $3, $4)")
        .bind(actor)
        .bind(action)
        .bind(subject)
        .bind(detail)
        .execute(&mut **tx)
        .await
        .map_err(db_err)?;
    Ok(())
}
