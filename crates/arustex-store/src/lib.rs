//! Bounded, read-only projections over the legacy PostgreSQL public schema.
//!
//! Use a role with SELECT access to the projected relations and columns. Each
//! pooled session also defaults to read-only transactions. The adapter does
//! not create, apply migrations to, or seed the selected database.

#![forbid(unsafe_code)]

use std::{env, fmt, net::IpAddr, str::FromStr, time::Duration};

use arustex_core::{
    ApprovalSummary, AssetSummary, ExplorationNodeSummary, FindingSummary, MAX_PAGE_SIZE, Page,
    Rows, SessionSummary, TaskSummary, TrafficEvidenceSummary, View,
};
use sqlx::{
    ConnectOptions, PgPool, Row,
    postgres::{PgConnectOptions, PgPoolOptions, PgRow, PgSslMode},
};
use url::{Host, Url};

const STATEMENT_TIMEOUT: Duration = Duration::from_secs(10);
const MAX_CONNECTIONS: u32 = 16;

#[derive(Clone)]
pub struct Store {
    pool: PgPool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StoreError {
    InvalidConnectionCount,
    InvalidDatabaseUrl,
    InvalidDatabaseOptions,
    InsecureDatabaseTransport,
    Connect,
    InvalidLimit,
    InvalidCursor,
    InvalidTask,
    Timeout,
    Query,
}

impl fmt::Display for StoreError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::InvalidConnectionCount => "invalid PostgreSQL connection pool size",
            Self::InvalidDatabaseUrl => "invalid PostgreSQL connection URL",
            Self::InvalidDatabaseOptions => {
                "unsupported or conflicting PostgreSQL connection options"
            }
            Self::InsecureDatabaseTransport => {
                "non-loopback PostgreSQL requires sslmode=verify-full"
            }
            Self::Connect => "failed to establish a read-only PostgreSQL connection",
            Self::InvalidLimit => "invalid page size",
            Self::InvalidCursor => "invalid page cursor",
            Self::InvalidTask => "invalid task ID",
            Self::Timeout => "PostgreSQL page request timed out",
            Self::Query => "PostgreSQL page query failed; check schema and role privileges",
        })
    }
}

impl std::error::Error for StoreError {}

impl Store {
    /// Validate the effective driver options without opening a connection.
    pub fn validate_dsn(dsn: &str) -> Result<(), StoreError> {
        connection_options(dsn).map(|_| ())
    }

    /// Connect without running legacy database setup, SQL migrations, or seeds.
    pub async fn connect(dsn: &str, max_connections: u32) -> Result<Self, StoreError> {
        if !(1..=MAX_CONNECTIONS).contains(&max_connections) {
            return Err(StoreError::InvalidConnectionCount);
        }
        let options = connection_options(dsn)?;
        let pool = PgPoolOptions::new()
            .max_connections(max_connections)
            .acquire_timeout(Duration::from_secs(5))
            .after_connect(|connection, _| {
                Box::pin(async move {
                    sqlx::query("SET default_transaction_read_only = on")
                        .execute(&mut *connection)
                        .await?;
                    sqlx::query("SET search_path = pg_catalog")
                        .execute(&mut *connection)
                        .await?;
                    sqlx::query("SET statement_timeout = '5s'")
                        .execute(&mut *connection)
                        .await?;
                    Ok(())
                })
            })
            .connect_with(options)
            .await
            .map_err(|_| StoreError::Connect)?;
        Ok(Self { pool })
    }

    /// Return an ID-ordered page. The exclusive cursor is the last returned ID.
    pub async fn browse(
        &self,
        view: View,
        before_id: Option<i64>,
        limit: u16,
    ) -> Result<Page, StoreError> {
        if !(1..=MAX_PAGE_SIZE).contains(&limit) {
            return Err(StoreError::InvalidLimit);
        }
        if before_id.is_some_and(|id| id <= 0) {
            return Err(StoreError::InvalidCursor);
        }
        if matches!(view, View::Exploration { task_id } if task_id <= 0) {
            return Err(StoreError::InvalidTask);
        }

        tokio::time::timeout(STATEMENT_TIMEOUT, self.fetch(view, before_id, limit))
            .await
            .map_err(|_| StoreError::Timeout)?
            .map_err(|_| StoreError::Query)
    }

