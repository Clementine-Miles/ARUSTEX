//! Workflow runtime: agent sessions and the task scheduler.
//!
//! A session is a bounded loop between one model provider and the static operation
//! registry. It ends only through an explicit outcome: a validated structured result
//! submitted with `submit_result`, a request for human review, a failure, cancellation
//! or an exhausted budget.

pub mod agent;
pub mod context;
pub mod roles;
pub mod scheduler;

pub use agent::{AgentRunner, PromptRef, SessionReport, SessionSpec};
pub use roles::{SUBMIT_RESULT, builtin_agents, default_prompt};
pub use scheduler::{Scheduler, SchedulerSettings, TaskHooks};
