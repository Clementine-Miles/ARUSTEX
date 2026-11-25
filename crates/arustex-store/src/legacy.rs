//! Allowlisted import from a legacy (Go-era) PostgreSQL database.
//!
//! Only reviewable business data is migrated: organizations, asset identities,
//! task metadata, exploration graph structure, and finding summaries. The importer
//! never reads or copies agent definitions, prompts, executable workflows, tool or
//! MCP configuration, activity transcripts, intercepted traffic, evidence payloads,
//! notification credentials, settings, or model-provider secrets.
//!
//! Exploration node payloads are replaced by neutral titles because legacy payloads
//! carry model-authored action plans and captured output. Finding evidence and
//! detailed reports are dropped; summaries are redacted.

use std::collections::BTreeMap;

use arustex_core::domain::finding::{FindingStatus, MAX_FINDING_SUMMARY, Severity};
use arustex_core::domain::record::redact;
use arustex_core::{CoreError, CoreResult};
use serde::Serialize;
use sqlx::postgres::{PgPool, PgPoolOptions};
use sqlx::{Row, Transaction};

use crate::backup::reset_identity;
use crate::postgres::{Store, audit, db_err};

/// Legacy tables that are deliberately not imported, reported with their row counts.
pub const EXCLUDED_LEGACY_TABLES: &[&str] = &[
    "activity",
    "main_sessions",
    "settings",
    "llm_profile_health",
    "task_archives",
    "task_templates",
    "task_constraints",
    "task_llm_profiles",
    "agents",
    "agent_prompts",
    "agent_prompt_vars",
    "mcp_servers",
    "mcp_tools_cache",
    "agent_visibility",
    "agent_skill_visibility",
    "skill_usage",
    "tool_usage",
    "tools",
    "conversations",
    "conversation_activities",
    "agent_triggers",
    "scheduler_state",
    "intercept_rules",
    "intercept_pending",
    "finding_retests",
    "traffic_evidence_snapshots",
    "finding_traffic_bindings",
    "server_logs",
    "side_question_sessions",
    "side_question_requests",
    "asset_intercept_rules",
    "task_intercept_rules",
    "notification_channels",
    "notification_events",
    "notification_deliveries",
];

/// A model-provider profile found in the legacy database, without its credential.
/// Profiles are configuration, not data: the operator copies them into the TOML file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct LegacyProfile {
    pub name: String,
    pub format: String,
    pub base_url: Option<String>,
    pub model: String,
    pub had_api_key: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct LegacyImportReport {
    pub dry_run: bool,
    pub organizations: u64,
    pub assets: u64,
    pub containment_edges: u64,
    pub tasks: u64,
    pub skipped_deleted_tasks: u64,
    pub explorations: u64,
    pub nodes: u64,
    pub edges: u64,
    pub skipped_edges: u64,
    pub anchors: u64,
    pub findings: u64,
    pub redacted_findings: u64,
    pub profiles: Vec<LegacyProfile>,
    pub excluded: Vec<(String, u64)>,
}

/// Maps legacy task states onto the new state machine. Running legacy work is never
/// resumed automatically; it becomes paused so an operator decides.
fn map_task_status(status: &str, paused: bool) -> &'static str {
    match status {
        "done" => "completed",
        "failed" => "failed",
        "timeout" => "timed_out",
        "running" | "paused" => "paused",
        _ if paused => "paused",
        _ => "created",
    }
}

fn map_severity(value: &str) -> Severity {
    value
        .trim()
        .to_ascii_lowercase()
        .parse()
        .unwrap_or(Severity::Info)
}

