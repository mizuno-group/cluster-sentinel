//! Cycle-safe traversal over the dependency graph.
//!
//! Every traversal here keeps a visited set. Operational dependency graphs do
//! contain cycles (a controller that depends on storage that is exported by a
//! host that depends on the controller's scheduler), and root-cause analysis
//! must terminate anyway (SPEC.md §28).

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet, VecDeque};

use super::{Criticality, DependencyEdge, DependencyType};
use crate::entity::{DiscoverySource, EntityId};

/// An entity reached by a traversal, with the distance it was reached at.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Reachable {
    /// The entity reached.
    pub entity: EntityId,
    /// Shortest number of edges from the traversal origin.
    pub depth: usize,
}

/// An indexed, immutable view of the dependency edges.
#[derive(Debug, Clone, Default)]
pub struct DependencyGraph {
    edges: Vec<DependencyEdge>,
    outgoing: HashMap<EntityId, Vec<usize>>,
    incoming: HashMap<EntityId, Vec<usize>>,
}

impl DependencyGraph {
    /// An empty graph.
    pub fn new() -> Self {
        Self::default()
    }

    /// Build a graph from edges.
    pub fn from_edges(edges: impl IntoIterator<Item = DependencyEdge>) -> Self {
        let mut graph = Self::new();
        for edge in edges {
            graph.insert(edge);
        }
        graph
    }

    /// Add or replace an edge (edges are keyed by their derived id).
    pub fn insert(&mut self, edge: DependencyEdge) {
        if let Some(existing) = self.edges.iter_mut().find(|e| e.id == edge.id) {
            *existing = edge;
            return;
        }
        let index = self.edges.len();
        self.outgoing.entry(edge.source).or_default().push(index);
        self.incoming.entry(edge.target).or_default().push(index);
        self.edges.push(edge);
    }

    /// All edges.
    pub fn edges(&self) -> &[DependencyEdge] {
        &self.edges
    }

    /// Number of edges.
    pub fn len(&self) -> usize {
        self.edges.len()
    }

    /// Whether the graph has no edges.
    pub fn is_empty(&self) -> bool {
        self.edges.is_empty()
    }

    /// Edges where `entity` is the dependent side.
    pub fn dependencies_of(&self, entity: EntityId) -> Vec<&DependencyEdge> {
        self.outgoing
            .get(&entity)
            .map(|ix| ix.iter().map(|&i| &self.edges[i]).collect())
            .unwrap_or_default()
    }

    /// Edges where `entity` is depended upon.
    pub fn dependents_of(&self, entity: EntityId) -> Vec<&DependencyEdge> {
        self.incoming
            .get(&entity)
            .map(|ix| ix.iter().map(|&i| &self.edges[i]).collect())
            .unwrap_or_default()
    }

    /// Everything `entity` transitively depends on, nearest first.
    ///
    /// `max_depth` bounds the walk; `None` means unbounded (still terminating,
    /// because of the visited set).
    pub fn upstream(&self, entity: EntityId, max_depth: Option<usize>) -> Vec<Reachable> {
        self.walk(entity, max_depth, Direction::Upstream, |_| true)
    }

    /// Everything that transitively depends on `entity`, nearest first. This is
    /// the blast radius used for dependency-aware severity (SPEC.md §101).
    pub fn downstream(&self, entity: EntityId, max_depth: Option<usize>) -> Vec<Reachable> {
        self.walk(entity, max_depth, Direction::Downstream, |_| true)
    }

    /// Like [`Self::upstream`], but only following edges the source genuinely
    /// needs. Optional relations do not propagate failure.
    pub fn critical_upstream(&self, entity: EntityId, max_depth: Option<usize>) -> Vec<Reachable> {
        self.walk(entity, max_depth, Direction::Upstream, |e| {
            matches!(e.criticality, Criticality::Critical | Criticality::Important)
        })
    }

    /// Entities that all of `entities` transitively depend on.
    ///
    /// This is how "the nodes behind that fileserver" is derived at runtime
    /// instead of being a hard-coded group (SPEC.md §29).
    pub fn shared_upstream(&self, entities: &[EntityId], max_depth: Option<usize>) -> BTreeSet<EntityId> {
        let mut iter = entities.iter();
        let Some(&first) = iter.next() else {
            return BTreeSet::new();
        };
        let mut shared: BTreeSet<EntityId> = self.upstream(first, max_depth).into_iter().map(|r| r.entity).collect();
        for &entity in iter {
            let next: BTreeSet<EntityId> = self.upstream(entity, max_depth).into_iter().map(|r| r.entity).collect();
            shared = shared.intersection(&next).copied().collect();
            if shared.is_empty() {
                break;
            }
        }
        shared
    }

