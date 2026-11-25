//! The per-task Exploration Graph records review progress: the objective, planned work
//! items, facts and observations derived from records, and their relationships.
//! Kind, state and relation names keep the legacy wire values so imported graphs keep
//! their shape.

use std::collections::{BTreeMap, BTreeSet};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use super::graph::Digraph;
use super::{AssetId, ExplorationId, NodeId, RecordId, str_enum};
use crate::{CoreError, CoreResult};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExplorationStatus {
    Open,
    Achieved,
    Failed,
}

str_enum!(ExplorationStatus {
    Open => "open",
    Achieved => "achieved",
    Failed => "failed",
});

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Exploration {
    pub id: ExplorationId,
    pub objective: String,
    pub status: ExplorationStatus,
    pub round: i64,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// Node kinds. `Intent` is a planned work item and `Finding` an observation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NodeKind {
    Begin,
    Goal,
    Intent,
    Fact,
    Finding,
    Hint,
    Digest,
}

str_enum!(NodeKind {
    Begin => "begin",
    Goal => "goal",
    Intent => "intent",
    Fact => "fact",
    Finding => "finding",
    Hint => "hint",
    Digest => "digest",
});

impl NodeKind {
    /// Allowed states per kind, identical to the legacy `ck_node_state` constraint.
    pub fn allowed_states(self) -> &'static [&'static str] {
        match self {
            Self::Begin => &["open"],
            Self::Intent => &[
                "open",
                "running",
                "paused",
                "done",
                "blocked",
                "exhausted",
                "stopped",
                "deleted",
            ],
            Self::Goal => &["open", "met", "abandoned"],
            Self::Fact => &["confirmed", "dismissed", "origin"],
            Self::Finding => &["confirmed", "dismissed"],
            Self::Hint => &["active", "consumed"],
            Self::Digest => &["active", "superseded"],
        }
    }

    pub fn initial_state(self) -> &'static str {
        self.allowed_states()[0]
    }

    pub fn validate_state(self, state: &str) -> CoreResult<()> {
        if self.allowed_states().contains(&state) {
            Ok(())
        } else {
            Err(CoreError::invalid(format!(
                "state {state:?} is not valid for {self} nodes"
            )))
        }
    }

    /// Whether a node in `state` counts as finished for coverage purposes.
    pub fn is_closed(self, state: &str) -> bool {
        match self {
            Self::Intent => matches!(state, "done" | "exhausted" | "stopped" | "deleted"),
            Self::Goal => matches!(state, "met" | "abandoned"),
            Self::Fact | Self::Finding => matches!(state, "confirmed" | "dismissed"),
            Self::Begin | Self::Hint | Self::Digest => false,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EdgeRelation {
    Spawns,
    DerivedFrom,
    Yields,
    Proves,
    Covers,
}

str_enum!(EdgeRelation {
    Spawns => "spawns",
    DerivedFrom => "derived_from",
    Yields => "yields",
    Proves => "proves",
    Covers => "covers",
});

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ExplorationNode {
    pub id: NodeId,
    pub exploration_id: ExplorationId,
    pub kind: NodeKind,
    pub state: String,
    pub priority: i32,
    pub title: String,
    pub payload: serde_json::Value,
    pub origin: Option<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub completed_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NewNode {
    pub kind: NodeKind,
    pub title: String,
    #[serde(default)]
    pub state: Option<String>,
    #[serde(default)]
    pub priority: i32,
    #[serde(default)]
    pub payload: serde_json::Value,
    #[serde(default)]
    pub origin: Option<String>,
    #[serde(default)]
    pub preserve_id: Option<NodeId>,
}

impl NewNode {
    pub fn effective_state(&self) -> CoreResult<String> {
        let state = self
            .state
            .clone()
            .unwrap_or_else(|| self.kind.initial_state().to_owned());
        self.kind.validate_state(&state)?;
        Ok(state)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct ExplorationEdge {
    pub src: NodeId,
    pub dst: NodeId,
    pub rel: EdgeRelation,
}

/// Links between exploration nodes and the assets / records they concern.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub enum Anchor {
    Asset(AssetId),
    Record(RecordId),
}

/// In-memory snapshot of one exploration for invariant checks and coverage.
#[derive(Debug, Clone, Default)]
pub struct ExplorationSnapshot {
    pub nodes: BTreeMap<NodeId, (NodeKind, String)>,
    pub edges: BTreeSet<ExplorationEdge>,
    pub anchors: BTreeMap<NodeId, BTreeSet<Anchor>>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ExplorationIssue {
    MissingBegin,
    MultipleBegins(Vec<NodeId>),
    DanglingEdge(ExplorationEdge),
    InvalidState {
        node: NodeId,
        kind: NodeKind,
        state: String,
    },
    SpawnCycle(Vec<NodeId>),
    DerivationCycle(Vec<NodeId>),
    MultipleSpawnParents {
        node: NodeId,
        parents: Vec<NodeId>,
    },
    UnreachableIntent(NodeId),
}

/// Review coverage over a set of in-scope records.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct ReviewCoverage {
    pub total: u64,
    pub reviewed: u64,
}

impl ReviewCoverage {
    pub fn ratio(&self) -> f64 {
        if self.total == 0 {
            1.0
        } else {
            self.reviewed as f64 / self.total as f64
        }
    }

    pub fn percent(&self) -> u8 {
        (self.ratio() * 100.0).floor().clamp(0.0, 100.0) as u8
    }
}

impl ExplorationSnapshot {
    fn relation(&self, rel: EdgeRelation) -> Digraph<NodeId> {
        Digraph::from_edges(
            self.edges
                .iter()
                .filter(|e| e.rel == rel)
                .map(|e| (e.src, e.dst)),
        )
    }

    pub fn check(&self) -> Vec<ExplorationIssue> {
        let mut issues = Vec::new();
        let begins: Vec<_> = self
            .nodes
            .iter()
            .filter(|(_, (kind, _))| *kind == NodeKind::Begin)
            .map(|(id, _)| *id)
            .collect();
        match begins.len() {
            0 if !self.nodes.is_empty() => issues.push(ExplorationIssue::MissingBegin),
            0 | 1 => {}
            _ => issues.push(ExplorationIssue::MultipleBegins(begins.clone())),
        }
        for (id, (kind, state)) in &self.nodes {
            if kind.validate_state(state).is_err() {
                issues.push(ExplorationIssue::InvalidState {
                    node: *id,
                    kind: *kind,
                    state: state.clone(),
                });
            }
        }
        for edge in &self.edges {
            if !self.nodes.contains_key(&edge.src) || !self.nodes.contains_key(&edge.dst) {
                issues.push(ExplorationIssue::DanglingEdge(*edge));
            }
        }
        let spawns = self.relation(EdgeRelation::Spawns);
        if let Some(cycle) = spawns.find_cycle() {
            issues.push(ExplorationIssue::SpawnCycle(cycle));
        }
        if let Some(cycle) = self.relation(EdgeRelation::DerivedFrom).find_cycle() {
            issues.push(ExplorationIssue::DerivationCycle(cycle));
        }
        for node in spawns.nodes() {
            let parents: Vec<_> = spawns.predecessors(node).collect();
            if parents.len() > 1 {
                issues.push(ExplorationIssue::MultipleSpawnParents { node, parents });
            }
        }
        if let [begin] = begins.as_slice() {
            let reachable: BTreeSet<NodeId> = spawns
                .descendants(*begin, usize::MAX, usize::MAX)
                .into_iter()
                .map(|(n, _)| n)
                .collect();
            for (id, (kind, _)) in &self.nodes {
                if *kind == NodeKind::Intent && !reachable.contains(id) {
                    issues.push(ExplorationIssue::UnreachableIntent(*id));
                }
            }
        }
        issues
    }

    /// Records counted as reviewed: anchored to a closed intent, fact or finding node.
    pub fn coverage(&self, in_scope: &BTreeSet<RecordId>) -> ReviewCoverage {
        let mut reviewed = BTreeSet::new();
        for (node, anchors) in &self.anchors {
            let Some((kind, state)) = self.nodes.get(node) else {
                continue;
            };
            if !kind.is_closed(state) {
                continue;
            }
            for anchor in anchors {
                if let Anchor::Record(record) = anchor
                    && in_scope.contains(record)
                {
                    reviewed.insert(*record);
                }
            }
        }
        ReviewCoverage {
            total: in_scope.len() as u64,
            reviewed: reviewed.len() as u64,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snapshot() -> ExplorationSnapshot {
        let mut s = ExplorationSnapshot::default();
        s.nodes.insert(NodeId(1), (NodeKind::Begin, "open".into()));
        s.nodes.insert(NodeId(2), (NodeKind::Intent, "done".into()));
        s.nodes.insert(NodeId(3), (NodeKind::Intent, "open".into()));
        s.nodes
            .insert(NodeId(4), (NodeKind::Finding, "confirmed".into()));
        for (src, dst, rel) in [
            (1, 2, EdgeRelation::Spawns),
            (1, 3, EdgeRelation::Spawns),
            (2, 4, EdgeRelation::Yields),
        ] {
            s.edges.insert(ExplorationEdge {
                src: NodeId(src),
                dst: NodeId(dst),
                rel,
            });
        }
        s.anchors.insert(
            NodeId(2),
            BTreeSet::from([Anchor::Record(RecordId(10)), Anchor::Record(RecordId(11))]),
        );
        s.anchors
            .insert(NodeId(3), BTreeSet::from([Anchor::Record(RecordId(12))]));
        s
    }

    #[test]
    fn healthy_graph_passes() {
        assert!(snapshot().check().is_empty());
    }

    #[test]
    fn coverage_counts_closed_nodes_only() {
        let scope = BTreeSet::from([RecordId(10), RecordId(11), RecordId(12), RecordId(13)]);
        let coverage = snapshot().coverage(&scope);
        assert_eq!(
            coverage,
            ReviewCoverage {
                total: 4,
                reviewed: 2
            }
        );
        assert_eq!(coverage.percent(), 50);
        assert_eq!(
            ReviewCoverage {
                total: 0,
                reviewed: 0
            }
            .percent(),
            100
        );
    }

    #[test]
    fn detects_invalid_states_cycles_and_orphans() {
        let mut s = snapshot();
        s.nodes
            .insert(NodeId(5), (NodeKind::Finding, "open".into()));
        s.nodes.insert(NodeId(6), (NodeKind::Intent, "open".into()));
        s.edges.insert(ExplorationEdge {
            src: NodeId(3),
            dst: NodeId(1),
            rel: EdgeRelation::Spawns,
        });
        let issues = s.check();
        assert!(
            issues
                .iter()
                .any(|i| matches!(i, ExplorationIssue::InvalidState { .. }))
        );
        assert!(
            issues
                .iter()
                .any(|i| matches!(i, ExplorationIssue::SpawnCycle(_)))
        );
        assert!(
            issues
                .iter()
                .any(|i| matches!(i, ExplorationIssue::UnreachableIntent(NodeId(6))))
        );
    }

    #[test]
    fn state_rules_match_legacy_constraint() {
        assert!(NodeKind::Intent.validate_state("paused").is_ok());
        assert!(NodeKind::Finding.validate_state("open").is_err());
        assert_eq!(NodeKind::Digest.initial_state(), "active");
    }
}