/// Natural identity of a legacy asset, mirroring the legacy uniqueness rules.
fn legacy_asset_identity(row: &sqlx::postgres::PgRow) -> CoreResult<Option<(String, String)>> {
    let kind: String = row.try_get("type").map_err(db_err)?;
    let get = |c: &str| -> CoreResult<Option<String>> { row.try_get(c).map_err(db_err) };
    let label = match kind.as_str() {
        "root_domain" | "subdomain" => get("domain")?,
        "ip" => get("ip")?,
        "app" => get("bundle_id")?.or(get("app_name")?),
        "service" => match get("url")? {
            Some(url) => Some(url),
            None => {
                let host = get("domain")?.or(get("ip")?).unwrap_or_default();
                let port: Option<i32> = row.try_get("port").map_err(db_err)?;
                let name = get("service_name")?.unwrap_or_default();
                Some(format!("{host}:{}/{name}", port.unwrap_or(0)))
            }
        },
        "endpoint" => {
            let url = get("url")?.unwrap_or_default();
            let method = get("method")?.unwrap_or_default();
            Some(format!("{method} {url}").trim().to_owned())
        }
        _ => None,
    };
    Ok(label.filter(|l| !l.trim().is_empty()).map(|l| {
        (
            format!("{kind}:{}", l.trim().to_ascii_lowercase()),
            l.trim().chars().take(1024).collect(),
        )
    }))
}

async fn table_exists(pool: &PgPool, table: &str) -> CoreResult<bool> {
    sqlx::query_scalar("SELECT to_regclass($1) IS NOT NULL")
        .bind(format!("public.{table}"))
        .fetch_one(pool)
        .await
        .map_err(db_err)
}

async fn ensure_target_empty(tx: &mut Transaction<'static, sqlx::Postgres>) -> CoreResult<()> {
    for table in ["tasks", "explorations", "assets", "findings"] {
        let any: bool = sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
            "SELECT EXISTS (SELECT 1 FROM {table})"
        )))
        .fetch_one(&mut **tx)
        .await
        .map_err(db_err)?;
        if any {
            return Err(CoreError::Conflict(format!(
                "legacy import needs an empty target, but {table} already has rows"
            )));
        }
    }
    Ok(())
}

impl Store {
    /// Connects read-only to a legacy database and imports allowlisted data in one
    /// transaction. With `dry_run` the transaction is rolled back after counting.
    pub async fn import_legacy(
        &self,
        legacy_url: &str,
        dry_run: bool,
        actor: &str,
    ) -> CoreResult<LegacyImportReport> {
        let legacy = PgPoolOptions::new()
            .max_connections(2)
            .after_connect(|conn, _| {
                Box::pin(async move {
                    sqlx::query("SET SESSION CHARACTERISTICS AS TRANSACTION READ ONLY")
                        .execute(conn)
                        .await?;
                    Ok(())
                })
            })
            .connect(legacy_url)
            .await
            .map_err(db_err)?;
        let result = self.import_from_pool(&legacy, dry_run, actor).await;
        legacy.close().await;
        result
    }

