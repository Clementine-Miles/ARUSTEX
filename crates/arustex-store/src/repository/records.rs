use arustex_core::domain::exploration::ExplorationSnapshot;
use arustex_core::domain::record::{Dataset, ImportedRecord, IngestLimits, RecordInput, redact};
use arustex_core::domain::{ContentHash, DatasetId, Provenance, RecordId, TaskId};
use arustex_core::events::DomainEvent;
use arustex_core::ports::{RecordScope, RecordSource, RecordSummary, ScopeStats};
use arustex_core::{CoreError, CoreResult, Page, Paged};
use async_trait::async_trait;
use sqlx::Row;
use sqlx::postgres::PgRow;

use crate::postgres::{Store, audit, db_err, insert_event, to_i32, to_u32, to_u64};

fn dataset_from_row(row: &PgRow) -> CoreResult<Dataset> {
    Ok(Dataset {
        id: DatasetId(row.try_get("id").map_err(db_err)?),
        name: row.try_get("name").map_err(db_err)?,
        description: row.try_get("description").map_err(db_err)?,
        source: row.try_get("source").map_err(db_err)?,
        record_count: to_u64(row.try_get("record_count").map_err(db_err)?),
        retention_days: to_u32(row.try_get("retention_days").map_err(db_err)?),
        created_at: row.try_get("created_at").map_err(db_err)?,
    })
}

pub(crate) fn record_from_row(row: &PgRow) -> CoreResult<ImportedRecord> {
    let origin: String = row.try_get("origin").map_err(db_err)?;
    let hash: String = row.try_get("content_hash").map_err(db_err)?;
    Ok(ImportedRecord {
        id: RecordId(row.try_get("id").map_err(db_err)?),
        dataset_id: DatasetId(row.try_get("dataset_id").map_err(db_err)?),
        external_id: row.try_get("external_id").map_err(db_err)?,
        title: row.try_get("title").map_err(db_err)?,
        content_type: row.try_get("content_type").map_err(db_err)?,
        body: row.try_get("body").map_err(db_err)?,
        fields: row.try_get("fields").map_err(db_err)?,
        content_hash: ContentHash::parse(&hash)?,
        redacted: row.try_get("redacted").map_err(db_err)?,
        provenance: Provenance {
            origin: origin.parse()?,
            source: row.try_get("source").map_err(db_err)?,
            locator: row.try_get("locator").map_err(db_err)?,
            session_id: None,
        },
        imported_at: row.try_get("imported_at").map_err(db_err)?,
    })
}

fn summary_from_row(row: &PgRow) -> CoreResult<RecordSummary> {
    Ok(RecordSummary {
        id: RecordId(row.try_get("id").map_err(db_err)?),
        dataset_id: DatasetId(row.try_get("dataset_id").map_err(db_err)?),
        external_id: row.try_get("external_id").map_err(db_err)?,
        title: row.try_get("title").map_err(db_err)?,
        content_type: row.try_get("content_type").map_err(db_err)?,
        body_bytes: to_u64(row.try_get("body_bytes").map_err(db_err)?),
    })
}

const DATASET_COLUMNS: &str = "d.id, d.name, d.description, d.source, d.retention_days, d.created_at,
    (SELECT count(*) FROM records r WHERE r.dataset_id = d.id) AS record_count";

/// Result of one ingestion batch.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize)]
pub struct IngestReport {
    pub inserted: u64,
    pub updated: u64,
    pub duplicates: u64,
    pub redacted: u64,
}

impl IngestReport {
    pub fn merge(&mut self, other: IngestReport) {
        self.inserted += other.inserted;
        self.updated += other.updated;
        self.duplicates += other.duplicates;
        self.redacted += other.redacted;
    }
}

/// Options for one ingestion batch.
#[derive(Debug, Clone)]
pub struct IngestOptions {
    pub source: String,
    pub redact: bool,
    pub limits: IngestLimits,
}

