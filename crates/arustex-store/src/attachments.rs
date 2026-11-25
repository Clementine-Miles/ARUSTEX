//! Content-addressed attachment storage in PostgreSQL BYTEA chunks.
//!
//! Ingestion streams the input in bounded chunks inside one transaction, hashing each
//! chunk and the whole payload. Identical payloads share one blob. Reads verify every
//! chunk hash and the full hash before returning data.

use arustex_core::domain::record::{Attachment, AttachmentOwner};
use arustex_core::domain::{AttachmentId, ContentHash};
use arustex_core::{CoreError, CoreResult};
use sha2::{Digest, Sha256};
use sqlx::Row;
use sqlx::postgres::PgRow;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

use crate::postgres::{Store, audit, db_err, to_i32, to_i64, to_u64};

/// Limits for attachment ingestion.
#[derive(Debug, Clone, Copy)]
pub struct AttachmentLimits {
    pub max_bytes: u64,
    pub chunk_size: usize,
}

impl Default for AttachmentLimits {
    fn default() -> Self {
        Self {
            max_bytes: 64 * 1024 * 1024,
            chunk_size: 256 * 1024,
        }
    }
}

/// Media types accepted for attachments. Attachments are stored and returned as
/// opaque bytes; they are never executed or rendered as active content.
pub const ALLOWED_MEDIA_TYPES: &[&str] = &[
    "text/plain",
    "text/markdown",
    "text/csv",
    "application/json",
    "application/pdf",
    "image/png",
    "image/jpeg",
    "application/octet-stream",
];

/// Metadata for a new attachment.
#[derive(Debug, Clone)]
pub struct NewAttachment {
    pub owner: AttachmentOwner,
    pub owner_id: i64,
    pub name: String,
    pub media_type: String,
    pub retention_days: u32,
}

fn attachment_from_row(row: &PgRow) -> CoreResult<Attachment> {
    let owner: String = row.try_get("owner").map_err(db_err)?;
    let hash: String = row.try_get("blob_hash").map_err(db_err)?;
    Ok(Attachment {
        id: AttachmentId(row.try_get("id").map_err(db_err)?),
        owner: owner.parse()?,
        owner_id: row.try_get("owner_id").map_err(db_err)?,
        name: row.try_get("name").map_err(db_err)?,
        media_type: row.try_get("media_type").map_err(db_err)?,
        size: to_u64(row.try_get("size").map_err(db_err)?),
        content_hash: ContentHash::parse(&hash)?,
        retention_until: row.try_get("retention_until").map_err(db_err)?,
        created_at: row.try_get("created_at").map_err(db_err)?,
    })
}

fn sanitize_name(name: &str) -> CoreResult<String> {
    let base = name.rsplit(['/', '\\']).next().unwrap_or_default().trim();
    let cleaned: String = base.chars().filter(|c| !c.is_control()).take(255).collect();
    if cleaned.is_empty() || cleaned == "." || cleaned == ".." {
        return Err(CoreError::invalid("attachment name is empty"));
    }
    Ok(cleaned)
}