    pub async fn import_from_pool(
        &self,
        legacy: &PgPool,
        dry_run: bool,
        actor: &str,
    ) -> CoreResult<LegacyImportReport> {
        for required in [
            "companies",
            "assets",
            "explorations",
            "exploration_nodes",
            "exploration_edges",
            "tasks",
            "findings",
        ] {
            if !table_exists(legacy, required).await? {
                return Err(CoreError::invalid(format!(
                    "legacy database has no {required} table"
                )));
            }
        }
        let mut report = LegacyImportReport {
            dry_run,
            ..Default::default()
        };
        let mut tx = self.begin().await?;
        ensure_target_empty(&mut tx).await?;

        // Organizations.
        let mut org_map: BTreeMap<i64, i64> = BTreeMap::new();
        let rows = sqlx::query("SELECT id, name, nkey, created_at FROM companies ORDER BY id")
            .fetch_all(legacy)
            .await
            .map_err(db_err)?;
        for row in &rows {
            let legacy_id: i64 = row.try_get("id").map_err(db_err)?;
            let name: String = row.try_get("name").map_err(db_err)?;
            let nkey: String = row.try_get("nkey").map_err(db_err)?;
            let label = if name.trim().is_empty() {
                nkey.clone()
            } else {
                name
            };
            let id: i64 = sqlx::query_scalar(
                "INSERT INTO assets (kind, identity_key, label, origin, source, locator, legacy_id, created_at)
                 VALUES ('organization', $1, $2, 'supplied', 'legacy:companies', $3, $4, $5)
                 ON CONFLICT (kind, identity_key) DO UPDATE SET label = EXCLUDED.label RETURNING id",
            )
            .bind(nkey.trim().to_ascii_lowercase().chars().take(1024).collect::<String>())
            .bind(label.trim().chars().take(1024).collect::<String>())
            .bind(format!("companies/{legacy_id}"))
            .bind(legacy_id)
            .bind(row.try_get::<chrono::DateTime<chrono::Utc>, _>("created_at").map_err(db_err)?)
            .fetch_one(&mut *tx)
            .await
            .map_err(db_err)?;
            org_map.insert(legacy_id, id);
            report.organizations += 1;
        }

        // Asset identities (allowlisted columns only; auth, params, extra and open
        // ports are never read).
        let mut asset_map: BTreeMap<i64, i64> = BTreeMap::new();
        let rows = sqlx::query(
            "SELECT id, type, company_id, domain, root_domain, ip, port, bundle_id, app_name, url,
                    service_type, service_name, method, created_at
             FROM assets ORDER BY id",
        )
        .fetch_all(legacy)
        .await
        .map_err(db_err)?;
        for row in &rows {
            let legacy_id: i64 = row.try_get("id").map_err(db_err)?;
            let Some((identity, label)) = legacy_asset_identity(row)? else {
                continue;
            };
            let legacy_type: String = row.try_get("type").map_err(db_err)?;
            let mut attributes = serde_json::Map::new();
            attributes.insert("legacy_type".into(), legacy_type.clone().into());
            if let Some(root) = row
                .try_get::<Option<String>, _>("root_domain")
                .map_err(db_err)?
            {
                attributes.insert("root_domain".into(), root.into());
            }
            if let Some(port) = row.try_get::<Option<i32>, _>("port").map_err(db_err)? {
                attributes.insert("port".into(), port.into());
            }
            let id: i64 = sqlx::query_scalar(
                "INSERT INTO assets (kind, identity_key, label, attributes, origin, source, locator, legacy_id, created_at)
                 VALUES ('reference', $1, $2, $3, 'supplied', 'legacy:assets', $4, $5, $6)
                 ON CONFLICT (kind, identity_key) DO UPDATE SET attributes = assets.attributes || EXCLUDED.attributes
                 RETURNING id",
            )
            .bind(&identity)
            .bind(&label)
            .bind(serde_json::Value::Object(attributes))
            .bind(format!("assets/{legacy_id}"))
            .bind(legacy_id)
            .bind(row.try_get::<chrono::DateTime<chrono::Utc>, _>("created_at").map_err(db_err)?)
            .fetch_one(&mut *tx)
            .await
            .map_err(db_err)?;
            asset_map.insert(legacy_id, id);
            report.assets += 1;
            if let Some(company) = row
                .try_get::<Option<i64>, _>("company_id")
                .map_err(db_err)?
                && let Some(org) = org_map.get(&company)
            {
                let inserted = sqlx::query(
                    "INSERT INTO asset_edges (src_id, dst_id, rel) VALUES ($1, $2, 'contains') ON CONFLICT DO NOTHING",
                )
                .bind(org)
                .bind(id)
                .execute(&mut *tx)
                .await
                .map_err(db_err)?
                .rows_affected();
                report.containment_edges += inserted;
            }
        }

        // Tasks and their explorations (deleted tasks are skipped).
        report.skipped_deleted_tasks = to_u64_count(
            sqlx::query_scalar("SELECT count(*) FROM tasks WHERE deleted_at IS NOT NULL")
                .fetch_one(legacy)
                .await
                .map_err(db_err)?,
        );
        let tasks = sqlx::query(
            "SELECT t.id, t.name, t.description, t.goal, t.exploration_id, t.status, t.paused,
                    t.timeout_seconds, t.created_at, t.updated_at, t.completed_at,
                    e.status AS exploration_status, e.created_at AS exploration_created_at
             FROM tasks t JOIN explorations e ON e.id = t.exploration_id
             WHERE t.deleted_at IS NULL ORDER BY t.id",
        )
        .fetch_all(legacy)
        .await
        .map_err(db_err)?;
        let mut explorations: Vec<i64> = Vec::new();
        for row in &tasks {
            let exploration: i64 = row.try_get("exploration_id").map_err(db_err)?;
            let goal: String = row.try_get("goal").map_err(db_err)?;
            let objective = if goal.trim().is_empty() {
                "legacy task".to_owned()
            } else {
                goal.chars().take(8192).collect()
            };
            sqlx::query(
                "INSERT INTO explorations (id, objective, status, created_at) VALUES ($1, $2, $3, $4)",
            )
            .bind(exploration)
            .bind(&objective)
            .bind(row.try_get::<String, _>("exploration_status").map_err(db_err)?)
            .bind(row.try_get::<chrono::DateTime<chrono::Utc>, _>("exploration_created_at").map_err(db_err)?)
            .execute(&mut *tx)
            .await
            .map_err(db_err)?;
            explorations.push(exploration);
            report.explorations += 1;
            let legacy_id: i64 = row.try_get("id").map_err(db_err)?;
            let status: String = row.try_get("status").map_err(db_err)?;
            let paused: bool = row.try_get("paused").map_err(db_err)?;
            let description: String = row.try_get("description").map_err(db_err)?;
            let timeout: i32 = row.try_get("timeout_seconds").map_err(db_err)?;
            sqlx::query(
                "INSERT INTO tasks (id, name, objective, description, status, exploration_id, timeout_seconds,
                                    legacy_id, created_at, completed_at)
                 VALUES ($1, $2, $3, $4, $5, $6, $7, $1, $8, $9)",
            )
            .bind(legacy_id)
            .bind(row.try_get::<String, _>("name").map_err(db_err)?.chars().take(200).collect::<String>())
            .bind(&objective)
            .bind(description.chars().take(8192).collect::<String>())
            .bind(map_task_status(&status, paused))
            .bind(exploration)
            .bind(timeout.max(0))
            .bind(row.try_get::<chrono::DateTime<chrono::Utc>, _>("created_at").map_err(db_err)?)
            .bind(row.try_get::<Option<chrono::DateTime<chrono::Utc>>, _>("completed_at").map_err(db_err)?)
            .execute(&mut *tx)
            .await
            .map_err(db_err)?;
            report.tasks += 1;
        }

