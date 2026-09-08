//! Merging snapshots from several providers into one inventory.
//!
//! The merge rules exist to answer one question well: *what happened to this
//! entity between then and now?* Everything here is written so that the answer
//! survives providers disagreeing, providers vanishing, and infrastructure
//! being reorganised.

use std::collections::{BTreeMap, BTreeSet};

use crate::dependency::{DependencyEdge, DependencyGraph};
use crate::entity::{DiscoverySource, EntityId, EntityType, LifecycleState, ManagedEntity};
use crate::time::now;

use super::InventorySnapshot;

/// What a merge changed, for logging and for tests.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MergeOutcome {
    /// Entities seen for the first time.
    pub added: BTreeSet<EntityId>,
    /// Entities that were already known and were refreshed.
    pub updated: BTreeSet<EntityId>,
    /// Entities marked stale because nothing reports them any more.
    pub marked_stale: BTreeSet<EntityId>,
    /// Entities that came back after being stale.
    pub revived: BTreeSet<EntityId>,
    /// Dependency edges added or refreshed.
    pub dependencies_touched: usize,
    /// Edges held back because an endpoint has not been discovered yet.
    pub dependencies_pending: usize,
}

impl MergeOutcome {
    /// Whether anything changed.
    pub fn is_empty(&self) -> bool {
        self.added.is_empty()
            && self.updated.is_empty()
            && self.marked_stale.is_empty()
            && self.revived.is_empty()
            && self.dependencies_touched == 0
            && self.dependencies_pending == 0
    }
}

/// The merged view of every provider's discoveries.
#[derive(Debug, Clone, Default)]
pub struct Inventory {
    entities: BTreeMap<EntityId, ManagedEntity>,
    graph: DependencyGraph,
    /// Edges whose endpoints are not (yet) known.
    ///
    /// Providers run in sequence, so a statically declared dependency may name
    /// a host that a later provider will discover. Dropping such an edge would
    /// make the graph depend on provider ordering — a bug that hides until the
    /// second cycle, when the entity is already in the database.
    pending: Vec<DependencyEdge>,
}

impl Inventory {
    /// An empty inventory.
    pub fn new() -> Self {
        Self::default()
    }

    /// Fold a provider's snapshot in.
    pub fn merge(&mut self, snapshot: &InventorySnapshot) -> MergeOutcome {
        let mut outcome = MergeOutcome::default();

        for incoming in &snapshot.entities {
            match self.entities.get_mut(&incoming.id) {
                Some(existing) => {
                    if existing.lifecycle_state == LifecycleState::Stale {
                        outcome.revived.insert(existing.id);
                    }
                    merge_into(existing, incoming);
                    outcome.updated.insert(existing.id);
                }
                None => {
                    self.entities.insert(incoming.id, incoming.clone());
                    outcome.added.insert(incoming.id);
                }
            }
        }

        for edge in &snapshot.dependencies {
            self.offer_edge(edge.clone(), &mut outcome);
        }

        // Entities this snapshot introduced may complete edges an earlier
        // provider offered.
        self.resolve_pending(&mut outcome);
        outcome.dependencies_pending = self.pending.len();
        outcome
    }

    /// Add an edge if both endpoints are known, else hold it back.
    fn offer_edge(&mut self, mut edge: DependencyEdge, outcome: &mut MergeOutcome) {
        if !self.entities.contains_key(&edge.source) || !self.entities.contains_key(&edge.target) {
            if !self.pending.iter().any(|e| e.id == edge.id) {
                self.pending.push(edge);
            }
            return;
        }
        if let Some(existing) = self.graph.edges().iter().find(|e| e.id == edge.id) {
            edge.first_seen_at = existing.first_seen_at;
        }
        edge.last_seen_at = now();
        self.graph.insert(edge);
        outcome.dependencies_touched += 1;
    }

    /// Retry held-back edges whose endpoints have since appeared.
    fn resolve_pending(&mut self, outcome: &mut MergeOutcome) {
        loop {
            let ready: Vec<DependencyEdge> = self
                .pending
                .iter()
                .filter(|e| self.entities.contains_key(&e.source) && self.entities.contains_key(&e.target))
                .cloned()
                .collect();
            if ready.is_empty() {
                return;
            }
            self.pending.retain(|e| !ready.iter().any(|r| r.id == e.id));
            for edge in ready {
                self.offer_edge(edge, outcome);
            }
        }
    }

