//! Pure, bounded graph algorithms shared by the Asset Graph and the Exploration Graph.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::hash::Hash;

/// A directed adjacency list keyed by node id.
#[derive(Debug, Clone)]
pub struct Digraph<N: Ord + Copy> {
    out: BTreeMap<N, BTreeSet<N>>,
    inc: BTreeMap<N, BTreeSet<N>>,
}

impl<N: Ord + Copy + Hash> Default for Digraph<N> {
    fn default() -> Self {
        Self {
            out: BTreeMap::new(),
            inc: BTreeMap::new(),
        }
    }
}

impl<N: Ord + Copy + Hash> Digraph<N> {
    pub fn from_edges(edges: impl IntoIterator<Item = (N, N)>) -> Self {
        let mut graph = Self::default();
        for (src, dst) in edges {
            graph.add_edge(src, dst);
        }
        graph
    }

    pub fn add_node(&mut self, node: N) {
        self.out.entry(node).or_default();
        self.inc.entry(node).or_default();
    }

    pub fn add_edge(&mut self, src: N, dst: N) {
        self.add_node(src);
        self.add_node(dst);
        self.out.entry(src).or_default().insert(dst);
        self.inc.entry(dst).or_default().insert(src);
    }

    pub fn nodes(&self) -> impl Iterator<Item = N> + '_ {
        self.out.keys().copied()
    }

    pub fn successors(&self, node: N) -> impl Iterator<Item = N> + '_ {
        self.out.get(&node).into_iter().flatten().copied()
    }

    pub fn predecessors(&self, node: N) -> impl Iterator<Item = N> + '_ {
        self.inc.get(&node).into_iter().flatten().copied()
    }

    /// Breadth-first traversal from `start` following outgoing edges, visiting at most
    /// `max_nodes` nodes and descending at most `max_depth` levels.
    pub fn descendants(&self, start: N, max_depth: usize, max_nodes: usize) -> Vec<(N, usize)> {
        self.bfs(start, max_depth, max_nodes, |g, n| g.successors(n).collect())
    }

    /// Breadth-first traversal following incoming edges (ancestry).
    pub fn ancestors(&self, start: N, max_depth: usize, max_nodes: usize) -> Vec<(N, usize)> {
        self.bfs(start, max_depth, max_nodes, |g, n| g.predecessors(n).collect())
    }

    fn bfs(
        &self,
        start: N,
        max_depth: usize,
        max_nodes: usize,
        next: impl Fn(&Self, N) -> Vec<N>,
    ) -> Vec<(N, usize)> {
        let mut seen = BTreeSet::from([start]);
        let mut queue = VecDeque::from([(start, 0usize)]);
        let mut result = Vec::new();
        while let Some((node, depth)) = queue.pop_front() {
            if depth == max_depth {
                continue;
            }
            for neighbour in next(self, node) {
                if result.len() >= max_nodes {
                    return result;
                }
                if seen.insert(neighbour) {
                    result.push((neighbour, depth + 1));
                    queue.push_back((neighbour, depth + 1));
                }
            }
        }
        result
    }

    /// Returns one cycle (as a node list) if the graph is not acyclic.
    pub fn find_cycle(&self) -> Option<Vec<N>> {
        #[derive(Clone, Copy, PartialEq)]
        enum Mark {
            Active,
            Done,
        }
        let mut marks: BTreeMap<N, Mark> = BTreeMap::new();
        for root in self.nodes() {
            if marks.contains_key(&root) {
                continue;
            }
            // Iterative DFS with an explicit path so deep graphs cannot overflow the stack.
            let mut stack: Vec<(N, Vec<N>)> = vec![(root, self.successors(root).collect())];
            let mut path = vec![root];
            marks.insert(root, Mark::Active);
            while let Some((_, pending)) = stack.last_mut() {
                if let Some(next) = pending.pop() {
                    match marks.get(&next) {
                        Some(Mark::Active) => {
                            let start = path.iter().position(|n| *n == next).unwrap_or(0);
                            let mut cycle = path[start..].to_vec();
                            cycle.push(next);
                            return Some(cycle);
                        }
                        Some(Mark::Done) => {}
                        None => {
                            marks.insert(next, Mark::Active);
                            path.push(next);
                            stack.push((next, self.successors(next).collect()));
                        }
                    }
                } else if let Some((done, _)) = stack.pop() {
                    marks.insert(done, Mark::Done);
                    path.pop();
                }
            }
        }
        None
    }

    /// Nodes that have no incoming edges.
    pub fn roots(&self) -> Vec<N> {
        self.inc
            .iter()
            .filter(|(_, preds)| preds.is_empty())
            .map(|(node, _)| *node)
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn traversal_respects_depth_and_node_bounds() {
        let g = Digraph::from_edges([(1, 2), (2, 3), (3, 4), (1, 5)]);
        let all: Vec<_> = g.descendants(1, 10, 100).into_iter().map(|(n, _)| n).collect();
        assert_eq!(all, vec![2, 5, 3, 4]);
        assert_eq!(g.descendants(1, 1, 100).len(), 2);
        assert_eq!(g.descendants(1, 10, 2).len(), 2);
        let up: Vec<_> = g.ancestors(4, 10, 100).into_iter().map(|(n, _)| n).collect();
        assert_eq!(up, vec![3, 2, 1]);
    }

    #[test]
    fn detects_cycles() {
        assert!(Digraph::from_edges([(1, 2), (2, 3)]).find_cycle().is_none());
        let cycle = Digraph::from_edges([(1, 2), (2, 3), (3, 1)])
            .find_cycle()
            .expect("cycle expected");
        assert_eq!(cycle.first(), cycle.last());
        assert!(cycle.len() >= 3);
    }

    #[test]
    fn deep_chain_does_not_overflow() {
        let g = Digraph::from_edges((0..200_000).map(|i| (i, i + 1)));
        assert!(g.find_cycle().is_none());
        assert_eq!(g.roots(), vec![0]);
    }
}
