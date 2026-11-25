//! Typed domain records.

pub mod agent;
pub mod asset;
pub mod exploration;
pub mod finding;
pub mod graph;
pub mod record;
pub mod report;
pub mod task;
pub mod work;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// Declares an `i64`-backed identifier with a transparent wire format.
macro_rules! id_type {
    ($(#[$meta:meta])* $name:ident) => {
        $(#[$meta])*
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
        #[serde(transparent)]
        pub struct $name(pub i64);

        impl std::fmt::Display for $name {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                self.0.fmt(f)
            }
        }

        impl From<i64> for $name {
            fn from(value: i64) -> Self {
                Self(value)
            }
        }
    };
}

id_type!(TaskId);
id_type!(ExplorationId);
id_type!(NodeId);
id_type!(AssetId);
id_type!(DatasetId);
id_type!(RecordId);
id_type!(FindingId);
id_type!(NoteId);
id_type!(AttachmentId);
id_type!(WorkItemId);
id_type!(ApprovalId);
id_type!(ReportId);
id_type!(ReviewResultId);

/// Lowercase hexadecimal SHA-256 digest.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ContentHash(String);

impl ContentHash {
    pub fn of(bytes: &[u8]) -> Self {
        Self(hex::encode(Sha256::digest(bytes)))
    }

    /// Accepts a previously computed digest after validating its shape.
    pub fn parse(value: &str) -> crate::CoreResult<Self> {
        let ok = value.len() == 64
            && value
                .bytes()
                .all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'));
        if ok {
            Ok(Self(value.to_owned()))
        } else {
            Err(crate::CoreError::invalid(
                "content hash must be 64 lowercase hex digits",
            ))
        }
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for ContentHash {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// Where a stored item came from. Generated content is never confused with supplied content.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Provenance {
    pub origin: Origin,
    /// Human-readable source label, such as a file name or a legacy table name.
    pub source: String,
    /// Locator inside the source, such as a line number or a legacy row id.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub locator: Option<String>,
    /// Session that produced a generated item.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<uuid::Uuid>,
}

impl Provenance {
    pub fn supplied(source: impl Into<String>, locator: Option<String>) -> Self {
        Self {
            origin: Origin::Supplied,
            source: source.into(),
            locator,
            session_id: None,
        }
    }

    pub fn generated(session_id: uuid::Uuid) -> Self {
        Self {
            origin: Origin::ModelGenerated,
            source: "model".into(),
            locator: None,
            session_id: Some(session_id),
        }
    }

    pub fn operator(source: impl Into<String>) -> Self {
        Self {
            origin: Origin::Operator,
            source: source.into(),
            locator: None,
            session_id: None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Origin {
    /// Imported by an operator from a dataset, fixture or legacy database.
    Supplied,
    /// Produced by a model during a workflow session. Always requires review.
    ModelGenerated,
    /// Entered by an operator through the TUI or CLI.
    Operator,
    /// Computed deterministically by the application.
    Derived,
}

/// Implements `as_str` and `FromStr` for a fieldless enum with snake_case names.
macro_rules! str_enum {
    ($name:ident { $($variant:ident => $text:literal),+ $(,)? }) => {
        impl $name {
            pub const ALL: &'static [$name] = &[$($name::$variant),+];

            pub fn as_str(self) -> &'static str {
                match self {
                    $($name::$variant => $text),+
                }
            }
        }

        impl std::str::FromStr for $name {
            type Err = $crate::CoreError;

            fn from_str(value: &str) -> Result<Self, Self::Err> {
                match value {
                    $($text => Ok($name::$variant),)+
                    other => Err($crate::CoreError::invalid(format!(
                        concat!("unknown ", stringify!($name), " value: {}"),
                        other
                    ))),
                }
            }
        }

        impl std::fmt::Display for $name {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str(self.as_str())
            }
        }
    };
}
pub(crate) use str_enum;

str_enum!(Origin {
    Supplied => "supplied",
    ModelGenerated => "model_generated",
    Operator => "operator",
    Derived => "derived",
});

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn content_hash_matches_known_vector() {
        let hash = ContentHash::of(b"abc");
        assert_eq!(
            hash.as_str(),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        assert!(ContentHash::parse(hash.as_str()).is_ok());
        assert!(ContentHash::parse("ABC").is_err());
    }
}
