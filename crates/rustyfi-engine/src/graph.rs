/// Module Dependency DAG and topological translation scheduler.
///
/// The core insight: a large codebase is not a flat list of files.
/// It is a directed graph where edges encode "must translate before" relationships.
/// Scheduling translation in topological order means:
///   - When translating file A, all files A imports already have Rust signatures.
///   - The LLM receives concrete Rust context instead of guessing at types.
///   - Repair is local — a broken interface propagates only to files above it.
use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};

use rustyfi_core::context::DependencyEdge;
use serde::{Deserialize, Serialize};
use tracing::{debug, warn};

// ---------------------------------------------------------------------------
// Edge record (serialisable subset of DependencyEdge)
// ---------------------------------------------------------------------------

/// A slimmed-down dependency edge that can be persisted in checkpoints.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EdgeRecord {
    /// The file that imports.
    pub from: PathBuf,
    /// The file being imported.
    pub to: PathBuf,
    /// Raw import symbol (for diagnostics).
    pub import_symbol: String,
}

impl From<&DependencyEdge> for EdgeRecord {
    fn from(e: &DependencyEdge) -> Self {
        Self {
            from: e.from.clone(),
            to: e.to.clone(),
            import_symbol: e.import_symbol.clone(),
        }
    }
}

// ---------------------------------------------------------------------------
// ModuleGraph
// ---------------------------------------------------------------------------

/// Directed Acyclic Graph of source-file dependencies.
///
/// Edges run `importer → imported` (i.e., `from` depends on `to`).
/// [`translation_order`] returns files in topological order so that every
/// file's dependencies have been translated before the file itself.
pub struct ModuleGraph {
    /// All node paths, in insertion order.
    nodes: Vec<PathBuf>,
    /// `adj[i]` = indices of files that `nodes[i]` **imports** (its deps).
    adj: Vec<Vec<usize>>,
    /// Reverse map: path → node index.
    index: HashMap<PathBuf, usize>,
}

impl ModuleGraph {
    /// Build a graph from a target list and dependency edges.
    ///
    /// Files referenced in edges but absent from `targets` are added as
    /// implicit nodes (external or inferred files).
    pub fn build(targets: &[PathBuf], edges: &[EdgeRecord]) -> Self {
        let mut nodes: Vec<PathBuf> = targets.to_vec();
        let mut index: HashMap<PathBuf, usize> = nodes
            .iter()
            .enumerate()
            .map(|(i, p)| (p.clone(), i))
            .collect();

        // Materialise any referenced path not already in the target list.
        for e in edges {
            for path in [&e.from, &e.to] {
                if !index.contains_key(path) {
                    let i = nodes.len();
                    index.insert(path.clone(), i);
                    nodes.push(path.clone());
                }
            }
        }

        let n = nodes.len();
        let mut adj: Vec<Vec<usize>> = vec![vec![]; n];

        for e in edges {
            // Only track internal edges (both ends in the graph).
            if let (Some(&from_idx), Some(&to_idx)) = (index.get(&e.from), index.get(&e.to)) {
                // Deduplicate edges.
                if !adj[from_idx].contains(&to_idx) {
                    adj[from_idx].push(to_idx);
                }
            }
        }

        Self { nodes, adj, index }
    }

    /// Topological order: dependencies translated **before** their importers.
    ///
    /// Uses Kahn's BFS algorithm.  If cycles are detected (which can happen
    /// in Python/JS circular imports), the remaining nodes are appended in
    /// arbitrary order with a warning rather than failing the run.
    pub fn translation_order(&self) -> Vec<PathBuf> {
        let n = self.nodes.len();

        // in_degree[i] = number of unprocessed dependencies of node i.
        let mut in_degree: Vec<usize> = self.adj.iter().map(|d| d.len()).collect();

        // rev[j] = list of importers of node j (nodes that have j as a dep).
        let mut rev: Vec<Vec<usize>> = vec![vec![]; n];
        for (importer, deps) in self.adj.iter().enumerate() {
            for &dep in deps {
                rev[dep].push(importer);
            }
        }

        // Seed queue with files that have no dependencies (pure leaf files).
        let mut queue: VecDeque<usize> = (0..n).filter(|&i| in_degree[i] == 0).collect();
        let mut order: Vec<PathBuf> = Vec::with_capacity(n);

        while let Some(node) = queue.pop_front() {
            order.push(self.nodes[node].clone());
            // When node is "done", reduce in_degree of all its importers.
            for &importer in &rev[node] {
                in_degree[importer] = in_degree[importer].saturating_sub(1);
                if in_degree[importer] == 0 {
                    queue.push_back(importer);
                }
            }
        }

        // Handle cycles by appending stragglers.
        if order.len() < n {
            let scheduled: HashSet<&PathBuf> = order.iter().collect();
            let stragglers: Vec<&PathBuf> = self
                .nodes
                .iter()
                .filter(|p| !scheduled.contains(p))
                .collect();
            warn!(
                "ModuleGraph: {} node(s) in a cycle — scheduling arbitrarily: {:?}",
                stragglers.len(),
                stragglers.iter().take(3).collect::<Vec<_>>()
            );
            for p in stragglers {
                order.push(p.clone());
            }
        }

        debug!("Translation order: {} files", order.len());
        order
    }

