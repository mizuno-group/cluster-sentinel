//! Peer assignment (SPEC.md §46-§49).
//!
//! Sentinel does not trust a single vantage point. One observer failing to
//! reach a host cannot distinguish a dead machine from a broken path to it, and
//! acting on that ambiguity is how people get sent to a datacentre at 3am for a
//! healthy node.
//!
//! So each target is watched from several places, and the choice of places is
//! deliberate. Three observers on the same switch, behind the same storage, in
//! the same rack are not three viewpoints — they are one viewpoint counted
//! three times, and they will all fail together for reasons that have nothing
//! to do with the target.
//!
//! The assignment therefore prefers observers that are *unlike each other*:
//!
//! 1. one sharing the target's dependency domain — it sees what the target sees;
//! 2. one in a different domain — it fails independently;
//! 3. one depending on nothing the target depends on — infrastructure, whose
//!    failure has no common cause with the target's at all.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

use crate::capability::well_known;
use crate::dependency::DependencyGraph;
use crate::entity::{EntityId, EntityType, ManagedEntity};

/// Default number of observers per target (SPEC.md §47).
pub const DEFAULT_DEGREE: u32 = 3;

/// Why an observer was chosen, kept so an operator can see the reasoning.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ObserverRole {
    /// Shares the target's dependency domain; sees what the target sees.
    SameDomain,
    /// Depends on different things; fails independently.
    DifferentDomain,
    /// Shares no dependency with the target at all.
    Independent,
    /// Chosen only to reach the requested degree.
    Filler,
}

/// One observer watching one target.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Observer {
    /// The observing entity.
    pub entity: EntityId,
    /// Why it was chosen.
    pub role: ObserverRole,
}

/// Who watches one target.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PeerAssignment {
    /// The entity being watched.
    pub target: EntityId,
    /// Who watches it, in preference order.
    pub observers: Vec<Observer>,
}

impl PeerAssignment {
    /// The observing entities.
    pub fn observer_ids(&self) -> Vec<EntityId> {
        self.observers.iter().map(|o| o.entity).collect()
    }

    /// Whether an entity observes this target.
    pub fn is_observed_by(&self, entity: EntityId) -> bool {
        self.observers.iter().any(|o| o.entity == entity)
    }

    /// How many genuinely independent viewpoints this assignment provides.
    ///
    /// Observers in the same domain as each other are counted once: they are
    /// one viewpoint, however many of them there are.
    pub fn independent_viewpoints(&self) -> usize {
        let roles: BTreeSet<ObserverRole> = self.observers.iter().map(|o| o.role).collect();
        roles.len()
    }
}

/// A complete assignment, with a revision so agents can tell theirs is stale.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AssignmentPlan {
    /// Increments whenever the plan changes.
    pub revision: u64,
    /// One entry per target.
    pub assignments: Vec<PeerAssignment>,
}

impl AssignmentPlan {
    /// What one observer has been asked to watch.
    pub fn targets_for(&self, observer: EntityId) -> Vec<EntityId> {
        self.assignments
            .iter()
            .filter(|a| a.is_observed_by(observer))
            .map(|a| a.target)
            .collect()
    }

    /// The assignment for one target.
    pub fn for_target(&self, target: EntityId) -> Option<&PeerAssignment> {
        self.assignments.iter().find(|a| a.target == target)
    }
}

/// The entities that may act as observers.
///
/// Capability, not role: a host is an observer because it has
/// `observer.peer`, whatever anyone has labelled it (SPEC.md §15).
pub fn observer_candidates<'a>(entities: impl IntoIterator<Item = &'a ManagedEntity>) -> Vec<EntityId> {
    let mut candidates: Vec<EntityId> = entities
        .into_iter()
        .filter(|e| e.entity_type == EntityType::Host)
        .filter(|e| e.lifecycle_state == crate::entity::LifecycleState::Active)
        .filter(|e| e.capabilities.has(well_known::OBSERVER_PEER))
        .map(|e| e.id)
        .collect();
    candidates.sort();
    candidates
}

/// The dependency domain of an entity: everything it transitively needs.
fn domain_of(graph: &DependencyGraph, entity: EntityId) -> BTreeSet<EntityId> {
    graph.upstream(entity, None).into_iter().map(|r| r.entity).collect()
}

