//! Turning inventory into peer assignments the agents can act on.

use crate::entity::{EntityId, EntityType};
use crate::persistence::StoreError;
use crate::protocol::AssignedTarget;

use super::peers::{assign, observer_candidates, AssignmentPlan};
use super::{endpoint_for, Controller};

impl Controller {
    /// Build the current peer assignment plan.
    ///
    /// Recomputed from inventory rather than stored, because it is a pure
    /// function of inventory and the degree. Deterministic assignment means
    /// recomputing gives the same answer, so there is nothing to keep in sync.
    pub async fn assignment_plan(&self) -> Result<AssignmentPlan, StoreError> {
        let environment = self.config().environment.clone();
        let inventory = self.store().load_inventory(&environment).await?;

        let targets: Vec<EntityId> = inventory
            .entities()
            .filter(|e| e.entity_type == EntityType::Host)
            .filter(|e| e.lifecycle_state == crate::entity::LifecycleState::Active)
            .map(|e| e.id)
            .collect();

        let candidates = observer_candidates(inventory.entities());
        let degree = self.config().peer_monitoring.degree;

        // The revision is derived from what the plan depends on, so it changes
        // exactly when the plan could have changed, and an agent comparing
        // revisions learns something true.
        let revision = plan_revision(&targets, &candidates, degree);

        Ok(assign(&targets, &candidates, inventory.graph(), degree, revision))
    }

    /// What one observer should watch, with everything needed to probe it.
    pub async fn assigned_targets(
        &self,
        plan: &AssignmentPlan,
        observer: EntityId,
    ) -> Result<Vec<AssignedTarget>, StoreError> {
        let environment = self.config().environment.clone();
        let inventory = self.store().load_inventory(&environment).await?;

        let mut targets = Vec::new();
        for target in plan.targets_for(observer) {
            let Some(entity) = inventory.get(target) else {
                continue;
            };
            // The controller resolves the address from inventory, so an agent
            // never has to guess one (SPEC.md §37).
            let Some(endpoint) = endpoint_for(entity) else {
                continue;
            };

            targets.push(AssignedTarget {
                entity_id: entity.id.to_string(),
                name: entity.canonical_name.clone(),
                address: endpoint.address.clone(),
                parameters: endpoint.parameters(),
                capabilities: entity.capabilities.clone(),
            });
        }

        Ok(targets)
    }
}

