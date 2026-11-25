//! Capability policy for model-selected operations.
//!
//! Every operation declares one [`Capability`] and an [`EffectClass`]. A session may
//! invoke an operation only when its role grants the capability, the operator has not
//! disabled the operation, and the per-session call budget is not exhausted. Anything
//! not explicitly allowed is denied: unknown operations, unknown roles and missing
//! scope all fail closed.
//!
//! Operations that would change stored state beyond a pending proposal are
//! [`EffectClass::ApprovalRequired`]: they only create an approval request, and an
//! operator applies them.

use std::collections::{BTreeMap, BTreeSet};

use arustex_core::domain::agent::AgentRole;
use arustex_core::ports::RecordScope;
use serde::{Deserialize, Serialize};

/// What an operation is allowed to touch.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Capability {
    /// Search and read records inside the session's approved scope.
    ReadRecords,
    /// Read the task's exploration graph and coverage.
    ReadGraph,
    /// Compute deterministic summaries over scoped records.
    ComputeSummary,
    /// Run graph integrity checks.
    ValidateGraph,
    /// Compare two structured values (worker output against expectations).
    CompareOutputs,
    /// Record a finding; it is stored as pending until an operator confirms it.
    ProposeFinding,
    /// Store a report draft; drafts are never exported without operator approval.
    ProposeReport,
    /// Request a change to stored review state; applied only after approval.
    ProposeUpdate,
}

/// The externally visible effect of an operation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EffectClass {
    /// Reads data; no state changes.
    ReadOnly,
    /// Stores a pending proposal (finding, draft) that needs operator review.
    Proposal,
    /// Creates an approval request; the change itself is applied by an operator.
    ApprovalRequired,
}

/// What the policy needs to know about an operation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct OperationClaim<'a> {
    pub name: &'a str,
    pub capability: Capability,
    pub effect: EffectClass,
}

/// The authority of one running session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PolicyContext {
    pub role: AgentRole,
    pub scope: RecordScope,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "decision", rename_all = "snake_case")]
pub enum Decision {
    Allow,
    /// Allowed, but the effect is an approval request rather than a direct change.
    AllowAsApprovalRequest,
    Deny {
        reason: String,
    },
}

impl Decision {
    pub fn is_allowed(&self) -> bool {
        !matches!(self, Decision::Deny { .. })
    }
}

/// Capabilities each role receives by default.
pub fn default_grants(role: AgentRole) -> &'static [Capability] {
    use Capability::*;
    match role {
        AgentRole::Planner => &[ReadRecords, ReadGraph, ComputeSummary, ValidateGraph],
        AgentRole::Worker => &[ReadRecords, ComputeSummary, ProposeFinding],
        AgentRole::Reviewer => &[ReadRecords, CompareOutputs, ComputeSummary],
        AgentRole::Assistant => &[
            ReadRecords,
            ReadGraph,
            ComputeSummary,
            ValidateGraph,
            ProposeReport,
            ProposeUpdate,
        ],
    }
}

/// The capability policy. Construct with [`Policy::standard`] and narrow it with
/// operator configuration; it can never grant beyond the defaults.
#[derive(Debug, Clone)]
pub struct Policy {
    grants: BTreeMap<AgentRole, BTreeSet<Capability>>,
    disabled: BTreeSet<String>,
    max_calls_per_session: u32,
}

impl Default for Policy {
    fn default() -> Self {
        Self::standard()
    }
}

impl Policy {
    pub fn standard() -> Self {
        let grants = AgentRole::ALL
            .iter()
            .map(|role| (*role, default_grants(*role).iter().copied().collect()))
            .collect();
        Self {
            grants,
            disabled: BTreeSet::new(),
            max_calls_per_session: 64,
        }
    }

    /// Removes a capability from a role. There is no way to add one.
    pub fn revoke(mut self, role: AgentRole, capability: Capability) -> Self {
        if let Some(set) = self.grants.get_mut(&role) {
            set.remove(&capability);
        }
        self
    }

    /// Disables an operation by name for every role.
    pub fn disable(mut self, operation: impl Into<String>) -> Self {
        self.disabled.insert(operation.into());
        self
    }

    pub fn with_call_budget(mut self, max_calls_per_session: u32) -> Self {
        self.max_calls_per_session = max_calls_per_session;
        self
    }

    pub fn max_calls_per_session(&self) -> u32 {
        self.max_calls_per_session
    }