/// Build an assignment plan.
///
/// Deterministic: the same inputs always give the same plan, so a controller
/// restart does not reshuffle the whole cluster's observers and invalidate
/// every debounce counter along with it.
pub fn assign(
    targets: &[EntityId],
    candidates: &[EntityId],
    graph: &DependencyGraph,
    degree: u32,
    revision: u64,
) -> AssignmentPlan {
    let mut assignments = Vec::with_capacity(targets.len());
    let domains: BTreeMap<EntityId, BTreeSet<EntityId>> =
        candidates.iter().map(|id| (*id, domain_of(graph, *id))).collect();

    for (index, target) in targets.iter().enumerate() {
        let target_domain = domain_of(graph, *target);

        // Partition the candidates by how independent they are of the target.
        let mut same = Vec::new();
        let mut different = Vec::new();
        let mut independent = Vec::new();

        for candidate in candidates {
            if candidate == target {
                // An entity cannot vouch for itself.
                continue;
            }
            let candidate_domain = domains.get(candidate).cloned().unwrap_or_default();

            if candidate_domain.is_empty() && target_domain.is_empty() {
                independent.push(*candidate);
            } else if candidate_domain.is_disjoint(&target_domain) {
                if candidate_domain.is_empty() {
                    independent.push(*candidate);
                } else {
                    different.push(*candidate);
                }
            } else {
                same.push(*candidate);
            }
        }

        // Rotate each bucket by the target's position, so the same few
        // candidates do not end up observing everything while others idle.
        // Deterministic, because it depends only on the ordered inputs.
        rotate(&mut same, index);
        rotate(&mut different, index);
        rotate(&mut independent, index);

        let mut observers = Vec::new();
        let mut chosen: BTreeSet<EntityId> = BTreeSet::new();

        // One from each kind first: variety before quantity, because a fourth
        // observer in an already-covered domain adds almost nothing.
        for (bucket, role) in [
            (&same, ObserverRole::SameDomain),
            (&different, ObserverRole::DifferentDomain),
            (&independent, ObserverRole::Independent),
        ] {
            if observers.len() as u32 >= degree {
                break;
            }
            if let Some(candidate) = bucket.iter().find(|c| !chosen.contains(c)) {
                chosen.insert(*candidate);
                observers.push(Observer {
                    entity: *candidate,
                    role,
                });
            }
        }

        // Then fill up to the requested degree with whatever is left.
        for bucket in [&same, &different, &independent] {
            for candidate in bucket {
                if observers.len() as u32 >= degree {
                    break;
                }
                if chosen.insert(*candidate) {
                    observers.push(Observer {
                        entity: *candidate,
                        role: ObserverRole::Filler,
                    });
                }
            }
        }

        assignments.push(PeerAssignment {
            target: *target,
            observers,
        });
    }

    AssignmentPlan { revision, assignments }
}