impl Store {
    pub async fn create_dataset(
        &self,
        name: &str,
        description: &str,
        source: &str,
        retention_days: u32,
        actor: &str,
    ) -> CoreResult<Dataset> {
        if name.trim().is_empty() || name.len() > 200 {
            return Err(CoreError::invalid("dataset name must be 1..=200 bytes"));
        }
        let mut tx = self.begin().await?;
        let id: i64 = sqlx::query_scalar(
            "INSERT INTO datasets (name, description, source, retention_days) VALUES ($1, $2, $3, $4) RETURNING id",
        )
        .bind(name.trim())
        .bind(description)
        .bind(source)
        .bind(to_i32(retention_days)?)
        .fetch_one(&mut *tx)
        .await
        .map_err(db_err)?;
        audit(&mut tx, actor, "dataset.create", &format!("dataset:{id}"), serde_json::json!({"name": name})).await?;
        self.commit(tx, Vec::new()).await?;
        self.get_dataset(DatasetId(id)).await
    }

    pub async fn get_dataset(&self, id: DatasetId) -> CoreResult<Dataset> {
        let sql = format!("SELECT {DATASET_COLUMNS} FROM datasets d WHERE d.id = $1");
        let row = sqlx::query(sqlx::AssertSqlSafe(sql))
            .bind(id.0)
            .fetch_optional(self.pool())
            .await
            .map_err(db_err)?
            .ok_or_else(|| CoreError::not_found(format!("dataset {id}")))?;
        dataset_from_row(&row)
    }

    pub async fn find_dataset(&self, name: &str) -> CoreResult<Option<Dataset>> {
        let sql = format!("SELECT {DATASET_COLUMNS} FROM datasets d WHERE d.name = $1");
        let row = sqlx::query(sqlx::AssertSqlSafe(sql))
            .bind(name)
            .fetch_optional(self.pool())
            .await
            .map_err(db_err)?;
        row.as_ref().map(dataset_from_row).transpose()
    }

    pub async fn list_datasets(&self, page: Page) -> CoreResult<Paged<Dataset>> {
        let page = page.clamped();
        let total: i64 = sqlx::query_scalar("SELECT count(*) FROM datasets")
            .fetch_one(self.pool())
            .await
            .map_err(db_err)?;
        let sql = format!("SELECT {DATASET_COLUMNS} FROM datasets d ORDER BY d.id LIMIT $1 OFFSET $2");
        let rows = sqlx::query(sqlx::AssertSqlSafe(sql))
            .bind(i64::from(page.limit))
            .bind(i64::from(page.offset))
            .fetch_all(self.pool())
            .await
            .map_err(db_err)?;
        let items = rows.iter().map(dataset_from_row).collect::<CoreResult<_>>()?;
        Ok(Paged::new(items, to_u64(total), page))
    }

