//! JSON Lines record import from an operator-selected local file.

use std::path::Path;

use arustex_core::domain::DatasetId;
use arustex_core::domain::record::{IngestLimits, RecordInput};
use arustex_core::{CoreError, CoreResult};
use arustex_store::Store;
use arustex_store::repository::records::{IngestOptions, IngestReport};
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncReadExt, BufReader};

const BATCH: usize = 500;

/// Reads `path` as JSON Lines and ingests it into `dataset`. Blank lines are skipped;
/// a malformed or oversized line aborts with its line number (earlier batches stay
/// committed, and re-running the import is idempotent).
pub async fn import_jsonl_file(
    store: &Store,
    dataset: DatasetId,
    path: &Path,
    options: &IngestOptions,
) -> CoreResult<IngestReport> {
    let meta = tokio::fs::symlink_metadata(path)
        .await
        .map_err(|e| CoreError::invalid(format!("cannot read {}: {e}", path.display())))?;
    if !meta.is_file() {
        return Err(CoreError::invalid(format!(
            "{} is not a regular file",
            path.display()
        )));
    }
    let file = tokio::fs::File::open(path)
        .await
        .map_err(|e| CoreError::invalid(format!("cannot open {}: {e}", path.display())))?;
    import_jsonl(store, dataset, file, options).await
}

/// Imports JSON Lines from any reader (used by the file import and tests).
pub async fn import_jsonl<R: AsyncRead + Unpin>(
    store: &Store,
    dataset: DatasetId,
    reader: R,
    options: &IngestOptions,
) -> CoreResult<IngestReport> {
    let limits: IngestLimits = options.limits;
    let mut reader = BufReader::new(reader);
    let mut report = IngestReport::default();
    let mut batch: Vec<(u64, RecordInput)> = Vec::with_capacity(BATCH);
    let mut line_no = 0u64;
    let mut records = 0u64;
    let mut buf = Vec::new();
    loop {
        buf.clear();
        let n = (&mut reader)
            .take(limits.max_line_bytes as u64 + 1)
            .read_until(b'\n', &mut buf)
            .await
            .map_err(|e| CoreError::invalid(format!("read error: {e}")))?;
        if n == 0 {
            break;
        }
        line_no += 1;
        if buf.len() > limits.max_line_bytes && buf.last() != Some(&b'\n') {
            return Err(CoreError::LimitExceeded(format!(
                "line {line_no} exceeds {} bytes",
                limits.max_line_bytes
            )));
        }
        let text = std::str::from_utf8(&buf)
            .map_err(|_| CoreError::invalid(format!("line {line_no} is not valid UTF-8")))?
            .trim();
        if text.is_empty() {
            continue;
        }
        let input: RecordInput = serde_json::from_str(text)
            .map_err(|e| CoreError::invalid(format!("line {line_no}: {e}")))?;
        input
            .validate(&limits)
            .map_err(|e| CoreError::invalid(format!("line {line_no}: {e}")))?;
        records += 1;
        if records > limits.max_records {
            return Err(CoreError::LimitExceeded(format!(
                "import exceeds {} records",
                limits.max_records
            )));
        }
        batch.push((line_no, input));
        if batch.len() >= BATCH {
            report.merge(
                store
                    .ingest_records(dataset, std::mem::take(&mut batch), options)
                    .await?,
            );
        }
    }
    if !batch.is_empty() {
        report.merge(store.ingest_records(dataset, batch, options).await?);
    }
    Ok(report)
}
