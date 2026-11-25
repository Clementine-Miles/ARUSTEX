//! Imported datasets, records, attachments and redaction.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use super::{AttachmentId, ContentHash, DatasetId, Provenance, RecordId, str_enum};
use crate::{CoreError, CoreResult};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Dataset {
    pub id: DatasetId,
    pub name: String,
    pub description: String,
    pub source: String,
    pub record_count: u64,
    /// Days after import when records become eligible for deletion; zero keeps them.
    pub retention_days: u32,
    pub created_at: DateTime<Utc>,
}

/// One imported record. Bodies are untrusted data: they are displayed and analysed,
/// never interpreted as instructions.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ImportedRecord {
    pub id: RecordId,
    pub dataset_id: DatasetId,
    pub external_id: String,
    pub title: String,
    pub content_type: String,
    pub body: String,
    pub fields: serde_json::Value,
    pub content_hash: ContentHash,
    pub redacted: bool,
    pub provenance: Provenance,
    pub imported_at: DateTime<Utc>,
}

/// Wire format for one line of a JSON Lines record import.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RecordInput {
    pub external_id: String,
    #[serde(default)]
    pub title: String,
    #[serde(default = "default_content_type")]
    pub content_type: String,
    #[serde(default)]
    pub body: String,
    #[serde(default = "empty_object")]
    pub fields: serde_json::Value,
}

fn default_content_type() -> String {
    "text/plain".into()
}

fn empty_object() -> serde_json::Value {
    serde_json::Value::Object(Default::default())
}

/// Limits applied during ingestion.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct IngestLimits {
    pub max_line_bytes: usize,
    pub max_body_bytes: usize,
    pub max_fields_bytes: usize,
    pub max_records: u64,
}

impl Default for IngestLimits {
    fn default() -> Self {
        Self {
            max_line_bytes: 1024 * 1024,
            max_body_bytes: 512 * 1024,
            max_fields_bytes: 128 * 1024,
            max_records: 1_000_000,
        }
    }
}

impl RecordInput {
    pub fn validate(&self, limits: &IngestLimits) -> CoreResult<()> {
        if self.external_id.trim().is_empty() || self.external_id.len() > 256 {
            return Err(CoreError::invalid("external_id must be 1..=256 bytes"));
        }
        if self.title.len() > 1024 {
            return Err(CoreError::LimitExceeded("record title too long".into()));
        }
        if self.body.len() > limits.max_body_bytes {
            return Err(CoreError::LimitExceeded(format!(
                "record body exceeds {} bytes",
                limits.max_body_bytes
            )));
        }
        if !self.fields.is_object() {
            return Err(CoreError::invalid("record fields must be a JSON object"));
        }
        if self.fields.to_string().len() > limits.max_fields_bytes {
            return Err(CoreError::LimitExceeded("record fields too large".into()));
        }
        if !matches!(
            self.content_type.as_str(),
            "text/plain" | "text/markdown" | "application/json" | "text/csv"
        ) {
            return Err(CoreError::invalid(format!(
                "unsupported content type {}",
                self.content_type
            )));
        }
        Ok(())
    }

    /// Canonical hash over the identifying content of the record.
    pub fn content_hash(&self) -> ContentHash {
        let canonical = serde_json::json!({
            "external_id": self.external_id,
            "title": self.title,
            "content_type": self.content_type,
            "body": self.body,
            "fields": self.fields,
        });
        ContentHash::of(canonical.to_string().as_bytes())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AttachmentOwner {
    Task,
    Record,
    Finding,
    Report,
}

str_enum!(AttachmentOwner {
    Task => "task",
    Record => "record",
    Finding => "finding",
    Report => "report",
});

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Attachment {
    pub id: AttachmentId,
    pub owner: AttachmentOwner,
    pub owner_id: i64,
    pub name: String,
    pub media_type: String,
    pub size: u64,
    pub content_hash: ContentHash,
    pub retention_until: Option<DateTime<Utc>>,
    pub created_at: DateTime<Utc>,
}

/// Masks common secret and personal-data shapes in free text before it is stored or
/// sent to a model provider. Deterministic and allocation-bounded by input length.
pub fn redact(text: &str) -> (String, bool) {
    let mut changed = false;
    let mut out = Vec::new();
    for token in text.split_inclusive(char::is_whitespace) {
        let (word, trailing) = split_trailing_ws(token);
        let replaced = redact_token(word);
        if replaced != word {
            changed = true;
        }
        out.push(format!("{replaced}{trailing}"));
    }
    (out.concat(), changed)
}

fn split_trailing_ws(token: &str) -> (&str, &str) {
    let trimmed = token.trim_end_matches(char::is_whitespace);
    (trimmed, &token[trimmed.len()..])
}

const SECRET_KEYS: &[&str] = &[
    "password",
    "passwd",
    "pwd",
    "secret",
    "token",
    "api_key",
    "apikey",
    "access_key",
    "authorization",
    "bearer",
];

fn redact_token(word: &str) -> String {
    if word.is_empty() {
        return String::new();
    }
    // key=value or key:value secrets
    for sep in ['=', ':'] {
        if let Some((key, value)) = word.split_once(sep) {
            let key_l = key
                .trim_matches(|c: char| !c.is_alphanumeric() && c != '_')
                .to_lowercase();
            if !value.is_empty()
                && SECRET_KEYS
                    .iter()
                    .any(|k| key_l == *k || key_l.ends_with(&format!("_{k}")))
            {
                return format!("{key}{sep}[REDACTED]");
            }
        }
    }
    let core = word.trim_matches(|c: char| !c.is_alphanumeric() && !"@._+-".contains(c));
    // email addresses
    if let Some((local, domain)) = core.split_once('@')
        && !local.is_empty()
        && domain.contains('.')
        && !domain.starts_with('.')
        && !domain.ends_with('.')
    {
        return word.replace(core, "[EMAIL]");
    }
    // long digit runs (account / card style numbers), allowing separators
    let digits = core.chars().filter(char::is_ascii_digit).count();
    let only_numeric = core
        .chars()
        .all(|c| c.is_ascii_digit() || c == '-' || c == ' ');
    if digits >= 12 && only_numeric {
        return word.replace(core, "[NUMBER]");
    }
    word.to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redacts_secrets_emails_and_numbers() {
        let (text, changed) =
            redact("contact a.b@example.org password=hunter2 acct 1234-5678-9012-3456 ok");
        assert!(changed);
        assert_eq!(text, "contact [EMAIL] password=[REDACTED] acct [NUMBER] ok");
        let (same, changed) = redact("plain words 2026 only");
        assert!(!changed);
        assert_eq!(same, "plain words 2026 only");
    }

    #[test]
    fn record_validation_and_hash() {
        let limits = IngestLimits::default();
        let record = RecordInput {
            external_id: "r-1".into(),
            title: "t".into(),
            content_type: "text/plain".into(),
            body: "hello".into(),
            fields: serde_json::json!({"a": 1}),
        };
        assert!(record.validate(&limits).is_ok());
        assert_eq!(record.content_hash(), record.clone().content_hash());
        let mut bad = record.clone();
        bad.content_type = "application/x-sh".into();
        assert!(bad.validate(&limits).is_err());
        let mut big = record;
        big.body = "x".repeat(limits.max_body_bytes + 1);
        assert!(big.validate(&limits).is_err());
    }
}
