use std::fmt;

use serde::{Deserialize, Serialize};

use crate::{Page, View};

pub const PROTOCOL_VERSION: u16 = 1;
pub const MAX_PAGE_SIZE: u16 = 100;

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Request {
    pub version: u16,
    pub query: Query,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case", deny_unknown_fields)]
pub enum Query {
    Ping,
    Browse {
        view: View,
        before_id: Option<i64>,
        limit: u16,
    },
}

impl Query {
    pub fn validate(&self) -> Result<(), ErrorCode> {
        if let Self::Browse {
            view,
            before_id,
            limit,
        } = self
            && (!(1..=MAX_PAGE_SIZE).contains(limit)
                || before_id.is_some_and(|id| id <= 0)
                || matches!(view, View::Exploration { task_id } if *task_id <= 0))
        {
            return Err(ErrorCode::InvalidRequest);
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Response {
    pub version: u16,
    pub reply: Reply,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "result", rename_all = "snake_case")]
pub enum Reply {
    Pong,
    Page(Page),
    Error { code: ErrorCode },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorCode {
    IncompatibleVersion,
    InvalidRequest,
    StoreUnavailable,
}

impl fmt::Display for ErrorCode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::IncompatibleVersion => "incompatible IPC protocol version",
            Self::InvalidRequest => "invalid request",
            Self::StoreUnavailable => "records are unavailable",
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn browse_contract_keeps_legacy_bigint_ids() {
        let fixture = r#"{"version":1,"query":{"op":"browse","view":{"exploration":{"task_id":9007199254740993}},"before_id":7,"limit":25}}"#;
        let request: Request = serde_json::from_str(fixture).unwrap();
        assert_eq!(request.version, PROTOCOL_VERSION);
        assert_eq!(
            request.query,
            Query::Browse {
                view: View::Exploration {
                    task_id: 9_007_199_254_740_993
                },
                before_id: Some(7),
                limit: 25,
            }
        );
        assert_eq!(serde_json::to_string(&request).unwrap(), fixture);
        assert_eq!(request.query.validate(), Ok(()));
    }

    #[test]
    fn rejects_invalid_bounds() {
        for (task_id, before_id, limit) in [
            (1, None, 0),
            (1, None, MAX_PAGE_SIZE + 1),
            (0, None, 25),
            (1, Some(-1), 25),
        ] {
            assert_eq!(
                Query::Browse {
                    view: View::Exploration { task_id },
                    before_id,
                    limit
                }
                .validate(),
                Err(ErrorCode::InvalidRequest)
            );
        }
    }
}