        // Exploration graph structure. Payloads are not copied.
        let nodes = sqlx::query(
            "SELECT id, exploration_id, kind, state, priority, created_at, completed_at
             FROM exploration_nodes WHERE exploration_id = ANY($1) ORDER BY id",
        )
        .bind(&explorations)
        .fetch_all(legacy)
        .await
        .map_err(db_err)?;
        let mut node_ids: Vec<i64> = Vec::with_capacity(nodes.len());
        for row in &nodes {
            let id: i64 = row.try_get("id").map_err(db_err)?;
            let kind: String = row.try_get("kind").map_err(db_err)?;
            sqlx::query(
                "INSERT INTO exploration_nodes (id, exploration_id, kind, state, priority, title, payload, origin,
                                                created_at, completed_at)
                 VALUES ($1, $2, $3, $4, $5, $6, '{\"legacy\": true}', 'legacy', $7, $8)",
            )
            .bind(id)
            .bind(row.try_get::<i64, _>("exploration_id").map_err(db_err)?)
            .bind(&kind)
            .bind(row.try_get::<String, _>("state").map_err(db_err)?)
            .bind(row.try_get::<i32, _>("priority").map_err(db_err)?)
            .bind(format!("legacy {kind} #{id}"))
            .bind(row.try_get::<chrono::DateTime<chrono::Utc>, _>("created_at").map_err(db_err)?)
            .bind(row.try_get::<Option<chrono::DateTime<chrono::Utc>>, _>("completed_at").map_err(db_err)?)
            .execute(&mut *tx)
            .await
            .map_err(db_err)?;
            node_ids.push(id);
            report.nodes += 1;
        }
        let edges = sqlx::query(
            "SELECT exploration_id, src_id, dst_id, rel, created_at FROM exploration_edges
             WHERE exploration_id = ANY($1) ORDER BY exploration_id, src_id, rel, dst_id",
        )
        .bind(&explorations)
        .fetch_all(legacy)
        .await
        .map_err(db_err)?;
        for row in &edges {
            // A legacy graph may violate a new invariant (e.g. two spawn parents);
            // such edges are skipped and counted rather than aborting the import.
            sqlx::query("SAVEPOINT legacy_edge")
                .execute(&mut *tx)
                .await
                .map_err(db_err)?;
            let inserted = sqlx::query(
                "INSERT INTO exploration_edges (exploration_id, src_id, dst_id, rel, created_at)
                 VALUES ($1, $2, $3, $4, $5)",
            )
            .bind(row.try_get::<i64, _>("exploration_id").map_err(db_err)?)
            .bind(row.try_get::<i64, _>("src_id").map_err(db_err)?)
            .bind(row.try_get::<i64, _>("dst_id").map_err(db_err)?)
            .bind(row.try_get::<String, _>("rel").map_err(db_err)?)
            .bind(
                row.try_get::<chrono::DateTime<chrono::Utc>, _>("created_at")
                    .map_err(db_err)?,
            )
            .execute(&mut *tx)
            .await;
            match inserted {
                Ok(_) => {
                    sqlx::query("RELEASE SAVEPOINT legacy_edge")
                        .execute(&mut *tx)
                        .await
                        .map_err(db_err)?;
                    report.edges += 1;
                }
                Err(_) => {
                    sqlx::query("ROLLBACK TO SAVEPOINT legacy_edge")
                        .execute(&mut *tx)
                        .await
                        .map_err(db_err)?;
                    report.skipped_edges += 1;
                }
            }
        }
        if table_exists(legacy, "exploration_anchors").await? {
            let anchors: Vec<(i64, i64)> = sqlx::query_as(
                "SELECT node_id, asset_id FROM exploration_anchors WHERE node_id = ANY($1) ORDER BY 1, 2",
            )
            .bind(&node_ids)
            .fetch_all(legacy)
            .await
            .map_err(db_err)?;
            for (node, asset) in anchors {
                if let Some(mapped) = asset_map.get(&asset) {
                    sqlx::query("INSERT INTO node_asset_anchors (node_id, asset_id) VALUES ($1, $2) ON CONFLICT DO NOTHING")
                        .bind(node)
                        .bind(mapped)
                        .execute(&mut *tx)
                        .await
                        .map_err(db_err)?;
                    report.anchors += 1;
                }
            }
        }

        // Finding summaries (evidence and detailed reports are never read).
        let imported_tasks: Vec<i64> = tasks
            .iter()
            .map(|r| r.try_get::<i64, _>("id"))
            .collect::<Result<_, _>>()
            .map_err(db_err)?;
        let findings = sqlx::query(
            "SELECT id, task_id, node_id, vulnclass, name, severity, summary, asset_ids, status, created_at
             FROM findings ORDER BY id",
        )
        .fetch_all(legacy)
        .await
        .map_err(db_err)?;
        for row in &findings {
            let id: i64 = row.try_get("id").map_err(db_err)?;
            let task: Option<i64> = row
                .try_get::<Option<i64>, _>("task_id")
                .map_err(db_err)?
                .filter(|t| imported_tasks.contains(t));
            let node: Option<i64> = row
                .try_get::<Option<i64>, _>("node_id")
                .map_err(db_err)?
                .filter(|n| node_ids.binary_search(n).is_ok());
            let class: String = row.try_get("vulnclass").map_err(db_err)?;
            let name: String = row.try_get("name").map_err(db_err)?;
            let title = [name.trim(), class.trim()]
                .into_iter()
                .find(|s| !s.is_empty())
                .unwrap_or("legacy finding");
            let (title, t_red) = redact(title);
            let summary: String = row.try_get("summary").map_err(db_err)?;
            let (summary, s_red) = redact(&summary);
            if t_red || s_red {
                report.redacted_findings += 1;
            }
            let status =
                FindingStatus::from_legacy(&row.try_get::<String, _>("status").map_err(db_err)?);
            sqlx::query(
                "INSERT INTO findings (id, task_id, node_id, title, category, severity, status, summary,
                                       origin, source, locator, legacy_id, created_at)
                 VALUES ($1, $2, $3, $4, $5, $6, $7, $8, 'derived', 'legacy:findings', $9, $1, $10)",
            )
            .bind(id)
            .bind(task)
            .bind(node)
            .bind(title.chars().take(512).collect::<String>())
            .bind(class.chars().take(200).collect::<String>())
            .bind(map_severity(&row.try_get::<String, _>("severity").map_err(db_err)?).as_str())
            .bind(status.as_str())
            .bind(truncate_bytes(&summary, MAX_FINDING_SUMMARY))
            .bind(format!("findings/{id}"))
            .bind(row.try_get::<chrono::DateTime<chrono::Utc>, _>("created_at").map_err(db_err)?)
            .execute(&mut *tx)
            .await
            .map_err(db_err)?;
            let assets: serde_json::Value = row.try_get("asset_ids").map_err(db_err)?;
            for legacy_asset in assets
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(serde_json::Value::as_i64)
            {
                if let Some(mapped) = asset_map.get(&legacy_asset) {
                    sqlx::query("INSERT INTO finding_assets (finding_id, asset_id) VALUES ($1, $2) ON CONFLICT DO NOTHING")
                        .bind(id)
                        .bind(mapped)
                        .execute(&mut *tx)
                        .await
                        .map_err(db_err)?;
                }
            }
            report.findings += 1;
        }

        // Model profiles are listed for the operator; credentials are never read.
        if table_exists(legacy, "llm_profiles").await? {
            let rows = sqlx::query(
                "SELECT name, format, base_url, model, (api_key IS NOT NULL AND api_key <> '') AS had_key
                 FROM llm_profiles ORDER BY id",
            )
            .fetch_all(legacy)
            .await
            .map_err(db_err)?;
            for row in rows {
                report.profiles.push(LegacyProfile {
                    name: row.try_get("name").map_err(db_err)?,
                    format: row.try_get("format").map_err(db_err)?,
                    base_url: row.try_get("base_url").map_err(db_err)?,
                    model: row.try_get("model").map_err(db_err)?,
                    had_api_key: row.try_get("had_key").map_err(db_err)?,
                });
            }
        }
        for table in EXCLUDED_LEGACY_TABLES {
            if table_exists(legacy, table).await? {
                let n: i64 = sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
                    "SELECT count(*) FROM {table}"
                )))
                .fetch_one(legacy)
                .await
                .map_err(db_err)?;
                report.excluded.push(((*table).to_owned(), to_u64_count(n)));
            }
        }

        for table in [
            "explorations",
            "exploration_nodes",
            "tasks",
            "assets",
            "findings",
        ] {
            reset_identity(&mut tx, table).await?;
        }
        if dry_run {
            tx.rollback().await.map_err(db_err)?;
        } else {
            audit(
                &mut tx,
                actor,
                "legacy.import",
                "database",
                serde_json::to_value(&report).unwrap_or_default(),
            )
            .await?;
            self.commit(tx, Vec::new()).await?;
        }
        Ok(report)
    }
}

fn to_u64_count(n: i64) -> u64 {
    u64::try_from(n).unwrap_or(0)
}

fn truncate_bytes(text: &str, max: usize) -> String {
    if text.len() <= max {
        return text.to_owned();
    }
    let mut end = max;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    text[..end].to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn task_states_map_conservatively() {
        assert_eq!(map_task_status("running", false), "paused");
        assert_eq!(map_task_status("done", false), "completed");
        assert_eq!(map_task_status("timeout", false), "timed_out");
        assert_eq!(map_task_status("created", true), "paused");
        assert_eq!(map_task_status("created", false), "created");
    }

    #[test]
    fn severities_default_to_info() {
        assert_eq!(map_severity("HIGH"), Severity::High);
        assert_eq!(map_severity(""), Severity::Info);
        assert_eq!(map_severity("urgent"), Severity::Info);
    }

    #[test]
    fn truncation_respects_char_boundaries() {
        assert_eq!(truncate_bytes("héllo", 2), "h");
        assert_eq!(truncate_bytes("abc", 10), "abc");
    }
}