    /// Edges still waiting for an endpoint to be discovered.
    ///
    /// A non-empty list after a full cycle means configuration refers to
    /// something no provider reports, which `sentinel config check` warns about.
    pub fn pending_dependencies(&self) -> &[DependencyEdge] {
        &self.pending
    }

    /// Mark every entity previously reported by `source`, but absent from
    /// `snapshot`, as [`LifecycleState::Stale`].
    ///
    /// Entities another provider still reports stay active: a host disappearing
    /// from Slurm has not disappeared from the building (SPEC.md §31).
    pub fn mark_absent_as_stale(&mut self, source: &DiscoverySource, snapshot: &InventorySnapshot) -> MergeOutcome {
        let seen: BTreeSet<EntityId> = snapshot.entities.iter().map(|e| e.id).collect();
        let mut outcome = MergeOutcome::default();

        for entity in self.entities.values_mut() {
            let reported_by_this_source = entity.discovery_sources.contains(source);
            let reported_by_another = entity.discovery_sources.iter().any(|s| s != source);

            if reported_by_this_source
                && !reported_by_another
                && !seen.contains(&entity.id)
                && entity.lifecycle_state == LifecycleState::Active
            {
                entity.lifecycle_state = LifecycleState::Stale;
                entity.updated_at = now();
                outcome.marked_stale.insert(entity.id);
            }
        }

        outcome
    }

    /// Insert an entity directly, replacing any entity with the same id.
    ///
    /// Used when rehydrating from the database, where the merge rules have
    /// already been applied and re-running them would reset timestamps.
    pub fn insert_entity(&mut self, entity: ManagedEntity) {
        self.entities.insert(entity.id, entity);
    }

    /// Insert a dependency edge directly, for rehydration.
    ///
    /// Unlike [`Inventory::merge`], this does not drop edges whose endpoints
    /// are unknown: the caller is expected to load entities first.
    pub fn insert_dependency(&mut self, edge: DependencyEdge) {
        self.graph.insert(edge);
    }

    /// Look an entity up.
    pub fn get(&self, id: EntityId) -> Option<&ManagedEntity> {
        self.entities.get(&id)
    }

    /// Find an entity by its natural key.
    pub fn find(&self, environment: &str, entity_type: EntityType, canonical_name: &str) -> Option<&ManagedEntity> {
        let id = crate::entity::EntityKey::new(environment, entity_type, canonical_name).entity_id();
        self.get(id)
    }

    /// Every entity, in id order.
    pub fn entities(&self) -> impl Iterator<Item = &ManagedEntity> {
        self.entities.values()
    }

    /// Entities of one type.
    pub fn entities_of_type(&self, entity_type: EntityType) -> impl Iterator<Item = &ManagedEntity> {
        self.entities.values().filter(move |e| e.entity_type == entity_type)
    }

    /// Entities that are still being reported.
    pub fn active_entities(&self) -> impl Iterator<Item = &ManagedEntity> {
        self.entities
            .values()
            .filter(|e| e.lifecycle_state == LifecycleState::Active)
    }

    /// The dependency graph.
    pub fn graph(&self) -> &DependencyGraph {
        &self.graph
    }

    /// Number of known entities, whatever their lifecycle state.
    pub fn len(&self) -> usize {
        self.entities.len()
    }

    /// Whether the inventory is empty.
    pub fn is_empty(&self) -> bool {
        self.entities.is_empty()
    }
}

/// Fold `incoming` into `existing`.
///
/// Union rather than replace, because two providers describe different
/// *aspects* of the same thing: Slurm knows a node's partitions, an agent knows
/// its GPUs, and configuration knows which rack it is in. Whoever reports last
/// must not erase the others.
fn merge_into(existing: &mut ManagedEntity, incoming: &ManagedEntity) {
    for capability in incoming.capabilities.iter() {
        existing.capabilities.insert(capability.clone());
    }
    for (key, value) in &incoming.labels {
        existing.labels.insert(key.clone(), value.clone());
    }
    for source in &incoming.discovery_sources {
        if !existing.discovery_sources.contains(source) {
            existing.discovery_sources.push(source.clone());
        }
    }
    if let Some(cluster) = &incoming.cluster {
        existing.cluster = Some(cluster.clone());
    }
    if incoming.display_name != incoming.canonical_name {
        existing.display_name = incoming.display_name.clone();
    }
    merge_metadata(&mut existing.metadata, &incoming.metadata);

    existing.lifecycle_state = LifecycleState::Active;
    existing.updated_at = now();
}

