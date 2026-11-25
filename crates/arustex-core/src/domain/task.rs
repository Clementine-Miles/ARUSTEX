use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use super::{DatasetId, ExplorationId, TaskId, str_enum};
use crate::{CoreError, CoreResult};

/// Lifecycle of a review task.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskStatus {
    Created,
    Queued,
    Running,
    Paused,
    NeedsReview,
    Completed,
    Failed,
    Cancelled,
    TimedOut,
}

str_enum!(TaskStatus {
    Created => "created",
    Queued => "queued",
    Running => "running",
    Paused => "paused",
    NeedsReview => "needs_review",
    Completed => "completed",
    Failed => "failed",
    Cancelled => "cancelled",
    TimedOut => "timed_out",
});

impl TaskStatus {
    pub fn is_terminal(self) -> bool {
        matches!(
            self,
            Self::Completed | Self::Failed | Self::Cancelled | Self::TimedOut
        )
    }

    /// The explicit task state machine. Terminal states never change again.
    pub fn can_transition_to(self, next: Self) -> bool {
        use TaskStatus::*;
        if self == next {
            return false;
        }
        match self {
            Created => matches!(next, Queued | Running | Cancelled),
            Queued => matches!(next, Running | Paused | Cancelled),
            Running => matches!(
                next,
                Paused | NeedsReview | Completed | Failed | Cancelled | TimedOut | Queued
            ),
            Paused => matches!(next, Queued | Running | Cancelled),
            NeedsReview => matches!(next, Completed | Failed | Running | Cancelled),
            Completed | Failed | Cancelled | TimedOut => false,
        }
    }

    pub fn ensure_transition(self, next: Self) -> CoreResult<()> {
        if self.can_transition_to(next) {
            Ok(())
        } else {
            Err(CoreError::Conflict(format!(
                "task cannot move from {self} to {next}"
            )))
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Task {
    pub id: TaskId,
    pub name: String,
    pub objective: String,
    pub description: String,
    pub status: TaskStatus,
    pub exploration_id: ExplorationId,
    pub dataset_ids: Vec<DatasetId>,
    /// Records per planned work item.
    pub batch_size: u32,
    /// Upper bound on concurrent workers for this task.
    pub max_workers: u32,
    /// Wall-clock limit in seconds; zero disables the limit.
    pub timeout_seconds: u32,
    /// Token budget across every session of the task; zero disables the limit.
    pub token_budget: u64,
    pub tokens_used: u64,
    pub legacy_id: Option<i64>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub started_at: Option<DateTime<Utc>>,
    pub completed_at: Option<DateTime<Utc>>,
}

impl Task {
    pub fn display_name(&self) -> &str {
        if self.name.trim().is_empty() {
            &self.objective
        } else {
            &self.name
        }
    }

    pub fn deadline(&self) -> Option<DateTime<Utc>> {
        if self.timeout_seconds == 0 {
            return None;
        }
        self.started_at
            .map(|start| start + chrono::Duration::seconds(i64::from(self.timeout_seconds)))
    }
}

/// Operator input for creating a task.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NewTask {
    pub name: String,
    pub objective: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub dataset_ids: Vec<DatasetId>,
    #[serde(default = "default_batch_size")]
    pub batch_size: u32,
    #[serde(default = "default_max_workers")]
    pub max_workers: u32,
    #[serde(default)]
    pub timeout_seconds: u32,
    #[serde(default)]
    pub token_budget: u64,
}

fn default_batch_size() -> u32 {
    25
}

fn default_max_workers() -> u32 {
    2
}

pub const MAX_TASK_TEXT: usize = 8 * 1024;
pub const MAX_BATCH_SIZE: u32 = 1_000;
pub const MAX_TASK_WORKERS: u32 = 64;

impl NewTask {
    pub fn validate(&self) -> CoreResult<()> {
        if self.objective.trim().is_empty() {
            return Err(CoreError::invalid("task objective must not be empty"));
        }
        for (field, value) in [
            ("name", &self.name),
            ("objective", &self.objective),
            ("description", &self.description),
        ] {
            if value.len() > MAX_TASK_TEXT {
                return Err(CoreError::LimitExceeded(format!(
                    "task {field} exceeds {MAX_TASK_TEXT} bytes"
                )));
            }
        }
        if self.batch_size == 0 || self.batch_size > MAX_BATCH_SIZE {
            return Err(CoreError::invalid(format!(
                "batch_size must be within 1..={MAX_BATCH_SIZE}"
            )));
        }
        if self.max_workers == 0 || self.max_workers > MAX_TASK_WORKERS {
            return Err(CoreError::invalid(format!(
                "max_workers must be within 1..={MAX_TASK_WORKERS}"
            )));
        }
        Ok(())
    }
}

/// Operator control actions accepted for a task.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskAction {
    Start,
    Pause,
    Resume,
    Cancel,
    /// Accept the results of a task waiting for human review.
    Accept,
}

impl TaskAction {
    /// The status this action requests from the current status, if it is allowed.
    pub fn target(self, current: TaskStatus) -> CoreResult<TaskStatus> {
        let next = match self {
            Self::Start => TaskStatus::Queued,
            Self::Pause => TaskStatus::Paused,
            Self::Resume => TaskStatus::Queued,
            Self::Cancel => TaskStatus::Cancelled,
            Self::Accept => TaskStatus::Completed,
        };
        if self == Self::Accept && current != TaskStatus::NeedsReview {
            return Err(CoreError::Conflict(
                "only tasks awaiting review can be accepted".into(),
            ));
        }
        current.ensure_transition(next)?;
        Ok(next)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn terminal_states_are_final() {
        for terminal in [
            TaskStatus::Completed,
            TaskStatus::Failed,
            TaskStatus::Cancelled,
            TaskStatus::TimedOut,
        ] {
            for next in TaskStatus::ALL {
                assert!(!terminal.can_transition_to(*next));
            }
        }
    }

    #[test]
    fn actions_follow_state_machine() {
        assert_eq!(
            TaskAction::Start.target(TaskStatus::Created).ok(),
            Some(TaskStatus::Queued)
        );
        assert!(TaskAction::Pause.target(TaskStatus::Running).is_ok());
        assert!(TaskAction::Resume.target(TaskStatus::Paused).is_ok());
        assert!(TaskAction::Accept.target(TaskStatus::Running).is_err());
        assert!(TaskAction::Start.target(TaskStatus::Completed).is_err());
    }

    #[test]
    fn new_task_validation() {
        let mut task = NewTask {
            name: "n".into(),
            objective: "Check record consistency".into(),
            description: String::new(),
            dataset_ids: vec![],
            batch_size: 10,
            max_workers: 2,
            timeout_seconds: 0,
            token_budget: 0,
        };
        assert!(task.validate().is_ok());
        task.batch_size = 0;
        assert!(task.validate().is_err());
        task.batch_size = 10;
        task.objective = " ".into();
        assert!(task.validate().is_err());
    }

    #[test]
    fn status_strings_round_trip() {
        for status in TaskStatus::ALL {
            assert_eq!(status.as_str().parse::<TaskStatus>().ok(), Some(*status));
        }
    }
}
