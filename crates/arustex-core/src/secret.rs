//! A string that never appears in `Debug`, `Display` or serialized output.

use std::fmt;

/// Holds a credential (API key, bot token, database password) in memory only.
/// Formatting and serialization always produce a redaction marker.
#[derive(Clone, PartialEq, Eq)]
pub struct Secret(String);

impl Secret {
    pub const REDACTED: &'static str = "[redacted]";

    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    /// Returns the secret value. Call sites must not log or persist the result.
    pub fn expose(&self) -> &str {
        &self.0
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

impl fmt::Debug for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(Self::REDACTED)
    }
}

impl fmt::Display for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(Self::REDACTED)
    }
}

impl serde::Serialize for Secret {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(Self::REDACTED)
    }
}

#[cfg(test)]
mod tests {
    use super::Secret;

    #[test]
    fn never_formats_the_value() {
        let s = Secret::new("sk-very-secret");
        assert_eq!(format!("{s:?} {s}"), "[redacted] [redacted]");
        assert_eq!(serde_json::to_string(&s).unwrap(), "\"[redacted]\"");
        assert_eq!(s.expose(), "sk-very-secret");
    }
}
