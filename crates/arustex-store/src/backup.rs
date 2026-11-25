//! Logical backup and restore.
//!
//! A backup is a directory with one JSON Lines file per table plus `manifest.json`
//! holding row counts, per-file SHA-256 and the schema version. Export runs in one
//! repeatable-read snapshot; restore runs in one transaction into a freshly migrated,
//! empty database and refuses to overwrite existing data.

use std::path::{Path, PathBuf};

use arustex_core::{CoreError, CoreResult};
use futures::TryStreamExt;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, BufWriter};

use crate::migration::MIGRATOR;
use crate::postgres::{Store, db_err};

/// Tables in foreign-key dependency order (parents first).
pub const BACKUP_TABLES: &[&str] = &[
    "explorations",
    "datasets",
    "records",
    "tasks",
    "task_datasets",
    "exploration_nodes",
    "exploration_edges",
    "assets",
    "asset_edges",
    "node_record_anchors",
    "node_asset_anchors",
    "findings",
    "finding_records",
    "finding_assets",
    "review_notes",
    "events",
    "audit_log",
    "prompt_templates",
    "work_items",
    "sessions",
    "transcript_entries",
    "llm_usage",
    "approvals",
    "review_results",
    "triggers",
    "blobs",
    "blob_chunks",
    "attachments",
    "reports",
    "notifications",
];

const RESTORE_BATCH: usize = 500;
pub const MANIFEST_FORMAT: &str = "arustex.backup.v1";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TableManifest {
    pub table: String,
    pub file: String,
    pub rows: u64,
    pub sha256: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BackupManifest {
    pub format: String,
    pub schema_version: i64,
    pub created_at: chrono::DateTime<chrono::Utc>,
    pub tables: Vec<TableManifest>,
}

fn io_err(path: &Path, e: std::io::Error) -> CoreError {
    CoreError::Storage(format!("{}: {e}", path.display()))
}

fn latest_schema_version() -> i64 {
    MIGRATOR.iter().map(|m| m.version).max().unwrap_or(0)
}

async fn insertable_columns(
    tx: &mut sqlx::Transaction<'static, sqlx::Postgres>,
    table: &str,
) -> CoreResult<Vec<String>> {
    let columns: Vec<String> = sqlx::query_scalar(
        "SELECT column_name::text FROM information_schema.columns
         WHERE table_schema = current_schema() AND table_name = $1 AND is_generated = 'NEVER'
         ORDER BY ordinal_position",
    )
    .bind(table)
    .fetch_all(&mut **tx)
    .await
    .map_err(db_err)?;
    if columns.is_empty() {
        return Err(CoreError::Integrity(format!("table {table} not found")));
    }
    Ok(columns)
}

fn quote_ident(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}

impl Store {
    /// Writes a logical backup into `dir`, which must not exist or be empty.
    pub async fn backup_to(&self, dir: &Path) -> CoreResult<BackupManifest> {
        tokio::fs::create_dir_all(dir)
            .await
            .map_err(|e| io_err(dir, e))?;
        let mut entries = tokio::fs::read_dir(dir).await.map_err(|e| io_err(dir, e))?;
        if entries
            .next_entry()
            .await
            .map_err(|e| io_err(dir, e))?
            .is_some()
        {
            return Err(CoreError::Conflict(format!(
                "backup directory {} is not empty",
                dir.display()
            )));
        }
        let mut tx = self.begin().await?;
        sqlx::query("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ, READ ONLY")
            .execute(&mut *tx)
            .await
            .map_err(db_err)?;
        let mut tables = Vec::new();
        for table in BACKUP_TABLES {
            let columns = insertable_columns(&mut tx, table).await?;
            let select = columns
                .iter()
                .map(|c| quote_ident(c))
                .collect::<Vec<_>>()
                .join(", ");
            let file = format!("{table}.jsonl");
            let path = dir.join(&file);
            let handle = tokio::fs::File::create(&path)
                .await
                .map_err(|e| io_err(&path, e))?;
            let mut out = BufWriter::new(handle);
            let mut hasher = Sha256::new();
            let mut rows = 0u64;
            let sql = format!(
                "SELECT row_to_json(t)::text FROM (SELECT {select} FROM {table} ORDER BY 1, 2) t"
            );
            {
                let mut stream =
                    sqlx::query_scalar::<_, String>(sqlx::AssertSqlSafe(sql)).fetch(&mut *tx);
                while let Some(line) = stream.try_next().await.map_err(db_err)? {
                    hasher.update(line.as_bytes());
                    hasher.update(b"\n");
                    out.write_all(line.as_bytes())
                        .await
                        .map_err(|e| io_err(&path, e))?;
                    out.write_all(b"\n").await.map_err(|e| io_err(&path, e))?;
                    rows += 1;
                }
            }
            out.flush().await.map_err(|e| io_err(&path, e))?;
            out.into_inner()
                .sync_all()
                .await
                .map_err(|e| io_err(&path, e))?;
            tables.push(TableManifest {
                table: (*table).to_owned(),
                file,
                rows,
                sha256: hex::encode(hasher.finalize()),
            });
        }
        tx.rollback().await.map_err(db_err)?;
        let manifest = BackupManifest {
            format: MANIFEST_FORMAT.to_owned(),
            schema_version: latest_schema_version(),
            created_at: chrono::Utc::now(),
            tables,
        };
        let path = dir.join("manifest.json");
        let body =
            serde_json::to_vec_pretty(&manifest).map_err(|e| CoreError::Internal(e.to_string()))?;
        tokio::fs::write(&path, body)
            .await
            .map_err(|e| io_err(&path, e))?;
        Ok(manifest)
    }

    /// Verifies a backup directory against its manifest without touching the database.
    pub async fn verify_backup(dir: &Path) -> CoreResult<BackupManifest> {
        let manifest = read_manifest(dir).await?;
        for table in &manifest.tables {
            let path = backup_file(dir, table)?;
            let data = tokio::fs::read(&path).await.map_err(|e| io_err(&path, e))?;
            if hex::encode(Sha256::digest(&data)) != table.sha256 {
                return Err(CoreError::Integrity(format!(
                    "{} does not match the manifest",
                    table.file
                )));
            }
            let lines = data.iter().filter(|b| **b == b'\n').count() as u64;
            if lines != table.rows {
                return Err(CoreError::Integrity(format!(
                    "{} has {lines} rows, manifest says {}",
                    table.file, table.rows
                )));
            }
        }
        Ok(manifest)
    }

    /// Restores a verified backup into this (migrated, empty) database.
    pub async fn restore_from(&self, dir: &Path) -> CoreResult<BackupManifest> {
        let manifest = Self::verify_backup(dir).await?;
        if manifest.schema_version != latest_schema_version() {
            return Err(CoreError::Invalid(format!(
                "backup schema version {} differs from this build ({})",
                manifest.schema_version,
                latest_schema_version()
            )));
        }
        let mut tx = self.begin().await?;
        for table in BACKUP_TABLES {
            let non_empty: bool = sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
                "SELECT EXISTS (SELECT 1 FROM {table})"
            )))
            .fetch_one(&mut *tx)
            .await
            .map_err(db_err)?;
            if non_empty {
                return Err(CoreError::Conflict(format!(
                    "refusing to restore: table {table} already contains data"
                )));
            }
        }
        for table in BACKUP_TABLES {
            let Some(entry) = manifest.tables.iter().find(|t| t.table == *table) else {
                return Err(CoreError::Integrity(format!(
                    "manifest has no entry for {table}"
                )));
            };
            let columns = insertable_columns(&mut tx, table).await?;
            let list = columns
                .iter()
                .map(|c| quote_ident(c))
                .collect::<Vec<_>>()
                .join(", ");
            let insert = format!(
                "INSERT INTO {table} ({list}) OVERRIDING SYSTEM VALUE
                 SELECT {list} FROM jsonb_populate_recordset(NULL::{table}, $1::jsonb)"
            );
            let path = backup_file(dir, entry)?;
            let file = tokio::fs::File::open(&path)
                .await
                .map_err(|e| io_err(&path, e))?;
            let mut lines = BufReader::new(file).lines();
            let mut batch: Vec<serde_json::Value> = Vec::with_capacity(RESTORE_BATCH);
            loop {
                let line = lines.next_line().await.map_err(|e| io_err(&path, e))?;
                if let Some(line) = &line {
                    let value: serde_json::Value = serde_json::from_str(line)
                        .map_err(|e| CoreError::Integrity(format!("{}: {e}", entry.file)))?;
                    batch.push(value);
                }
                if batch.len() >= RESTORE_BATCH || (line.is_none() && !batch.is_empty()) {
                    sqlx::query(sqlx::AssertSqlSafe(insert.clone()))
                        .bind(serde_json::Value::Array(std::mem::take(&mut batch)))
                        .execute(&mut *tx)
                        .await
                        .map_err(db_err)?;
                }
                if line.is_none() {
                    break;
                }
            }
            reset_identity(&mut tx, table).await?;
        }
        self.commit(tx, Vec::new()).await?;
        Ok(manifest)
    }
}

