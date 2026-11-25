//! The global Asset Graph organizes imported records, their groupings and provenance.
//! Graph state is descriptive only: nothing here initiates actions against external systems.

use std::collections::{BTreeMap, BTreeSet};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use super::graph::Digraph;
use super::{AssetId, Provenance, str_enum};
use crate::{CoreError, CoreResult};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AssetKind {
    /// An owning organization or business unit.
    Organization,
    /// A logical grouping such as a project or archive.
    Collection,
    /// A document under review.
    Document,
    /// An imported dataset.
    Dataset,
    /// A named entity referenced by records (a product, a counterparty, a system name).
    Entity,
    /// An inert reference preserved from an imported inventory (for example a legacy
    /// inventory entry). References are labels; they are never contacted.
    Reference,
}

str_enum!(AssetKind {
    Organization => "organization",
    Collection => "collection",
    Document => "document",
    Dataset => "dataset",
    Entity => "entity",
    Reference => "reference",
});

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AssetRelation {
    /// Hierarchical ownership; must form a forest (no cycles, at most one parent).
    Contains,
    References,
    DerivedFrom,
    DuplicateOf,
}

str_enum!(AssetRelation {
    Contains => "contains",
    References => "references",
    DerivedFrom => "derived_from",
    DuplicateOf => "duplicate_of",
});

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Asset {
    pub id: AssetId,
    pub kind: AssetKind,
    /// Normalized natural key used for deduplication; unique per kind.
    pub identity_key: String,
    pub label: String,
    pub attributes: serde_json::Value,
    pub provenance: Provenance,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NewAsset {
    pub kind: AssetKind,
    pub label: String,
    /// Optional explicit natural key; defaults to the normalized label.
    #[serde(default)]
    pub identity_key: Option<String>,
    #[serde(default = "empty_object")]
    pub attributes: serde_json::Value,
    pub provenance: Provenance,
    /// Preserve an imported identifier when migrating legacy records.
    #[serde(default)]
    pub preserve_id: Option<AssetId>,
}

fn empty_object() -> serde_json::Value {
    serde_json::Value::Object(Default::default())
}

pub const MAX_ASSET_LABEL: usize = 1_024;
pub const MAX_ASSET_ATTRIBUTES: usize = 64 * 1024;

impl NewAsset {
    pub fn normalized_key(&self) -> String {
        normalize_identity(self.identity_key.as_deref().unwrap_or(&self.label))
    }

    pub fn validate(&self) -> CoreResult<()> {
        if self.label.trim().is_empty() {
            return Err(CoreError::invalid("asset label must not be empty"));
        }
        if self.label.len() > MAX_ASSET_LABEL {
            return Err(CoreError::LimitExceeded("asset label too long".into()));
        }
        if !self.attributes.is_object() {
            return Err(CoreError::invalid("asset attributes must be a JSON object"));
        }
        if self.attributes.to_string().len() > MAX_ASSET_ATTRIBUTES {
            return Err(CoreError::LimitExceeded("asset attributes too large".into()));
        }
        if self.normalized_key().is_empty() {
            return Err(CoreError::invalid("asset identity key is empty after normalization"));
        }
        Ok(())
    }
}

/// Deduplication key: trimmed, lowercased, inner whitespace collapsed, trailing dots removed.
pub fn normalize_identity(raw: &str) -> String {
    let collapsed = raw.split_whitespace().collect::<Vec<_>>().join(" ");
    collapsed.trim_end_matches('.').to_lowercase()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct AssetEdge {
    pub src: AssetId,
    pub dst: AssetId,
    pub rel: AssetRelation,
}

/// In-memory view of (a bounded part of) the Asset Graph for invariant checks.
#[derive(Debug, Clone, Default)]
pub struct AssetGraph {
    pub assets: BTreeMap<AssetId, AssetKind>,
    pub edges: BTreeSet<AssetEdge>,
}

/// A violated Asset Graph invariant.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum AssetGraphIssue {
    DanglingEdge(AssetEdge),
    SelfLoop(AssetEdge),
    MultipleParents { child: AssetId, parents: Vec<AssetId> },
    ContainmentCycle(Vec<AssetId>),
    DuplicateChain(Vec<AssetId>),
}

impl AssetGraph {
    pub fn containment(&self) -> Digraph<AssetId> {
        self.relation(AssetRelation::Contains)
    }

    fn relation(&self, rel: AssetRelation) -> Digraph<AssetId> {
        let mut graph = Digraph::default();
        for id in self.assets.keys() {
            graph.add_node(*id);
        }
        for edge in self.edges.iter().filter(|e| e.rel == rel) {
            graph.add_edge(edge.src, edge.dst);
        }
        graph
    }

    /// Ancestry along `contains`, nearest parent first.
    pub fn ancestry(&self, id: AssetId) -> Vec<AssetId> {
        self.containment()
            .ancestors(id, 64, 64)
            .into_iter()
            .map(|(node, _)| node)
            .collect()
    }

    pub fn check(&self) -> Vec<AssetGraphIssue> {
        let mut issues = Vec::new();
        let mut parents: BTreeMap<AssetId, Vec<AssetId>> = BTreeMap::new();
        for edge in &self.edges {
            if edge.src == edge.dst {
                issues.push(AssetGraphIssue::SelfLoop(*edge));
                continue;
            }
            if !self.assets.contains_key(&edge.src) || !self.assets.contains_key(&edge.dst) {
                issues.push(AssetGraphIssue::DanglingEdge(*edge));
                continue;
            }
            if edge.rel == AssetRelation::Contains {
                parents.entry(edge.dst).or_default().push(edge.src);
            }
        }
        for (child, list) in parents {
            if list.len() > 1 {
                issues.push(AssetGraphIssue::MultipleParents { child, parents: list });
            }
        }
        if let Some(cycle) = self.containment().find_cycle() {
            issues.push(AssetGraphIssue::ContainmentCycle(cycle));
        }
        // duplicate_of must point at a canonical asset, never at another duplicate.
        let dup = self.relation(AssetRelation::DuplicateOf);
        for node in dup.nodes() {
            for target in dup.successors(node) {
                if dup.successors(target).next().is_some() {
                    issues.push(AssetGraphIssue::DuplicateChain(vec![node, target]));
                }
            }
        }
        issues
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn graph(edges: &[(i64, i64, AssetRelation)]) -> AssetGraph {
        let mut g = AssetGraph::default();
        for (s, d, rel) in edges {
            g.assets.insert(AssetId(*s), AssetKind::Collection);
            g.assets.insert(AssetId(*d), AssetKind::Document);
            g.edges.insert(AssetEdge {
                src: AssetId(*s),
                dst: AssetId(*d),
                rel: *rel,
            });
        }
        g
    }

    #[test]
    fn normalization_deduplicates_cosmetic_variants() {
        assert_eq!(normalize_identity("  Quarterly   Report. "), "quarterly report");
        assert_eq!(
            normalize_identity("Quarterly Report"),
            normalize_identity("quarterly  report.")
        );
    }

    #[test]
    fn healthy_tree_has_no_issues_and_ancestry() {
        let g = graph(&[(1, 2, AssetRelation::Contains), (2, 3, AssetRelation::Contains)]);
        assert!(g.check().is_empty());
        assert_eq!(g.ancestry(AssetId(3)), vec![AssetId(2), AssetId(1)]);
    }

    #[test]
    fn detects_violations() {
        let g = graph(&[
            (1, 3, AssetRelation::Contains),
            (2, 3, AssetRelation::Contains),
            (4, 5, AssetRelation::Contains),
            (5, 4, AssetRelation::Contains),
            (6, 7, AssetRelation::DuplicateOf),
            (7, 8, AssetRelation::DuplicateOf),
        ]);
        let issues = g.check();
        assert!(issues.iter().any(|i| matches!(i, AssetGraphIssue::MultipleParents { .. })));
        assert!(issues.iter().any(|i| matches!(i, AssetGraphIssue::ContainmentCycle(_))));
        assert!(issues.iter().any(|i| matches!(i, AssetGraphIssue::DuplicateChain(_))));
    }
}