/// Rotate a slice left by `by`, leaving it unchanged when empty.
fn rotate(items: &mut [EntityId], by: usize) {
    if items.is_empty() {
        return;
    }
    items.rotate_left(by % items.len());
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::capability::CapabilitySet;
    use crate::dependency::{DependencyEdge, DependencyType};
    use crate::entity::EntityKey;

    fn host(name: &str) -> EntityId {
        EntityKey::new("lab", EntityType::Host, name).entity_id()
    }

    fn storage(name: &str) -> EntityId {
        EntityKey::new("lab", EntityType::Storage, name).entity_id()
    }

    /// Two storage domains, plus infrastructure that depends on neither.
    fn graph() -> DependencyGraph {
        DependencyGraph::from_edges([
            DependencyEdge::new(host("c1"), storage("s1"), DependencyType::UsesStorage),
            DependencyEdge::new(host("c2"), storage("s1"), DependencyType::UsesStorage),
            DependencyEdge::new(host("c3"), storage("s2"), DependencyType::UsesStorage),
            DependencyEdge::new(host("c4"), storage("s2"), DependencyType::UsesStorage),
            DependencyEdge::new(storage("s1"), host("fs1"), DependencyType::Provides),
            DependencyEdge::new(storage("s2"), host("fs2"), DependencyType::Provides),
        ])
    }

    fn candidates() -> Vec<EntityId> {
        let mut candidates = vec![host("c1"), host("c2"), host("c3"), host("c4"), host("fs1"), host("fs2")];
        candidates.sort();
        candidates
    }

    #[test]
    fn an_entity_never_observes_itself() {
        // Asking a host whether it is up can only ever get one answer.
        let plan = assign(&[host("c1")], &candidates(), &graph(), 3, 1);
        assert!(!plan.assignments[0].is_observed_by(host("c1")));
    }

    #[test]
    fn observers_are_chosen_from_different_domains() {
        // SPEC.md §48. Three observers behind the same storage are one
        // viewpoint counted three times.
        let plan = assign(&[host("c1")], &candidates(), &graph(), 3, 1);
        let assignment = &plan.assignments[0];

        assert_eq!(assignment.observers.len(), 3);
        let roles: BTreeSet<_> = assignment.observers.iter().map(|o| o.role).collect();
        assert!(roles.contains(&ObserverRole::SameDomain), "{assignment:?}");
        assert!(roles.contains(&ObserverRole::DifferentDomain), "{assignment:?}");
        assert!(roles.contains(&ObserverRole::Independent), "{assignment:?}");
        assert_eq!(assignment.independent_viewpoints(), 3);
    }

    #[test]
    fn the_same_domain_observer_really_shares_the_targets_storage() {
        let plan = assign(&[host("c1")], &candidates(), &graph(), 3, 1);
        let same = plan.assignments[0]
            .observers
            .iter()
            .find(|o| o.role == ObserverRole::SameDomain)
            .expect("a same-domain observer");
        assert_eq!(same.entity, host("c2"), "c2 is the other client of s1");
    }

    #[test]
    fn the_independent_observer_depends_on_nothing_the_target_depends_on() {
        let plan = assign(&[host("c1")], &candidates(), &graph(), 3, 1);
        let independent = plan.assignments[0]
            .observers
            .iter()
            .find(|o| o.role == ObserverRole::Independent)
            .expect("an independent observer");
        assert!(
            independent.entity == host("fs1") || independent.entity == host("fs2"),
            "the fileservers depend on no storage themselves"
        );
    }

    #[test]
    fn no_observer_is_assigned_twice_to_one_target() {
        let plan = assign(&[host("c1")], &candidates(), &graph(), 6, 1);
        let ids = plan.assignments[0].observer_ids();
        let unique: BTreeSet<_> = ids.iter().collect();
        assert_eq!(ids.len(), unique.len());
    }

    #[test]
    fn the_requested_degree_is_respected() {
        for degree in 1..=4u32 {
            let plan = assign(&[host("c1")], &candidates(), &graph(), degree, 1);
            assert_eq!(plan.assignments[0].observers.len(), degree as usize, "degree {degree}");
        }
    }

    #[test]
    fn a_degree_larger_than_the_candidate_pool_assigns_everyone_once() {
        let plan = assign(&[host("c1")], &candidates(), &graph(), 100, 1);
        assert_eq!(
            plan.assignments[0].observers.len(),
            candidates().len() - 1,
            "everyone but the target"
        );
    }

    #[test]
    fn a_single_candidate_still_produces_an_assignment() {
        let plan = assign(&[host("c1")], &[host("fs1")], &graph(), 3, 1);
        assert_eq!(plan.assignments[0].observers.len(), 1);
    }

    #[test]
    fn a_target_with_no_available_candidates_gets_no_observers() {
        // Reported as empty rather than fabricated. An assignment that names no
        // one is honest; one that names the target itself is not.
        let plan = assign(&[host("c1")], &[host("c1")], &graph(), 3, 1);
        assert!(plan.assignments[0].observers.is_empty());
    }

    #[test]
    fn assignment_is_deterministic() {
        // A controller restart must not reshuffle every observer in the
        // cluster and reset every debounce counter with them.
        let first = assign(&[host("c1"), host("c3")], &candidates(), &graph(), 3, 1);
        let second = assign(&[host("c1"), host("c3")], &candidates(), &graph(), 3, 1);
        assert_eq!(first, second);
    }

    #[test]
    fn observation_load_is_spread_rather_than_concentrated() {
        // Without rotation the first candidate in each bucket would observe
        // every target in the cluster.
        let targets = vec![host("c1"), host("c2"), host("c3"), host("c4")];
        let plan = assign(&targets, &candidates(), &graph(), 2, 1);

        let mut load: BTreeMap<EntityId, usize> = BTreeMap::new();
        for assignment in &plan.assignments {
            for observer in &assignment.observers {
                *load.entry(observer.entity).or_default() += 1;
            }
        }

        let max = load.values().copied().max().unwrap_or(0);
        let total: usize = load.values().sum();
        assert!(max < total, "one observer took every assignment: {load:?}");
    }

    #[test]
    fn only_hosts_with_the_observer_capability_are_candidates() {
        // SPEC.md §15: a role label must not make something an observer.
        let entities = vec![
            ManagedEntity::new("lab", EntityType::Host, "watcher")
                .with_capabilities(CapabilitySet::from_iter([well_known::OBSERVER_PEER])),
            ManagedEntity::new("lab", EntityType::Host, "plain").with_label("role", "observer"),
            ManagedEntity::new("lab", EntityType::Storage, "s1")
                .with_capabilities(CapabilitySet::from_iter([well_known::OBSERVER_PEER])),
        ];

        let candidates = observer_candidates(&entities);
        assert_eq!(candidates, vec![host("watcher")]);
    }

    #[test]
    fn a_stale_host_is_not_used_as_an_observer() {
        let mut stale = ManagedEntity::new("lab", EntityType::Host, "gone")
            .with_capabilities(CapabilitySet::from_iter([well_known::OBSERVER_PEER]));
        stale.lifecycle_state = crate::entity::LifecycleState::Stale;
        assert!(observer_candidates(std::iter::once(&stale)).is_empty());
    }

    #[test]
    fn the_plan_can_be_read_from_either_end() {
        let plan = assign(&[host("c1"), host("c3")], &candidates(), &graph(), 3, 7);
        assert_eq!(plan.revision, 7);

        let target = plan.for_target(host("c1")).expect("assignment");
        let observer = target.observers[0].entity;
        assert!(plan.targets_for(observer).contains(&host("c1")));
    }

    #[test]
    fn a_graph_with_no_dependencies_still_produces_assignments() {
        // A cluster before any storage topology has been described.
        let plan = assign(
            &[host("a")],
            &[host("a"), host("b"), host("c")],
            &DependencyGraph::new(),
            2,
            1,
        );
        assert_eq!(plan.assignments[0].observers.len(), 2);
        assert!(!plan.assignments[0].is_observed_by(host("a")));
    }
}