/// A revision that changes when, and only when, the plan could have changed.
fn plan_revision(targets: &[EntityId], candidates: &[EntityId], degree: u32) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    let mut mix = |bytes: &[u8]| {
        for byte in bytes {
            hash ^= u64::from(*byte);
            hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
        }
    };

    for id in targets {
        mix(id.as_uuid().as_bytes());
    }
    for id in candidates {
        mix(id.as_uuid().as_bytes());
    }
    mix(&degree.to_le_bytes());

    hash
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::capability::{well_known, CapabilitySet};
    use crate::config::Config;
    use crate::entity::{DiscoverySource, EntityKey, ManagedEntity};
    use crate::inventory::InventorySnapshot;
    use crate::persistence::SqliteStore;

    fn host_id(name: &str) -> EntityId {
        EntityKey::new("lab", EntityType::Host, name).entity_id()
    }

    async fn controller_with(hosts: &[(&str, bool)]) -> Controller {
        let mut config = Config {
            config_version: 1,
            environment: "lab".into(),
            ..Config::default()
        };
        config.controller.observe = false;

        let store = SqliteStore::open_in_memory().await.expect("store");
        let mut controller = Controller::new(config, store).await.expect("controller");

        let mut snapshot = InventorySnapshot::new(DiscoverySource::StaticConfig);
        for (name, is_observer) in hosts {
            let mut entity = ManagedEntity::new("lab", EntityType::Host, *name);
            if *is_observer {
                entity = entity.with_capabilities(CapabilitySet::from_iter([well_known::OBSERVER_PEER]));
            }
            snapshot.add_entity(entity);
        }
        controller.ingest_snapshot(&snapshot).await.expect("inventory");
        controller
    }

    #[tokio::test]
    async fn every_host_is_assigned_observers() {
        let controller = controller_with(&[("a", true), ("b", true), ("c", true), ("d", true)]).await;
        let plan = controller.assignment_plan().await.expect("plan");

        assert_eq!(plan.assignments.len(), 4);
        for assignment in &plan.assignments {
            assert!(!assignment.observers.is_empty(), "{assignment:?}");
            assert!(!assignment.is_observed_by(assignment.target));
        }
    }

    #[tokio::test]
    async fn only_hosts_with_the_observer_capability_are_used() {
        let controller = controller_with(&[("watcher", true), ("plain", false), ("target", false)]).await;
        let plan = controller.assignment_plan().await.expect("plan");

        for assignment in &plan.assignments {
            for observer in &assignment.observers {
                assert_eq!(observer.entity, host_id("watcher"), "only the capable host may observe");
            }
        }
    }

    #[tokio::test]
    async fn a_cluster_with_no_observers_produces_empty_assignments() {
        // Honest rather than fabricated: with nobody able to observe, the plan
        // says so, and the reachability rules will decline to conclude anything.
        let controller = controller_with(&[("a", false), ("b", false)]).await;
        let plan = controller.assignment_plan().await.expect("plan");

        assert_eq!(plan.assignments.len(), 2);
        assert!(plan.assignments.iter().all(|a| a.observers.is_empty()));
    }

    #[tokio::test]
    async fn the_revision_is_stable_while_the_inventory_is() {
        let controller = controller_with(&[("a", true), ("b", true)]).await;
        let first = controller.assignment_plan().await.expect("plan");
        let second = controller.assignment_plan().await.expect("plan");
        assert_eq!(first.revision, second.revision);
        assert_eq!(first, second);
    }

    #[tokio::test]
    async fn the_revision_changes_when_a_host_joins() {
        let mut controller = controller_with(&[("a", true), ("b", true)]).await;
        let before = controller.assignment_plan().await.expect("plan").revision;

        let mut snapshot = InventorySnapshot::new(DiscoverySource::StaticConfig);
        snapshot.add_entity(
            ManagedEntity::new("lab", EntityType::Host, "c")
                .with_capabilities(CapabilitySet::from_iter([well_known::OBSERVER_PEER])),
        );
        controller.ingest_snapshot(&snapshot).await.expect("inventory");

        assert_ne!(controller.assignment_plan().await.expect("plan").revision, before);
    }

    #[tokio::test]
    async fn an_observer_is_told_where_to_reach_its_targets() {
        let controller = controller_with(&[("a", true), ("b", true), ("c", true)]).await;
        let plan = controller.assignment_plan().await.expect("plan");

        let targets = controller.assigned_targets(&plan, host_id("a")).await.expect("targets");
        assert!(!targets.is_empty());

        for target in &targets {
            assert!(
                !target.address.is_empty(),
                "an agent must never have to guess an address"
            );
            assert!(target.parameters.get("address").is_some());
            assert_ne!(target.entity_id, host_id("a").to_string(), "not itself");
        }
    }

    #[tokio::test]
    async fn an_observer_with_nothing_assigned_gets_an_empty_list() {
        let controller = controller_with(&[("a", true), ("b", false)]).await;
        let plan = controller.assignment_plan().await.expect("plan");
        let targets = controller.assigned_targets(&plan, host_id("b")).await.expect("targets");
        assert!(targets.is_empty());
    }

    #[tokio::test]
    async fn assignment_targets_carry_the_capabilities_that_decide_what_to_probe() {
        let mut config = Config {
            config_version: 1,
            environment: "lab".into(),
            ..Config::default()
        };
        config.controller.observe = false;
        let store = SqliteStore::open_in_memory().await.expect("store");
        let mut controller = Controller::new(config, store).await.expect("controller");

        let mut snapshot = InventorySnapshot::new(DiscoverySource::StaticConfig);
        snapshot.add_entity(
            ManagedEntity::new("lab", EntityType::Host, "watcher")
                .with_capabilities(CapabilitySet::from_iter([well_known::OBSERVER_PEER])),
        );
        snapshot.add_entity(
            ManagedEntity::new("lab", EntityType::Host, "target")
                .with_capabilities(CapabilitySet::from_iter(["ssh.server", "sentinel.agent"])),
        );
        controller.ingest_snapshot(&snapshot).await.expect("inventory");

        let plan = controller.assignment_plan().await.expect("plan");
        let targets = controller
            .assigned_targets(&plan, host_id("watcher"))
            .await
            .expect("targets");

        let target = targets.iter().find(|t| t.name == "target").expect("assigned");
        assert!(target.capabilities.has("ssh.server"));
        assert!(target.capabilities.has("sentinel.agent"));
    }
}