impl Store {
    /// Streams `reader` into content-addressed storage and links it to its owner.
    pub async fn put_attachment<R: AsyncRead + Unpin + Send>(
        &self,
        meta: &NewAttachment,
        mut reader: R,
        limits: AttachmentLimits,
        actor: &str,
    ) -> CoreResult<Attachment> {
        if !ALLOWED_MEDIA_TYPES.contains(&meta.media_type.as_str()) {
            return Err(CoreError::invalid(format!(
                "media type {} is not allowed",
                meta.media_type
            )));
        }
        if !(4096..=1024 * 1024).contains(&limits.chunk_size) {
            return Err(CoreError::invalid(
                "chunk size must be between 4 KiB and 1 MiB",
            ));
        }
        let name = sanitize_name(&meta.name)?;
        let mut tx = self.begin().await?;
        // Stage chunks under a random key; the content hash is only known at the end.
        let staging = hex::encode(Sha256::digest(uuid::Uuid::new_v4().as_bytes()));
        sqlx::query(
            "INSERT INTO blobs (hash, size, chunk_size, chunk_count) VALUES ($1, 0, $2, 0)",
        )
        .bind(&staging)
        .bind(to_i32(
            u32::try_from(limits.chunk_size).unwrap_or(u32::MAX),
        )?)
        .execute(&mut *tx)
        .await
        .map_err(db_err)?;
        let mut whole = Sha256::new();
        let mut size: u64 = 0;
        let mut idx: i32 = 0;
        let mut buf = vec![0u8; limits.chunk_size];
        loop {
            let mut filled = 0;
            while filled < buf.len() {
                let n = reader
                    .read(&mut buf[filled..])
                    .await
                    .map_err(|e| CoreError::invalid(format!("attachment read failed: {e}")))?;
                if n == 0 {
                    break;
                }
                filled += n;
            }
            if filled == 0 {
                break;
            }
            size += filled as u64;
            if size > limits.max_bytes {
                return Err(CoreError::LimitExceeded(format!(
                    "attachment exceeds {} bytes",
                    limits.max_bytes
                )));
            }
            let chunk = &buf[..filled];
            whole.update(chunk);
            sqlx::query("INSERT INTO blob_chunks (blob_hash, idx, data, chunk_hash) VALUES ($1, $2, $3, $4)")
                .bind(&staging)
                .bind(idx)
                .bind(chunk)
                .bind(hex::encode(Sha256::digest(chunk)))
                .execute(&mut *tx)
                .await
                .map_err(db_err)?;
            idx += 1;
            if filled < buf.len() {
                break;
            }
        }
        let hash = hex::encode(whole.finalize());
        let existing: Option<bool> =
            sqlx::query_scalar("SELECT complete FROM blobs WHERE hash = $1 FOR UPDATE")
                .bind(&hash)
                .fetch_optional(&mut *tx)
                .await
                .map_err(db_err)?;
        match existing {
            Some(true) => {
                sqlx::query("DELETE FROM blobs WHERE hash = $1")
                    .bind(&staging)
                    .execute(&mut *tx)
                    .await
                    .map_err(db_err)?;
            }
            _ => {
                sqlx::query("DELETE FROM blobs WHERE hash = $1 AND NOT complete")
                    .bind(&hash)
                    .execute(&mut *tx)
                    .await
                    .map_err(db_err)?;
                sqlx::query(
                    "INSERT INTO blobs (hash, size, chunk_size, chunk_count, complete) VALUES ($1, $2, $3, $4, true)",
                )
                .bind(&hash)
                .bind(to_i64(size)?)
                .bind(to_i32(u32::try_from(limits.chunk_size).unwrap_or(u32::MAX))?)
                .bind(idx)
                .execute(&mut *tx)
                .await
                .map_err(db_err)?;
                sqlx::query("UPDATE blob_chunks SET blob_hash = $1 WHERE blob_hash = $2")
                    .bind(&hash)
                    .bind(&staging)
                    .execute(&mut *tx)
                    .await
                    .map_err(db_err)?;
                sqlx::query("DELETE FROM blobs WHERE hash = $1")
                    .bind(&staging)
                    .execute(&mut *tx)
                    .await
                    .map_err(db_err)?;
            }
        }
        let row = sqlx::query(
            "INSERT INTO attachments (owner, owner_id, name, media_type, blob_hash, size, retention_until)
             VALUES ($1, $2, $3, $4, $5, $6, CASE WHEN $7 > 0 THEN now() + make_interval(days => $7) END)
             RETURNING *",
        )
        .bind(meta.owner.as_str())
        .bind(meta.owner_id)
        .bind(&name)
        .bind(&meta.media_type)
        .bind(&hash)
        .bind(to_i64(size)?)
        .bind(to_i32(meta.retention_days)?)
        .fetch_one(&mut *tx)
        .await
        .map_err(db_err)?;
        let attachment = attachment_from_row(&row)?;
        audit(
            &mut tx,
            actor,
            "attachment.put",
            &format!("attachment:{}", attachment.id),
            serde_json::json!({"size": size, "hash": hash}),
        )
        .await?;
        self.commit(tx, Vec::new()).await?;
        Ok(attachment)
    }

    pub async fn get_attachment(&self, id: AttachmentId) -> CoreResult<Attachment> {
        let row = sqlx::query("SELECT * FROM attachments WHERE id = $1")
            .bind(id.0)
            .fetch_optional(self.pool())
            .await
            .map_err(db_err)?
            .ok_or_else(|| CoreError::not_found(format!("attachment {id}")))?;
        attachment_from_row(&row)
    }

    pub async fn list_attachments(
        &self,
        owner: AttachmentOwner,
        owner_id: i64,
    ) -> CoreResult<Vec<Attachment>> {
        let rows =
            sqlx::query("SELECT * FROM attachments WHERE owner = $1 AND owner_id = $2 ORDER BY id")
                .bind(owner.as_str())
                .bind(owner_id)
                .fetch_all(self.pool())
                .await
                .map_err(db_err)?;
        rows.iter().map(attachment_from_row).collect()
    }