/// Shallow-merge JSON objects, keeping keys the incoming side does not mention.
fn merge_metadata(existing: &mut serde_json::Value, incoming: &serde_json::Value) {
    match (existing, incoming) {
        (serde_json::Value::Object(existing), serde_json::Value::Object(incoming)) => {
            for (key, value) in incoming {
                existing.insert(key.clone(), value.clone());
            }
        }
        (existing, incoming) if !incoming.is_null() => *existing = incoming.clone(),
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::capability::CapabilitySet;
    use crate::dependency::DependencyType;

    fn host(name: &str) -> ManagedEntity {
        ManagedEntity::new("lab", EntityType::Host, name)
    }

    fn snapshot_from(source: DiscoverySource, entities: Vec<ManagedEntity>) -> InventorySnapshot {
        let mut snapshot = InventorySnapshot::new(source);
        for entity in entities {
            snapshot.add_entity(entity);
        }
        snapshot
    }

    #[test]
    fn a_first_snapshot_adds_everything() {
        let mut inventory = Inventory::new();
        let outcome = inventory.merge(&snapshot_from(
            DiscoverySource::StaticConfig,
            vec![host("a"), host("b")],
        ));
        assert_eq!(outcome.added.len(), 2);
        assert_eq!(inventory.len(), 2);
    }

    #[test]
    fn the_same_host_from_two_providers_is_one_entity_with_both_origins() {
        let mut inventory = Inventory::new();
        inventory.merge(&snapshot_from(DiscoverySource::StaticConfig, vec![host("a")]));
        inventory.merge(&snapshot_from(
            DiscoverySource::Integration("slurm".into()),
            vec![host("a")],
        ));

        assert_eq!(inventory.len(), 1, "one host, not two");
        let entity = inventory.find("lab", EntityType::Host, "a").expect("found");
        assert_eq!(entity.discovery_sources.len(), 2);
        assert!(entity.discovery_sources.contains(&DiscoverySource::StaticConfig));
    }

    #[test]
    fn capabilities_from_different_providers_are_unioned_not_replaced() {
        // Slurm knows it runs slurmd; the agent knows it has GPUs. Both are true.
        let mut inventory = Inventory::new();
        let from_slurm = host("a").with_capabilities(CapabilitySet::from_iter(["slurm.compute"]));
        let from_agent = host("a").with_capabilities(CapabilitySet::from_iter(["gpu.nvidia", "host.metrics"]));

        inventory.merge(&snapshot_from(
            DiscoverySource::Integration("slurm".into()),
            vec![from_slurm],
        ));
        inventory.merge(&snapshot_from(DiscoverySource::AgentRegistration, vec![from_agent]));

        let entity = inventory.find("lab", EntityType::Host, "a").expect("found");
        assert!(entity.capabilities.has("slurm.compute"));
        assert!(entity.capabilities.has("gpu.nvidia"));
        assert!(entity.capabilities.has("host.metrics"));
    }

    #[test]
    fn labels_and_metadata_merge_key_by_key() {
        let mut inventory = Inventory::new();
        let mut first = host("a").with_label("rack", "r01");
        first.metadata = serde_json::json!({"os": "linux"});
        let mut second = host("a").with_label("owner", "lab");
        second.metadata = serde_json::json!({"kernel": "6.1"});

        inventory.merge(&snapshot_from(DiscoverySource::StaticConfig, vec![first]));
        inventory.merge(&snapshot_from(DiscoverySource::AgentRegistration, vec![second]));

        let entity = inventory.find("lab", EntityType::Host, "a").expect("found");
        assert_eq!(entity.labels.get("rack").unwrap(), "r01");
        assert_eq!(entity.labels.get("owner").unwrap(), "lab");
        assert_eq!(entity.metadata["os"], "linux");
        assert_eq!(entity.metadata["kernel"], "6.1");
    }

    #[test]
    fn a_host_that_leaves_slurm_becomes_stale_rather_than_disappearing() {
        // SPEC.md §31: removal from Slurm is not removal from the building.
        let slurm = DiscoverySource::Integration("slurm".into());
        let mut inventory = Inventory::new();
        inventory.merge(&snapshot_from(slurm.clone(), vec![host("a"), host("b")]));

        let next = snapshot_from(slurm.clone(), vec![host("a")]);
        inventory.merge(&next);
        let outcome = inventory.mark_absent_as_stale(&slurm, &next);

        assert_eq!(outcome.marked_stale.len(), 1);
        let gone = inventory.find("lab", EntityType::Host, "b").expect("still present");
        assert_eq!(gone.lifecycle_state, LifecycleState::Stale);
        assert_eq!(inventory.len(), 2, "history is retained");
        assert_eq!(inventory.active_entities().count(), 1);
    }

    #[test]
    fn an_entity_another_provider_still_reports_does_not_go_stale() {
        let slurm = DiscoverySource::Integration("slurm".into());
        let mut inventory = Inventory::new();
        inventory.merge(&snapshot_from(slurm.clone(), vec![host("a")]));
        inventory.merge(&snapshot_from(DiscoverySource::StaticConfig, vec![host("a")]));

        let empty = InventorySnapshot::new(slurm.clone());
        inventory.merge(&empty);
        let outcome = inventory.mark_absent_as_stale(&slurm, &empty);

        assert!(outcome.marked_stale.is_empty(), "static config still vouches for it");
        assert_eq!(
            inventory.find("lab", EntityType::Host, "a").unwrap().lifecycle_state,
            LifecycleState::Active
        );
    }

    #[test]
    fn a_returning_entity_is_revived_rather_than_duplicated() {
        let slurm = DiscoverySource::Integration("slurm".into());
        let mut inventory = Inventory::new();
        inventory.merge(&snapshot_from(slurm.clone(), vec![host("a")]));

        let empty = InventorySnapshot::new(slurm.clone());
        inventory.mark_absent_as_stale(&slurm, &empty);
        assert_eq!(inventory.active_entities().count(), 0);

        let returned = snapshot_from(slurm.clone(), vec![host("a")]);
        let outcome = inventory.merge(&returned);

        assert_eq!(outcome.revived.len(), 1);
        assert_eq!(inventory.len(), 1);
        assert_eq!(inventory.active_entities().count(), 1);
    }

    #[test]
    fn a_dependency_between_known_entities_lands_in_the_graph() {
        let mut inventory = Inventory::new();
        let mut snapshot = InventorySnapshot::new(DiscoverySource::StaticConfig);
        let (a, b) = (host("a"), ManagedEntity::new("lab", EntityType::Storage, "s"));
        let (a_id, b_id) = (a.id, b.id);
        snapshot.add_entity(a);
        snapshot.add_entity(b);
        snapshot.add_dependency(DependencyEdge::new(a_id, b_id, DependencyType::UsesStorage));

        let outcome = inventory.merge(&snapshot);
        assert_eq!(outcome.dependencies_touched, 1);
        assert_eq!(inventory.graph().dependencies_of(a_id).len(), 1);
    }

    #[test]
    fn a_dependency_naming_an_unknown_entity_stays_out_of_the_graph() {
        let mut inventory = Inventory::new();
        let mut snapshot = InventorySnapshot::new(DiscoverySource::StaticConfig);
        let a = host("a");
        let a_id = a.id;
        let phantom = ManagedEntity::new("lab", EntityType::Storage, "never-reported").id;
        snapshot.add_entity(a);
        snapshot.add_dependency(DependencyEdge::new(a_id, phantom, DependencyType::UsesStorage));

        let outcome = inventory.merge(&snapshot);
        assert_eq!(outcome.dependencies_touched, 0);
        assert!(inventory.graph().is_empty(), "the graph must never reference a phantom");
        assert_eq!(
            outcome.dependencies_pending, 1,
            "but the edge is remembered, not discarded"
        );
    }

    #[test]
    fn an_edge_naming_a_later_discovered_entity_resolves_when_it_arrives() {
        // Providers run in sequence: static configuration may legitimately
        // depend on a host Slurm has not been asked about yet. Dropping the
        // edge would make the graph depend on provider ordering.
        let mut inventory = Inventory::new();

        let mut from_config = InventorySnapshot::new(DiscoverySource::StaticConfig);
        let storage = ManagedEntity::new("lab", EntityType::Storage, "shared");
        let (storage_id, node_id) = (storage.id, host("discovered-later").id);
        from_config.add_entity(storage);
        from_config.add_dependency(DependencyEdge::new(node_id, storage_id, DependencyType::UsesStorage));

        let outcome = inventory.merge(&from_config);
        assert_eq!(outcome.dependencies_touched, 0);
        assert_eq!(inventory.pending_dependencies().len(), 1);

        let mut from_slurm = InventorySnapshot::new(DiscoverySource::Integration("slurm".into()));
        from_slurm.add_entity(host("discovered-later"));
        let outcome = inventory.merge(&from_slurm);

        assert_eq!(outcome.dependencies_touched, 1, "the held-back edge lands now");
        assert_eq!(inventory.graph().len(), 1);
        assert!(inventory.pending_dependencies().is_empty());
    }

    #[test]
    fn a_chain_of_held_back_edges_resolves_in_one_pass() {
        let mut inventory = Inventory::new();
        let (a, b, c) = (host("a"), host("b"), host("c"));
        let (a_id, b_id, c_id) = (a.id, b.id, c.id);

        let mut edges_first = InventorySnapshot::new(DiscoverySource::StaticConfig);
        edges_first.add_entity(a);
        edges_first.add_dependency(DependencyEdge::new(a_id, b_id, DependencyType::DependsOn));
        edges_first.add_dependency(DependencyEdge::new(b_id, c_id, DependencyType::DependsOn));
        inventory.merge(&edges_first);
        assert_eq!(inventory.pending_dependencies().len(), 2);

        let mut rest = InventorySnapshot::new(DiscoverySource::AgentRegistration);
        rest.add_entity(b);
        rest.add_entity(c);
        let outcome = inventory.merge(&rest);

        assert_eq!(outcome.dependencies_touched, 2);
        assert_eq!(inventory.graph().len(), 2);
        assert_eq!(inventory.graph().upstream(a_id, None).len(), 2);
    }

    #[test]
    fn a_held_back_edge_is_not_remembered_twice() {
        let mut inventory = Inventory::new();
        let mut snapshot = InventorySnapshot::new(DiscoverySource::StaticConfig);
        let a = host("a");
        let a_id = a.id;
        snapshot.add_entity(a);
        snapshot.add_dependency(DependencyEdge::new(a_id, host("ghost").id, DependencyType::DependsOn));

        inventory.merge(&snapshot);
        inventory.merge(&snapshot);
        assert_eq!(inventory.pending_dependencies().len(), 1);
    }

    #[test]
    fn rediscovering_an_edge_keeps_its_first_seen_time() {
        let mut inventory = Inventory::new();
        let (a, b) = (host("a"), ManagedEntity::new("lab", EntityType::Storage, "s"));
        let (a_id, b_id) = (a.id, b.id);

        let mut first = InventorySnapshot::new(DiscoverySource::StaticConfig);
        first.add_entity(a);
        first.add_entity(b);
        first.add_dependency(DependencyEdge::new(a_id, b_id, DependencyType::UsesStorage));
        inventory.merge(&first);
        let first_seen = inventory.graph().edges()[0].first_seen_at;

        inventory.merge(&first);
        assert_eq!(inventory.graph().len(), 1);
        assert_eq!(inventory.graph().edges()[0].first_seen_at, first_seen);
    }

    #[test]
    fn merging_an_empty_snapshot_changes_nothing() {
        let mut inventory = Inventory::new();
        inventory.merge(&snapshot_from(DiscoverySource::StaticConfig, vec![host("a")]));
        let outcome = inventory.merge(&InventorySnapshot::new(DiscoverySource::StaticConfig));
        assert!(outcome.is_empty());
        assert_eq!(inventory.len(), 1);
    }
}