    /// Return the direct dependencies (imported files) of `path`.
    pub fn deps_of(&self, path: &Path) -> Vec<&PathBuf> {
        match self.index.get(path) {
            Some(&idx) => self.adj[idx].iter().map(|&i| &self.nodes[i]).collect(),
            None => vec![],
        }
    }

    /// Return the files that directly import `path`.
    pub fn importers_of(&self, path: &Path) -> Vec<&PathBuf> {
        let Some(&target_idx) = self.index.get(path) else {
            return vec![];
        };
        self.nodes
            .iter()
            .enumerate()
            .filter(|(i, _)| self.adj[*i].contains(&target_idx))
            .map(|(_, p)| p)
            .collect()
    }

    /// Total number of nodes.
    pub fn len(&self) -> usize {
        self.nodes.len()
    }

    pub fn is_empty(&self) -> bool {
        self.nodes.is_empty()
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn p(s: &str) -> PathBuf {
        PathBuf::from(s)
    }

    fn edge(from: &str, to: &str) -> EdgeRecord {
        EdgeRecord {
            from: p(from),
            to: p(to),
            import_symbol: "mod".into(),
        }
    }

    #[test]
    fn empty_graph_empty_order() {
        let g = ModuleGraph::build(&[], &[]);
        assert!(g.translation_order().is_empty());
    }

    #[test]
    fn single_file_no_edges() {
        let g = ModuleGraph::build(&[p("main.py")], &[]);
        assert_eq!(g.translation_order(), vec![p("main.py")]);
    }

    #[test]
    fn chain_translates_deps_first() {
        // a.py → b.py → c.py  (a imports b, b imports c)
        let targets = vec![p("a.py"), p("b.py"), p("c.py")];
        let edges = vec![edge("a.py", "b.py"), edge("b.py", "c.py")];
        let g = ModuleGraph::build(&targets, &edges);
        let order = g.translation_order();
        // c must come before b, b before a.
        let pos = |f: &str| order.iter().position(|p| p == Path::new(f)).unwrap();
        assert!(pos("c.py") < pos("b.py"));
        assert!(pos("b.py") < pos("a.py"));
    }

    #[test]
    fn diamond_dependency_order() {
        // main → a → lib, main → b → lib
        let targets = vec![p("main.py"), p("a.py"), p("b.py"), p("lib.py")];
        let edges = vec![
            edge("main.py", "a.py"),
            edge("main.py", "b.py"),
            edge("a.py", "lib.py"),
            edge("b.py", "lib.py"),
        ];
        let g = ModuleGraph::build(&targets, &edges);
        let order = g.translation_order();
        let pos = |f: &str| order.iter().position(|p| p == Path::new(f)).unwrap();
        assert!(pos("lib.py") < pos("a.py"));
        assert!(pos("lib.py") < pos("b.py"));
        assert!(pos("a.py") < pos("main.py"));
        assert!(pos("b.py") < pos("main.py"));
    }

    #[test]
    fn cycle_does_not_panic() {
        // a → b → a  (cycle)
        let targets = vec![p("a.py"), p("b.py")];
        let edges = vec![edge("a.py", "b.py"), edge("b.py", "a.py")];
        let g = ModuleGraph::build(&targets, &edges);
        let order = g.translation_order();
        // Should return both files without panicking.
        assert_eq!(order.len(), 2);
    }

    #[test]
    fn deps_of_returns_correct_imports() {
        let targets = vec![p("a.py"), p("b.py"), p("c.py")];
        let edges = vec![edge("a.py", "b.py"), edge("a.py", "c.py")];
        let g = ModuleGraph::build(&targets, &edges);
        let mut deps: Vec<_> = g.deps_of(Path::new("a.py")).into_iter().cloned().collect();
        deps.sort();
        assert_eq!(deps, vec![p("b.py"), p("c.py")]);
    }
}
