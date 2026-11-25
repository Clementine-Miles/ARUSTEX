use serde::{Deserialize, Serialize};

/// Stable, machine-readable error classes shared by every layer and the IPC protocol.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorCode {
    NotFound,
    Invalid,
    Conflict,
    LimitExceeded,
    Denied,
    Integrity,
    Cancelled,
    Timeout,
    Storage,
    Provider,
    Unavailable,
    Internal,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum CoreError {
    #[error("not found: {0}")]
    NotFound(String),
    #[error("invalid input: {0}")]
    Invalid(String),
    #[error("conflict: {0}")]
    Conflict(String),
    #[error("limit exceeded: {0}")]
    LimitExceeded(String),
    #[error("denied: {0}")]
    Denied(String),
    #[error("integrity violation: {0}")]
    Integrity(String),
    #[error("cancelled")]
    Cancelled,
    #[error("timed out: {0}")]
    Timeout(String),
    #[error("storage error: {0}")]
    Storage(String),
    #[error("provider error: {0}")]
    Provider(String),
    #[error("unavailable: {0}")]
    Unavailable(String),
    #[error("internal error: {0}")]
    Internal(String),
}

impl CoreError {
    pub fn code(&self) -> ErrorCode {
        match self {
            Self::NotFound(_) => ErrorCode::NotFound,
            Self::Invalid(_) => ErrorCode::Invalid,
            Self::Conflict(_) => ErrorCode::Conflict,
            Self::LimitExceeded(_) => ErrorCode::LimitExceeded,
            Self::Denied(_) => ErrorCode::Denied,
            Self::Integrity(_) => ErrorCode::Integrity,
            Self::Cancelled => ErrorCode::Cancelled,
            Self::Timeout(_) => ErrorCode::Timeout,
            Self::Storage(_) => ErrorCode::Storage,
            Self::Provider(_) => ErrorCode::Provider,
            Self::Unavailable(_) => ErrorCode::Unavailable,
            Self::Internal(_) => ErrorCode::Internal,
        }
    }

    /// Rebuilds an error from its wire representation.
    pub fn from_code(code: ErrorCode, message: String) -> Self {
        match code {
            ErrorCode::NotFound => Self::NotFound(message),
            ErrorCode::Invalid => Self::Invalid(message),
            ErrorCode::Conflict => Self::Conflict(message),
            ErrorCode::LimitExceeded => Self::LimitExceeded(message),
            ErrorCode::Denied => Self::Denied(message),
            ErrorCode::Integrity => Self::Integrity(message),
            ErrorCode::Cancelled => Self::Cancelled,
            ErrorCode::Timeout => Self::Timeout(message),
            ErrorCode::Storage => Self::Storage(message),
            ErrorCode::Provider => Self::Provider(message),
            ErrorCode::Unavailable => Self::Unavailable(message),
            ErrorCode::Internal => Self::Internal(message),
        }
    }

    pub fn invalid(message: impl Into<String>) -> Self {
        Self::Invalid(message.into())
    }

    pub fn not_found(message: impl Into<String>) -> Self {
        Self::NotFound(message.into())
    }

    pub fn denied(message: impl Into<String>) -> Self {
        Self::Denied(message.into())
    }

    /// Whether retrying the same request may succeed without operator action.
    pub fn is_transient(&self) -> bool {
        matches!(
            self,
            Self::Timeout(_) | Self::Unavailable(_) | Self::Storage(_) | Self::Provider(_)
        )
    }
}

pub type CoreResult<T> = Result<T, CoreError>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn code_round_trip_preserves_class() {
        let samples = [
            CoreError::NotFound("a".into()),
            CoreError::Denied("b".into()),
            CoreError::LimitExceeded("c".into()),
            CoreError::Cancelled,
        ];
        for err in samples {
            let rebuilt = CoreError::from_code(err.code(), err.to_string());
            assert_eq!(rebuilt.code(), err.code());
        }
    }
}