    async fn fetch(
        &self,
        view: View,
        before_id: Option<i64>,
        limit: u16,
    ) -> Result<Page, sqlx::Error> {
        let statement = match view {
            View::Tasks => TASKS,
            View::Assets => ASSETS,
            View::Findings => FINDINGS,
            View::Sessions => SESSIONS,
            View::Approvals => APPROVALS,
            View::TrafficEvidence => TRAFFIC_EVIDENCE,
            View::Exploration { .. } => EXPLORATION,
        };
        let query = sqlx::query(statement)
            .bind(before_id)
            .bind(i64::from(limit) + 1);
        let query = match view {
            View::Exploration { task_id } => query.bind(task_id),
            _ => query,
        };
        let mut records = query.fetch_all(&self.pool).await?;
        let limit = usize::from(limit);
        let next_before = if records.len() > limit {
            records.truncate(limit);
            records.last().map(|row| row.try_get("id")).transpose()?
        } else {
            None
        };

        let rows = match view {
            View::Tasks => Rows::Tasks(decode(records, |row| {
                Ok(TaskSummary {
                    id: row.try_get("id")?,
                    name: row.try_get("name")?,
                    status: row.try_get("status")?,
                    exploration_id: row.try_get("exploration_id")?,
                    archived: row.try_get("archived")?,
                })
            })?),
            View::Assets => Rows::Assets(decode(records, |row| {
                Ok(AssetSummary {
                    id: row.try_get("id")?,
                    kind: row.try_get("kind")?,
                    company_id: row.try_get("company_id")?,
                    label: row.try_get("label")?,
                })
            })?),
            View::Findings => Rows::Findings(decode(records, |row| {
                Ok(FindingSummary {
                    id: row.try_get("id")?,
                    task_id: row.try_get("task_id")?,
                    name: row.try_get("name")?,
                    severity: row.try_get("severity")?,
                    status: row.try_get("status")?,
                })
            })?),
            View::Sessions => Rows::Sessions(decode(records, |row| {
                Ok(SessionSummary {
                    id: row.try_get("id")?,
                    agent_key: row.try_get("agent_key")?,
                    title: row.try_get("title")?,
                })
            })?),
            View::Approvals => Rows::Approvals(decode(records, |row| {
                Ok(ApprovalSummary {
                    id: row.try_get("id")?,
                    status: row.try_get("status")?,
                    task_ref: row.try_get("task_ref")?,
                })
            })?),
            View::TrafficEvidence => Rows::TrafficEvidence(decode(records, |row| {
                Ok(TrafficEvidenceSummary {
                    id: row.try_get("id")?,
                    finding_id: row.try_get("finding_id")?,
                    method: row.try_get("method")?,
                    status: row.try_get("status")?,
                    request_bytes: row.try_get("req_len")?,
                    response_bytes: row.try_get("resp_len")?,
                })
            })?),
            View::Exploration { .. } => Rows::Exploration(decode(records, |row| {
                Ok(ExplorationNodeSummary {
                    id: row.try_get("id")?,
                    kind: row.try_get("kind")?,
                    state: row.try_get("state")?,
                    priority: row.try_get("priority")?,
                })
            })?),
        };

        Ok(Page { rows, next_before })
    }
}

fn connection_options(dsn: &str) -> Result<PgConnectOptions, StoreError> {
    let dsn = dsn.trim();
    let mut url = Url::parse(dsn).map_err(|_| StoreError::InvalidDatabaseUrl)?;
    if !matches!(url.scheme(), "postgres" | "postgresql") || url.fragment().is_some() {
        return Err(StoreError::InvalidDatabaseUrl);
    }
    let host = url.host().ok_or(StoreError::InvalidDatabaseUrl)?;
    let address = match host {
        Host::Ipv4(address) => Some(IpAddr::V4(address)),
        Host::Ipv6(address) => Some(IpAddr::V6(address)),
        Host::Domain(host) => host.parse().ok(),
    };
    if has_unpinned_pg_options() {
        return Err(StoreError::InvalidDatabaseOptions);
    }
    validate_dsn_query(&url)?;

    if url.port().is_none() {
        // Set the port before SQLx looks up credentials in pgpass.
        url.set_port(Some(5432))
            .map_err(|_| StoreError::InvalidDatabaseUrl)?;
    }
    let options =
        PgConnectOptions::from_str(url.as_str()).map_err(|_| StoreError::InvalidDatabaseUrl)?;
    if options.get_socket().is_some() || options.get_host().starts_with('/') {
        return Err(StoreError::InvalidDatabaseUrl);
    }
    if !address.is_some_and(|address| address.is_loopback())
        && !matches!(options.get_ssl_mode(), PgSslMode::VerifyFull)
    {
        return Err(StoreError::InsecureDatabaseTransport);
    }
    Ok(options
        .application_name("arustex-observer")
        .disable_statement_logging())
}