    /// Group entities by the upstream entities they share, keeping only groups
    /// with more than one member. Used to spot "these five nodes all hang off
    /// the same storage" without naming any storage system.
    pub fn group_by_shared_upstream(
        &self,
        entities: &[EntityId],
        max_depth: Option<usize>,
    ) -> BTreeMap<EntityId, BTreeSet<EntityId>> {
        let mut groups: BTreeMap<EntityId, BTreeSet<EntityId>> = BTreeMap::new();
        for &entity in entities {
            for reachable in self.upstream(entity, max_depth) {
                groups.entry(reachable.entity).or_default().insert(entity);
            }
        }
        groups.retain(|_, members| members.len() > 1);
        groups
    }

    /// Drop every edge this source reported that is not in `keep`.
    ///
    /// A derived graph has to be able to shrink. A node that moves from one
    /// fileserver to another stops mounting the first, and an edge that only
    /// ever accumulates would keep it voting in that fileserver's failures
    /// forever -- turning a graph derived from evidence into one that merely
    /// remembers everything ever true.
    ///
    /// Scoped to one source, so a provider going quiet cannot delete another's
    /// findings.
    pub fn retain_from_source(&mut self, source: &DiscoverySource, keep: &BTreeSet<uuid::Uuid>) -> Vec<uuid::Uuid> {
        let mut dropped = Vec::new();
        self.edges.retain(|edge| {
            let keep_it = &edge.discovery_source != source || keep.contains(&edge.id);
            if !keep_it {
                dropped.push(edge.id);
            }
            keep_it
        });
        dropped
    }

    /// Edges of a given type where `entity` is the dependent side.
    pub fn dependencies_of_type(&self, entity: EntityId, dependency_type: &DependencyType) -> Vec<&DependencyEdge> {
        self.dependencies_of(entity)
            .into_iter()
            .filter(|e| &e.dependency_type == dependency_type)
            .collect()
    }

    fn walk(
        &self,
        origin: EntityId,
        max_depth: Option<usize>,
        direction: Direction,
        follow: impl Fn(&DependencyEdge) -> bool,
    ) -> Vec<Reachable> {
        let mut visited: HashSet<EntityId> = HashSet::from([origin]);
        let mut queue: VecDeque<Reachable> = VecDeque::from([Reachable {
            entity: origin,
            depth: 0,
        }]);
        let mut out = Vec::new();

        while let Some(current) = queue.pop_front() {
            if let Some(limit) = max_depth {
                if current.depth >= limit {
                    continue;
                }
            }
            let edges = match direction {
                Direction::Upstream => self.dependencies_of(current.entity),
                Direction::Downstream => self.dependents_of(current.entity),
            };
            for edge in edges {
                if !follow(edge) {
                    continue;
                }
                let next = match direction {
                    Direction::Upstream => edge.target,
                    Direction::Downstream => edge.source,
                };
                if visited.insert(next) {
                    let reachable = Reachable {
                        entity: next,
                        depth: current.depth + 1,
                    };
                    out.push(reachable);
                    queue.push_back(reachable);
                }
            }
        }
        out
    }
}

#[derive(Clone, Copy)]
enum Direction {
    Upstream,
    Downstream,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::entity::{EntityKey, EntityType};

    fn host(name: &str) -> EntityId {
        EntityKey::new("env", EntityType::Host, name).entity_id()
    }

    fn storage(name: &str) -> EntityId {
        EntityKey::new("env", EntityType::Storage, name).entity_id()
    }

    fn edge(source: EntityId, target: EntityId) -> DependencyEdge {
        DependencyEdge::new(source, target, DependencyType::DependsOn)
    }

    /// Two storage domains, each with its own clients — the shape of the
    /// initial deployment, expressed only as test data.
    fn two_domain_graph() -> DependencyGraph {
        DependencyGraph::from_edges([
            edge(host("c2"), storage("s1")),
            edge(host("c3"), storage("s1")),
            edge(storage("s1"), host("fs1")),
            edge(host("c5"), storage("s2")),
            edge(host("c6"), storage("s2")),
            edge(storage("s2"), host("fs2")),
        ])
    }