    pub fn grants(&self, role: AgentRole) -> impl Iterator<Item = Capability> + '_ {
        self.grants.get(&role).into_iter().flatten().copied()
    }

    /// Whether the role may see the operation at all (used to build the model's tool list).
    pub fn visible(&self, role: AgentRole, op: &OperationClaim<'_>) -> bool {
        !self.disabled.contains(op.name)
            && self
                .grants
                .get(&role)
                .is_some_and(|g| g.contains(&op.capability))
    }

    /// Authorizes one invocation. `calls_so_far` counts this session's earlier calls.
    pub fn authorize(
        &self,
        ctx: &PolicyContext,
        op: &OperationClaim<'_>,
        calls_so_far: u32,
    ) -> Decision {
        if self.disabled.contains(op.name) {
            return Decision::Deny {
                reason: format!("operation {} is disabled by the operator", op.name),
            };
        }
        if !self
            .grants
            .get(&ctx.role)
            .is_some_and(|g| g.contains(&op.capability))
        {
            return Decision::Deny {
                reason: format!("role {} is not granted {:?}", ctx.role, op.capability),
            };
        }
        if calls_so_far >= self.max_calls_per_session {
            return Decision::Deny {
                reason: "operation call budget for this session is exhausted".into(),
            };
        }
        let needs_scope = matches!(
            op.capability,
            Capability::ReadRecords | Capability::ComputeSummary | Capability::ProposeFinding
        );
        if needs_scope && ctx.scope.dataset_ids.is_empty() {
            return Decision::Deny {
                reason: "the session has no approved record scope".into(),
            };
        }
        let needs_task = matches!(
            op.capability,
            Capability::ReadGraph
                | Capability::ValidateGraph
                | Capability::ProposeFinding
                | Capability::ProposeReport
                | Capability::ProposeUpdate
        );
        if needs_task && ctx.scope.task_id.is_none() {
            return Decision::Deny {
                reason: "the operation requires a task context".into(),
            };
        }
        match op.effect {
            EffectClass::ReadOnly | EffectClass::Proposal => Decision::Allow,
            EffectClass::ApprovalRequired => Decision::AllowAsApprovalRequest,
        }
    }
}

#[cfg(test)]
mod tests {
    use arustex_core::domain::{DatasetId, TaskId};

    use super::*;

    fn ctx(role: AgentRole, scoped: bool) -> PolicyContext {
        PolicyContext {
            role,
            scope: RecordScope {
                task_id: Some(TaskId(1)),
                dataset_ids: if scoped { vec![DatasetId(1)] } else { vec![] },
                record_ids: None,
            },
        }
    }

    const READ: OperationClaim<'static> = OperationClaim {
        name: "search_records",
        capability: Capability::ReadRecords,
        effect: EffectClass::ReadOnly,
    };
    const UPDATE: OperationClaim<'static> = OperationClaim {
        name: "propose_status_update",
        capability: Capability::ProposeUpdate,
        effect: EffectClass::ApprovalRequired,
    };

    #[test]
    fn roles_get_only_their_grants() {
        let p = Policy::standard();
        assert_eq!(
            p.authorize(&ctx(AgentRole::Worker, true), &READ, 0),
            Decision::Allow
        );
        assert!(
            !p.authorize(&ctx(AgentRole::Worker, true), &UPDATE, 0)
                .is_allowed()
        );
        assert_eq!(
            p.authorize(&ctx(AgentRole::Assistant, true), &UPDATE, 0),
            Decision::AllowAsApprovalRequest
        );
        assert!(!p.visible(AgentRole::Planner, &UPDATE));
    }

    #[test]
    fn fails_closed_without_scope_budget_or_when_disabled() {
        let p = Policy::standard().with_call_budget(2);
        assert!(
            !p.authorize(&ctx(AgentRole::Worker, false), &READ, 0)
                .is_allowed()
        );
        assert!(
            !p.authorize(&ctx(AgentRole::Worker, true), &READ, 2)
                .is_allowed()
        );
        let p = p.disable("search_records");
        assert!(
            !p.authorize(&ctx(AgentRole::Worker, true), &READ, 0)
                .is_allowed()
        );
        assert!(!p.visible(AgentRole::Worker, &READ));
        let p = Policy::standard().revoke(AgentRole::Worker, Capability::ReadRecords);
        assert!(
            !p.authorize(&ctx(AgentRole::Worker, true), &READ, 0)
                .is_allowed()
        );
    }

    #[test]
    fn no_role_can_both_propose_and_approve() {
        // Approval decisions are operator-only: no capability exists for them.
        for role in AgentRole::ALL {
            assert!(
                Policy::standard()
                    .grants(*role)
                    .all(|c| c != Capability::ProposeUpdate || *role == AgentRole::Assistant)
            );
        }
    }
}