    /// Streams an attachment to `writer`, verifying every chunk and the full hash.
    /// On a mismatch the write stops with an integrity error.
    pub async fn read_attachment<W: AsyncWrite + Unpin + Send>(
        &self,
        id: AttachmentId,
        mut writer: W,
    ) -> CoreResult<u64> {
        let attachment = self.get_attachment(id).await?;
        let hash = attachment.content_hash.as_str().to_owned();
        let (count, complete): (i32, bool) =
            sqlx::query_as("SELECT chunk_count, complete FROM blobs WHERE hash = $1")
                .bind(&hash)
                .fetch_optional(self.pool())
                .await
                .map_err(db_err)?
                .ok_or_else(|| CoreError::Integrity(format!("blob {hash} is missing")))?;
        if !complete {
            return Err(CoreError::Integrity(format!("blob {hash} is incomplete")));
        }
        let mut whole = Sha256::new();
        let mut written = 0u64;
        for idx in 0..count {
            let (data, chunk_hash): (Vec<u8>, String) = sqlx::query_as(
                "SELECT data, chunk_hash FROM blob_chunks WHERE blob_hash = $1 AND idx = $2",
            )
            .bind(&hash)
            .bind(idx)
            .fetch_optional(self.pool())
            .await
            .map_err(db_err)?
            .ok_or_else(|| CoreError::Integrity(format!("blob {hash} chunk {idx} is missing")))?;
            if hex::encode(Sha256::digest(&data)) != chunk_hash {
                return Err(CoreError::Integrity(format!(
                    "blob {hash} chunk {idx} is corrupt"
                )));
            }
            whole.update(&data);
            writer
                .write_all(&data)
                .await
                .map_err(|e| CoreError::Internal(format!("attachment write failed: {e}")))?;
            written += data.len() as u64;
        }
        if hex::encode(whole.finalize()) != hash || written != attachment.size {
            return Err(CoreError::Integrity(format!(
                "blob {hash} does not match its hash"
            )));
        }
        writer
            .flush()
            .await
            .map_err(|e| CoreError::Internal(format!("attachment write failed: {e}")))?;
        Ok(written)
    }

    pub async fn read_attachment_bytes(&self, id: AttachmentId) -> CoreResult<Vec<u8>> {
        let mut out = Vec::new();
        self.read_attachment(id, &mut out).await?;
        Ok(out)
    }

    pub async fn delete_attachment(&self, id: AttachmentId, actor: &str) -> CoreResult<()> {
        let mut tx = self.begin().await?;
        let r = sqlx::query("DELETE FROM attachments WHERE id = $1")
            .bind(id.0)
            .execute(&mut *tx)
            .await
            .map_err(db_err)?;
        if r.rows_affected() == 0 {
            return Err(CoreError::not_found(format!("attachment {id}")));
        }
        audit(
            &mut tx,
            actor,
            "attachment.delete",
            &format!("attachment:{id}"),
            serde_json::json!({}),
        )
        .await?;
        self.commit(tx, Vec::new()).await
    }

    /// Deletes attachments past their retention date, then blobs no attachment uses.
    /// Returns `(attachments, blobs)` removed.
    pub async fn collect_garbage(&self) -> CoreResult<(u64, u64)> {
        let mut tx = self.begin().await?;
        let expired = sqlx::query(
            "DELETE FROM attachments WHERE retention_until IS NOT NULL AND retention_until < now()",
        )
        .execute(&mut *tx)
        .await
        .map_err(db_err)?
        .rows_affected();
        let blobs = sqlx::query(
            "DELETE FROM blobs b WHERE NOT EXISTS (SELECT 1 FROM attachments a WHERE a.blob_hash = b.hash)",
        )
        .execute(&mut *tx)
        .await
        .map_err(db_err)?
        .rows_affected();
        self.commit(tx, Vec::new()).await?;
        Ok((expired, blobs))
    }

    /// Verifies every complete blob; returns the hashes that fail verification.
    pub async fn verify_blobs(&self) -> CoreResult<Vec<String>> {
        let rows = sqlx::query(
            "SELECT b.hash, b.size, b.chunk_count,
                    (SELECT count(*) FROM blob_chunks c WHERE c.blob_hash = b.hash) AS present,
                    (SELECT COALESCE(sum(octet_length(c.data)), 0) FROM blob_chunks c WHERE c.blob_hash = b.hash)::bigint AS bytes
             FROM blobs b WHERE b.complete ORDER BY b.hash",
        )
        .fetch_all(self.pool())
        .await
        .map_err(db_err)?;
        let mut bad = Vec::new();
        for row in rows {
            let hash: String = row.try_get("hash").map_err(db_err)?;
            let size: i64 = row.try_get("size").map_err(db_err)?;
            let count: i32 = row.try_get("chunk_count").map_err(db_err)?;
            let present: i64 = row.try_get("present").map_err(db_err)?;
            let bytes: i64 = row.try_get("bytes").map_err(db_err)?;
            if present != i64::from(count) || bytes != size {
                bad.push(hash);
                continue;
            }
            let mut whole = Sha256::new();
            let mut ok = true;
            for idx in 0..count {
                let (data, chunk_hash): (Vec<u8>, String) = sqlx::query_as(
                    "SELECT data, chunk_hash FROM blob_chunks WHERE blob_hash = $1 AND idx = $2",
                )
                .bind(&hash)
                .bind(idx)
                .fetch_one(self.pool())
                .await
                .map_err(db_err)?;
                if hex::encode(Sha256::digest(&data)) != chunk_hash {
                    ok = false;
                    break;
                }
                whole.update(&data);
            }
            if !ok || hex::encode(whole.finalize()) != hash {
                bad.push(hash);
            }
        }
        Ok(bad)
    }
}