fn has_unpinned_pg_options() -> bool {
    // SQLx inherits both of these and does not expose a way to clear them.
    ["PGHOSTADDR", "PGOPTIONS"]
        .into_iter()
        .any(|name| env::var_os(name).is_some_and(|value| !value.is_empty()))
}

fn validate_dsn_query(url: &Url) -> Result<(), StoreError> {
    let mut seen = [false; 4];
    for (key, _) in url.query_pairs() {
        let index = match key.as_ref() {
            "sslmode" | "ssl-mode" => 0,
            "sslrootcert" | "ssl-root-cert" | "ssl-ca" => 1,
            "sslcert" | "ssl-cert" => 2,
            "sslkey" | "ssl-key" => 3,
            _ => return Err(StoreError::InvalidDatabaseOptions),
        };
        if seen[index] {
            return Err(StoreError::InvalidDatabaseOptions);
        }
        seen[index] = true;
    }
    Ok(())
}

fn decode<T>(
    rows: Vec<PgRow>,
    parse: impl FnMut(PgRow) -> Result<T, sqlx::Error>,
) -> Result<Vec<T>, sqlx::Error> {
    rows.into_iter().map(parse).collect()
}

// Limit free-form labels in SQL so even a single corrupt row cannot produce an
// unbounded IPC response. Every projection uses an explicit column allowlist.
const TASKS: &str =
    "SELECT id, left(name, 128) AS name, left(status, 32) AS status, exploration_id,
    archived_at IS NOT NULL AS archived FROM public.tasks
    WHERE deleted_at IS NULL AND ($1::bigint IS NULL OR id < $1)
    ORDER BY id DESC LIMIT $2";

const ASSETS: &str = "SELECT id, left(type, 32) AS kind, company_id,
    left(coalesce(nullif(domain, ''), nullif(ip, ''), nullif(app_name, '')), 128) AS label
    FROM public.assets WHERE ($1::bigint IS NULL OR id < $1) ORDER BY id DESC LIMIT $2";

const FINDINGS: &str = "SELECT id, task_id,
    left(coalesce(nullif(name, ''), vulnclass), 128) AS name,
    left(severity, 32) AS severity, left(status, 32) AS status
    FROM public.findings WHERE ($1::bigint IS NULL OR id < $1) ORDER BY id DESC LIMIT $2";

const SESSIONS: &str = "SELECT id, left(agent_key, 64) AS agent_key, left(title, 128) AS title
    FROM public.conversations WHERE ($1::bigint IS NULL OR id < $1)
    ORDER BY id DESC LIMIT $2";

const APPROVALS: &str = "SELECT id, left(status, 32) AS status, left(task_id, 64) AS task_ref
    FROM public.intercept_pending WHERE ($1::bigint IS NULL OR id < $1)
    ORDER BY id DESC LIMIT $2";

const TRAFFIC_EVIDENCE: &str = "SELECT binding.id, binding.finding_id,
    left(snapshot.method, 16) AS method, snapshot.status, snapshot.req_len, snapshot.resp_len
    FROM public.finding_traffic_bindings AS binding
    JOIN public.traffic_evidence_snapshots AS snapshot ON snapshot.id = binding.snapshot_id
    WHERE ($1::bigint IS NULL OR binding.id < $1) ORDER BY binding.id DESC LIMIT $2";

const EXPLORATION: &str = "SELECT node.id, left(node.kind, 32) AS kind,
    left(node.state, 32) AS state, node.priority FROM public.exploration_nodes AS node
    JOIN public.tasks AS task ON task.exploration_id = node.exploration_id
    WHERE ($1::bigint IS NULL OR node.id < $1) AND task.id = $3 AND task.deleted_at IS NULL
    ORDER BY node.id DESC LIMIT $2";