    /// Validates, optionally redacts and upserts one batch of records in a single
    /// transaction. Records are deduplicated by `(dataset, external_id)`; unchanged
    /// content (same hash) counts as a duplicate and is left untouched.
    pub async fn ingest_records(
        &self,
        dataset: DatasetId,
        batch: Vec<(u64, RecordInput)>,
        options: &IngestOptions,
    ) -> CoreResult<IngestReport> {
        let retention_days = self.get_dataset(dataset).await?.retention_days;
        let mut report = IngestReport::default();
        let mut tx = self.begin().await?;
        for (line, mut input) in batch {
            input
                .validate(&options.limits)
                .map_err(|e| CoreError::invalid(format!("line {line}: {e}")))?;
            let mut was_redacted = false;
            if options.redact {
                let (title, a) = redact(&input.title);
                let (body, b) = redact(&input.body);
                input.title = title;
                input.body = body;
                was_redacted = a || b;
                if was_redacted {
                    report.redacted += 1;
                }
            }
            let hash = input.content_hash();
            let row = sqlx::query(
                "INSERT INTO records (dataset_id, external_id, title, content_type, body, fields,
                                      content_hash, redacted, origin, source, locator, retention_until)
                 VALUES ($1, $2, $3, $4, $5, $6, $7, $8, 'supplied', $9, $10,
                         CASE WHEN $11 > 0 THEN now() + make_interval(days => $11) END)
                 ON CONFLICT (dataset_id, external_id) DO UPDATE
                     SET title = EXCLUDED.title, content_type = EXCLUDED.content_type,
                         body = EXCLUDED.body, fields = EXCLUDED.fields,
                         content_hash = EXCLUDED.content_hash, redacted = EXCLUDED.redacted,
                         source = EXCLUDED.source, locator = EXCLUDED.locator,
                         imported_at = now()
                     WHERE records.content_hash <> EXCLUDED.content_hash
                 RETURNING (xmax = 0) AS inserted",
            )
            .bind(dataset.0)
            .bind(&input.external_id)
            .bind(&input.title)
            .bind(&input.content_type)
            .bind(&input.body)
            .bind(&input.fields)
            .bind(hash.as_str())
            .bind(was_redacted)
            .bind(&options.source)
            .bind(format!("line {line}"))
            .bind(to_i32(retention_days)?)
            .fetch_optional(&mut *tx)
            .await
            .map_err(db_err)?;
            match row {
                None => report.duplicates += 1,
                Some(row) => {
                    if row.try_get::<bool, _>("inserted").map_err(db_err)? {
                        report.inserted += 1;
                    } else {
                        report.updated += 1;
                    }
                }
            }
        }
        let event = insert_event(
            &mut tx,
            None,
            None,
            DomainEvent::RecordsImported {
                dataset_id: dataset,
                inserted: report.inserted + report.updated,
                duplicates: report.duplicates,
            },
        )
        .await?;
        self.commit(tx, vec![event]).await?;
        Ok(report)
    }

    pub async fn get_record(&self, id: RecordId) -> CoreResult<ImportedRecord> {
        let row = sqlx::query("SELECT * FROM records WHERE id = $1")
            .bind(id.0)
            .fetch_optional(self.pool())
            .await
            .map_err(db_err)?
            .ok_or_else(|| CoreError::not_found(format!("record {id}")))?;
        record_from_row(&row)
    }

    /// Lists record summaries, optionally filtered by dataset and full-text query.
    pub async fn list_records(
        &self,
        dataset: Option<DatasetId>,
        query: Option<&str>,
        page: Page,
    ) -> CoreResult<Paged<RecordSummary>> {
        let page = page.clamped();
        let query = query.map(str::trim).filter(|q| !q.is_empty());
        let total: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM records
             WHERE ($1::bigint IS NULL OR dataset_id = $1)
               AND ($2::text IS NULL OR search @@ websearch_to_tsquery('simple', $2))",
        )
        .bind(dataset.map(|d| d.0))
        .bind(query)
        .fetch_one(self.pool())
        .await
        .map_err(db_err)?;
        let rows = sqlx::query(
            "SELECT id, dataset_id, external_id, title, content_type, octet_length(body)::bigint AS body_bytes
             FROM records
             WHERE ($1::bigint IS NULL OR dataset_id = $1)
               AND ($2::text IS NULL OR search @@ websearch_to_tsquery('simple', $2))
             ORDER BY id LIMIT $3 OFFSET $4",
        )
        .bind(dataset.map(|d| d.0))
        .bind(query)
        .bind(i64::from(page.limit))
        .bind(i64::from(page.offset))
        .fetch_all(self.pool())
        .await
        .map_err(db_err)?;
        let items = rows.iter().map(summary_from_row).collect::<CoreResult<_>>()?;
        Ok(Paged::new(items, to_u64(total), page))
    }

    pub async fn record_count(&self) -> CoreResult<u64> {
        let n: i64 = sqlx::query_scalar("SELECT count(*) FROM records")
            .fetch_one(self.pool())
            .await
            .map_err(db_err)?;
        Ok(to_u64(n))
    }

    /// Deletes a dataset and its records. Fails while a task still references it.
    pub async fn delete_dataset(&self, id: DatasetId, actor: &str) -> CoreResult<u64> {
        let mut tx = self.begin().await?;
        let removed: i64 = sqlx::query_scalar("SELECT count(*) FROM records WHERE dataset_id = $1")
            .bind(id.0)
            .fetch_one(&mut *tx)
            .await
            .map_err(db_err)?;
        let result = sqlx::query("DELETE FROM datasets WHERE id = $1")
            .bind(id.0)
            .execute(&mut *tx)
            .await
            .map_err(|e| match db_err(e) {
                CoreError::Invalid(m) => CoreError::Conflict(format!("dataset is still referenced: {m}")),
                other => other,
            })?;
        if result.rows_affected() == 0 {
            return Err(CoreError::not_found(format!("dataset {id}")));
        }
        audit(&mut tx, actor, "dataset.delete", &format!("dataset:{id}"), serde_json::json!({"records": removed})).await?;
        self.commit(tx, Vec::new()).await?;
        Ok(to_u64(removed))
    }

    /// Deletes records whose retention period has elapsed and that no finding references.
    pub async fn purge_expired_records(&self) -> CoreResult<u64> {
        let result = sqlx::query(
            "DELETE FROM records r WHERE r.retention_until IS NOT NULL AND r.retention_until < now()
               AND NOT EXISTS (SELECT 1 FROM finding_records f WHERE f.record_id = r.id)",
        )
        .execute(self.pool())
        .await
        .map_err(db_err)?;
        Ok(result.rows_affected())
    }

    async fn scope_dataset_check(&self, scope: &RecordScope) -> CoreResult<Vec<i64>> {
        if scope.dataset_ids.is_empty() {
            return Err(CoreError::denied("the session has no approved datasets"));
        }
        Ok(scope.dataset_ids.iter().map(|d| d.0).collect())
    }
}