/// Moves identity sequences past the highest stored value, e.g. after importing
/// rows with preserved ids.
pub(crate) async fn reset_identity(
    tx: &mut sqlx::Transaction<'static, sqlx::Postgres>,
    table: &str,
) -> CoreResult<()> {
    let identity: Vec<String> = sqlx::query_scalar(
        "SELECT column_name::text FROM information_schema.columns
         WHERE table_schema = current_schema() AND table_name = $1 AND is_identity = 'YES'",
    )
    .bind(table)
    .fetch_all(&mut **tx)
    .await
    .map_err(db_err)?;
    for column in identity {
        let col = quote_ident(&column);
        let sql = format!(
            "SELECT setval(pg_get_serial_sequence('{table}', '{column}'),
                           COALESCE((SELECT max({col}) FROM {table}), 0) + 1, false)"
        );
        sqlx::query(sqlx::AssertSqlSafe(sql))
            .execute(&mut **tx)
            .await
            .map_err(db_err)?;
    }
    Ok(())
}

impl Store {
    /// Resets identity sequences for every table (after a legacy import).
    pub async fn reset_all_identities(&self) -> CoreResult<()> {
        let mut tx = self.begin().await?;
        for table in BACKUP_TABLES {
            reset_identity(&mut tx, table).await?;
        }
        self.commit(tx, Vec::new()).await
    }
}

async fn read_manifest(dir: &Path) -> CoreResult<BackupManifest> {
    let path = dir.join("manifest.json");
    let data = tokio::fs::read(&path).await.map_err(|e| io_err(&path, e))?;
    let manifest: BackupManifest = serde_json::from_slice(&data)
        .map_err(|e| CoreError::Integrity(format!("manifest.json: {e}")))?;
    if manifest.format != MANIFEST_FORMAT {
        return Err(CoreError::Invalid(format!(
            "unsupported backup format {}",
            manifest.format
        )));
    }
    Ok(manifest)
}

/// Resolves a manifest file name strictly inside the backup directory.
fn backup_file(dir: &Path, entry: &TableManifest) -> CoreResult<PathBuf> {
    let expected = format!("{}.jsonl", entry.table);
    if entry.file != expected || !BACKUP_TABLES.contains(&entry.table.as_str()) {
        return Err(CoreError::Integrity(format!(
            "unexpected backup file {}",
            entry.file
        )));
    }
    Ok(dir.join(expected))
}