    #[test]
    fn upstream_reaches_transitively_with_depth() {
        let graph = two_domain_graph();
        let upstream = graph.upstream(host("c2"), None);
        let names: BTreeSet<_> = upstream.iter().map(|r| r.entity).collect();
        assert_eq!(names, BTreeSet::from([storage("s1"), host("fs1")]));
        assert_eq!(upstream.iter().find(|r| r.entity == storage("s1")).unwrap().depth, 1);
        assert_eq!(upstream.iter().find(|r| r.entity == host("fs1")).unwrap().depth, 2);
    }

    #[test]
    fn downstream_gives_the_blast_radius() {
        let graph = two_domain_graph();
        let downstream: BTreeSet<_> = graph
            .downstream(host("fs1"), None)
            .into_iter()
            .map(|r| r.entity)
            .collect();
        assert_eq!(downstream, BTreeSet::from([storage("s1"), host("c2"), host("c3")]));
        assert!(
            !downstream.contains(&host("c5")),
            "the other domain must not be implicated"
        );
    }

    #[test]
    fn traversal_terminates_on_a_cycle() {
        let graph = DependencyGraph::from_edges([
            edge(host("a"), host("b")),
            edge(host("b"), host("c")),
            edge(host("c"), host("a")),
        ]);
        let upstream: BTreeSet<_> = graph.upstream(host("a"), None).into_iter().map(|r| r.entity).collect();
        assert_eq!(upstream, BTreeSet::from([host("b"), host("c")]));
        assert!(!upstream.contains(&host("a")), "the origin is not its own upstream");
    }

    #[test]
    fn self_loop_terminates() {
        let graph = DependencyGraph::from_edges([edge(host("a"), host("a"))]);
        assert!(graph.upstream(host("a"), None).is_empty());
    }

    #[test]
    fn max_depth_bounds_the_walk() {
        let graph = two_domain_graph();
        let shallow: BTreeSet<_> = graph
            .upstream(host("c2"), Some(1))
            .into_iter()
            .map(|r| r.entity)
            .collect();
        assert_eq!(shallow, BTreeSet::from([storage("s1")]));
    }

    #[test]
    fn shared_upstream_derives_a_group_without_naming_it() {
        let graph = two_domain_graph();
        let shared = graph.shared_upstream(&[host("c2"), host("c3")], None);
        assert_eq!(shared, BTreeSet::from([storage("s1"), host("fs1")]));

        let cross_domain = graph.shared_upstream(&[host("c2"), host("c5")], None);
        assert!(cross_domain.is_empty(), "nodes in different domains share no upstream");
    }

    #[test]
    fn shared_upstream_of_empty_input_is_empty() {
        assert!(two_domain_graph().shared_upstream(&[], None).is_empty());
    }

    #[test]
    fn group_by_shared_upstream_keeps_only_real_groups() {
        let graph = two_domain_graph();
        let groups = graph.group_by_shared_upstream(&[host("c2"), host("c3"), host("c5")], None);
        assert_eq!(
            groups.get(&storage("s1")),
            Some(&BTreeSet::from([host("c2"), host("c3")]))
        );
        assert!(!groups.contains_key(&storage("s2")), "a single member is not a group");
    }

    #[test]
    fn critical_upstream_ignores_optional_edges() {
        let graph = DependencyGraph::from_edges([
            edge(host("a"), host("b")).with_criticality(Criticality::Optional),
            edge(host("a"), host("c")).with_criticality(Criticality::Critical),
        ]);
        let critical: BTreeSet<_> = graph
            .critical_upstream(host("a"), None)
            .into_iter()
            .map(|r| r.entity)
            .collect();
        assert_eq!(critical, BTreeSet::from([host("c")]));
    }

    #[test]
    fn reinserting_the_same_relation_updates_instead_of_duplicating() {
        let mut graph = DependencyGraph::new();
        graph.insert(edge(host("a"), host("b")));
        graph.insert(edge(host("a"), host("b")).with_criticality(Criticality::Optional));
        assert_eq!(graph.len(), 1);
        assert_eq!(graph.edges()[0].criticality, Criticality::Optional);
    }

    #[test]
    fn unknown_entities_have_no_edges() {
        let graph = two_domain_graph();
        assert!(graph.dependencies_of(host("ghost")).is_empty());
        assert!(graph.upstream(host("ghost"), None).is_empty());
    }
}