#[async_trait]
impl RecordSource for Store {
    async fn search_records(
        &self,
        scope: &RecordScope,
        query: &str,
        limit: u32,
    ) -> CoreResult<Vec<RecordSummary>> {
        let datasets = self.scope_dataset_check(scope).await?;
        let narrowed: Option<Vec<i64>> = scope
            .record_ids
            .as_ref()
            .map(|ids| ids.iter().map(|r| r.0).collect());
        let query = query.trim();
        let rows = sqlx::query(
            "SELECT id, dataset_id, external_id, title, content_type, octet_length(body)::bigint AS body_bytes
             FROM records
             WHERE dataset_id = ANY($1)
               AND ($2::bigint[] IS NULL OR id = ANY($2))
               AND ($3 = '' OR search @@ websearch_to_tsquery('simple', $3))
             ORDER BY id LIMIT $4",
        )
        .bind(&datasets)
        .bind(&narrowed)
        .bind(query)
        .bind(i64::from(limit.clamp(1, 200)))
        .fetch_all(self.pool())
        .await
        .map_err(db_err)?;
        rows.iter().map(summary_from_row).collect()
    }

    async fn get_records(
        &self,
        scope: &RecordScope,
        ids: &[RecordId],
    ) -> CoreResult<Vec<ImportedRecord>> {
        let datasets = self.scope_dataset_check(scope).await?;
        let wanted: Vec<i64> = ids.iter().map(|r| r.0).collect();
        let rows = sqlx::query("SELECT * FROM records WHERE id = ANY($1) AND dataset_id = ANY($2) ORDER BY id")
            .bind(&wanted)
            .bind(&datasets)
            .fetch_all(self.pool())
            .await
            .map_err(db_err)?;
        let records: Vec<ImportedRecord> = rows.iter().map(record_from_row).collect::<CoreResult<_>>()?;
        if records.len() != wanted.len() || records.iter().any(|r| !scope.allows(r)) {
            return Err(CoreError::denied("one or more records are outside the approved scope"));
        }
        Ok(records)
    }

    async fn scope_stats(&self, scope: &RecordScope) -> CoreResult<ScopeStats> {
        let datasets = self.scope_dataset_check(scope).await?;
        let narrowed: Option<Vec<i64>> = scope
            .record_ids
            .as_ref()
            .map(|ids| ids.iter().map(|r| r.0).collect());
        let rows: Vec<(String, i64, i64)> = sqlx::query_as(
            "SELECT content_type, count(*), COALESCE(sum(octet_length(body)), 0)::bigint FROM records
             WHERE dataset_id = ANY($1) AND ($2::bigint[] IS NULL OR id = ANY($2))
             GROUP BY content_type ORDER BY content_type",
        )
        .bind(&datasets)
        .bind(&narrowed)
        .fetch_all(self.pool())
        .await
        .map_err(db_err)?;
        let findings: i64 = match scope.task_id {
            Some(task) => sqlx::query_scalar("SELECT count(*) FROM findings WHERE task_id = $1")
                .bind(task.0)
                .fetch_one(self.pool())
                .await
                .map_err(db_err)?,
            None => 0,
        };
        let mut stats = ScopeStats {
            findings: to_u64(findings),
            ..ScopeStats::default()
        };
        for (content_type, count, bytes) in rows {
            stats.records += to_u64(count);
            stats.total_body_bytes += to_u64(bytes);
            stats.by_content_type.push((content_type, to_u64(count)));
        }
        Ok(stats)
    }

    async fn exploration_snapshot(&self, task_id: TaskId) -> CoreResult<ExplorationSnapshot> {
        let exploration = self.exploration_of(task_id).await?;
        self.load_exploration(exploration).await
    }
}
