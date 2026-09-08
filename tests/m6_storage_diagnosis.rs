//! M6 acceptance: storage faults are attributed to the right thing.
//!
//! The expensive mistakes in storage monitoring go both ways:
//!
//! * declaring a fileserver dead because one client's mount is wedged sends
//!   people to the wrong machine, and may get a healthy server rebooted;
//! * reporting five separate client faults hides the single cause behind them,
//!   and five people investigate five symptoms.
//!
//! The topology here mirrors the pseudo-cluster: two storage domains with
//! different clients, which is the minimum needed to tell those apart.

use std::collections::HashMap;

use sentinel::capability::CapabilitySet;
use sentinel::dependency::{DependencyEdge, DependencyType};
use sentinel::diagnosis::{builtin_rules, kind, Confidence, Diagnosis, DiagnosisContext, ObservationIndex};
use sentinel::entity::{EntityId, EntityKey, EntityType, ManagedEntity};
use sentinel::inventory::Inventory;
use sentinel::observation::{Observation, ProbeStatus};
use sentinel::probes::nfs::{PROBE_CLIENT_IO, PROBE_SERVER_PORT};
use sentinel::probes::ProbeId;
use sentinel::state::{ComponentState, EntityState, Health, StateComponent};

fn host_id(name: &str) -> EntityId {
    EntityKey::new("lab", EntityType::Host, name).entity_id()
}

fn storage_id(name: &str) -> EntityId {
    EntityKey::new("lab", EntityType::Storage, name).entity_id()
}

/// A cluster with two storage domains, as the pseudo-cluster has.
struct Cluster {
    inventory: Inventory,
    states: HashMap<EntityId, EntityState>,
    observations: ObservationIndex,
}

impl Cluster {
    /// storage-a serves c1 and c2; storage-b serves c3.
    fn new() -> Self {
        let mut inventory = Inventory::new();

        for name in ["fs-a", "fs-b"] {
            inventory.insert_entity(
                ManagedEntity::new("lab", EntityType::Host, name)
                    .with_capabilities(CapabilitySet::from_iter(["storage.nfs.server"])),
            );
        }
        for name in ["c1", "c2", "c3"] {
            inventory.insert_entity(
                ManagedEntity::new("lab", EntityType::Host, name)
                    .with_capabilities(CapabilitySet::from_iter(["storage.nfs.client"])),
            );
        }
        for name in ["storage-a", "storage-b"] {
            inventory.insert_entity(ManagedEntity::new("lab", EntityType::Storage, name));
        }

        for (storage, server) in [("storage-a", "fs-a"), ("storage-b", "fs-b")] {
            inventory.insert_dependency(DependencyEdge::new(
                storage_id(storage),
                host_id(server),
                DependencyType::Provides,
            ));
        }
        for (client, storage) in [("c1", "storage-a"), ("c2", "storage-a"), ("c3", "storage-b")] {
            inventory.insert_dependency(DependencyEdge::new(
                host_id(client),
                storage_id(storage),
                DependencyType::UsesStorage,
            ));
        }

        let mut cluster = Self {
            inventory,
            states: HashMap::new(),
            observations: ObservationIndex::new(),
        };
        // Everything starts up and healthy.
        for name in ["fs-a", "fs-b", "c1", "c2", "c3"] {
            cluster = cluster.host_up(name);
        }
        cluster
    }

    fn host_up(mut self, name: &str) -> Self {
        let id = host_id(name);
        let state = self.states.entry(id).or_insert_with(|| EntityState::unknown(id));
        state.set_component(StateComponent::Network, ComponentState::new(Health::Healthy));
        state.set_component(StateComponent::Agent, ComponentState::new(Health::Healthy));
        self
    }

    fn host_down(mut self, name: &str) -> Self {
        let id = host_id(name);
        let state = self.states.entry(id).or_insert_with(|| EntityState::unknown(id));
        state.set_component(StateComponent::Network, ComponentState::new(Health::Unavailable));
        state.set_component(StateComponent::Agent, ComponentState::new(Health::Unavailable));
        self
    }

    fn storage_impaired(mut self, name: &str, health: Health) -> Self {
        let id = host_id(name);
        let state = self.states.entry(id).or_insert_with(|| EntityState::unknown(id));
        state.set_component(StateComponent::Storage, ComponentState::new(health));
        self
    }

    fn observe(mut self, name: &str, probe: &str, status: ProbeStatus) -> Self {
        self.observations
            .insert(Observation::new(ProbeId::new(probe), host_id(name), status));
        self
    }

    fn diagnose(&self) -> Vec<Diagnosis> {
        let context = DiagnosisContext {
            environment: "lab",
            inventory: &self.inventory,
            states: &self.states,
            observations: &self.observations,
        };
        builtin_rules().diagnose(&context)
    }

    fn types(&self) -> Vec<String> {
        self.diagnose()
            .into_iter()
            .map(|d| d.diagnosis_type.to_string())
            .collect()
    }
}

#[test]
fn a_healthy_cluster_produces_no_storage_diagnoses() {
    assert!(Cluster::new().types().is_empty());
}

#[test]
fn a_fileservers_export_service_failing_is_told_apart_from_the_fileserver_failing() {
    // SPEC.md §172. The host is up; only the service is not.
    let cluster = Cluster::new().observe("fs-a", PROBE_SERVER_PORT, ProbeStatus::Failed);

    let diagnoses = cluster.diagnose();
    let service_failure = diagnoses
        .iter()
        .find(|d| d.is(kind::NFS_SERVICE_FAILURE))
        .expect("the service failure is diagnosed");

    assert_eq!(service_failure.suspected_root_entities, vec![host_id("fs-a")]);
    assert!(
        service_failure.summary.contains("is up but"),
        "{}",
        service_failure.summary
    );
}

#[test]
fn an_unreachable_fileserver_is_not_blamed_on_its_export_service() {
    // The machine is gone. Reporting a service failure would send someone to
    // restart a daemon on a host that is not answering.
    let cluster = Cluster::new()
        .host_down("fs-a")
        .observe("fs-a", PROBE_SERVER_PORT, ProbeStatus::Failed);

    assert!(!cluster.types().contains(&kind::NFS_SERVICE_FAILURE.to_string()));
}

#[test]
fn clients_of_one_storage_failing_together_is_one_incident_not_several() {
    // SPEC.md §173: the whole point of the dependency graph.
    let cluster = Cluster::new()
        .storage_impaired("c1", Health::Unavailable)
        .storage_impaired("c2", Health::Unavailable);

    let diagnoses = cluster.diagnose();
    let shared = diagnoses
        .iter()
        .find(|d| d.is(kind::SHARED_STORAGE_FAILURE))
        .expect("correlated into one shared failure");

    assert_eq!(
        shared.confidence,
        Confidence::High,
        "every client of storage-a is affected"
    );
    assert_eq!(
        shared.suspected_root_entities,
        vec![host_id("fs-a")],
        "the cause is the fileserver behind it"
    );
    assert!(shared.affected_entities.contains(&host_id("c1")));
    assert!(shared.affected_entities.contains(&host_id("c2")));
    assert!(
        !shared.affected_entities.contains(&host_id("c3")),
        "the other domain is untouched"
    );

    // And crucially, not reported as two separate client faults.
    assert!(
        !cluster.types().contains(&kind::NFS_CLIENT_FAILURE.to_string()),
        "a shared cause must not be reported as several client faults"
    );
}

#[test]
fn one_client_failing_alone_does_not_incriminate_the_fileserver() {
    // SPEC.md §174, and the mistake that gets a healthy fileserver rebooted.
    let cluster = Cluster::new().storage_impaired("c1", Health::Unavailable);

    let diagnoses = cluster.diagnose();
    let client_failure = diagnoses
        .iter()
        .find(|d| d.is(kind::NFS_CLIENT_FAILURE))
        .expect("diagnosed as client-local");

    assert_eq!(
        client_failure.suspected_root_entities,
        vec![host_id("c1")],
        "the client, not the server"
    );
    assert!(
        client_failure.summary.contains("c2"),
        "it should say who is fine: {}",
        client_failure.summary
    );

    assert!(
        !cluster.types().contains(&kind::SHARED_STORAGE_FAILURE.to_string()),
        "one client is not evidence about the storage"
    );
    assert!(
        !cluster.types().contains(&kind::NFS_SERVICE_FAILURE.to_string()),
        "the fileserver must not be implicated"
    );
}

#[test]
fn the_two_storage_situations_are_never_reported_together() {
    // An operator told both "the storage is broken" and "only this client is
    // broken" has learned nothing.
    for (name, setup) in [
        (
            "shared",
            Cluster::new()
                .storage_impaired("c1", Health::Unavailable)
                .storage_impaired("c2", Health::Unavailable),
        ),
        (
            "client-local",
            Cluster::new().storage_impaired("c1", Health::Unavailable),
        ),
    ] {
        let types = setup.types();
        let shared = types.contains(&kind::SHARED_STORAGE_FAILURE.to_string());
        let local = types.contains(&kind::NFS_CLIENT_FAILURE.to_string());
        assert!(shared != local, "{name}: exactly one should fire, got {types:?}");
    }
}

#[test]
fn a_slow_mount_is_diagnosed_without_being_called_an_outage() {
    // Storage that answers eventually is degraded, not down.
    let cluster =
        Cluster::new()
            .storage_impaired("c1", Health::Degraded)
            .observe("c1", PROBE_CLIENT_IO, ProbeStatus::Degraded);

    let diagnoses = cluster.diagnose();
    assert!(diagnoses.iter().any(|d| d.is(kind::NFS_CLIENT_FAILURE)));
}

#[test]
fn a_stuck_mount_gets_a_hint_about_blocked_tasks() {
    let cluster =
        Cluster::new()
            .storage_impaired("c1", Health::Unavailable)
            .observe("c1", PROBE_CLIENT_IO, ProbeStatus::Stuck);

    let diagnoses = cluster.diagnose();
    let client_failure = diagnoses
        .iter()
        .find(|d| d.is(kind::NFS_CLIENT_FAILURE))
        .expect("diagnosed");
    assert!(
        client_failure.recommended_actions.iter().any(|a| a.contains("stack")),
        "{:?}",
        client_failure.recommended_actions
    );
}

#[test]
fn no_storage_diagnosis_recommends_touching_a_mount() {
    // Remounting is exactly the sort of thing that turns a degraded mount into
    // a wedged one, and v1 does not act at all (SPEC.md §113).
    let cluster = Cluster::new()
        .observe("fs-a", PROBE_SERVER_PORT, ProbeStatus::Failed)
        .storage_impaired("c1", Health::Unavailable);

    let diagnoses = cluster.diagnose();
    assert!(!diagnoses.is_empty());

    for diagnosis in &diagnoses {
        for action in &diagnosis.recommended_actions {
            for mutating in ["mount ", "umount", "restart", "reboot", "exportfs -r", "-o remount"] {
                assert!(
                    !action.contains(mutating),
                    "{} recommended a mutating command: {action}",
                    diagnosis.diagnosis_type
                );
            }
        }
    }
}

#[test]
fn storage_groups_are_derived_from_the_graph_not_declared() {
    // SPEC.md §29: adding a storage domain must need no code change. Here a
    // third domain is added at runtime and the rules pick it up.
    let mut cluster = Cluster::new();
    cluster.inventory.insert_entity(
        ManagedEntity::new("lab", EntityType::Host, "fs-c")
            .with_capabilities(CapabilitySet::from_iter(["storage.nfs.server"])),
    );
    cluster
        .inventory
        .insert_entity(ManagedEntity::new("lab", EntityType::Storage, "storage-c"));
    cluster.inventory.insert_dependency(DependencyEdge::new(
        storage_id("storage-c"),
        host_id("fs-c"),
        DependencyType::Provides,
    ));
    for client in ["c4", "c5"] {
        cluster.inventory.insert_entity(
            ManagedEntity::new("lab", EntityType::Host, client)
                .with_capabilities(CapabilitySet::from_iter(["storage.nfs.client"])),
        );
        cluster.inventory.insert_dependency(DependencyEdge::new(
            host_id(client),
            storage_id("storage-c"),
            DependencyType::UsesStorage,
        ));
    }

    let cluster = cluster
        .host_up("fs-c")
        .host_up("c4")
        .host_up("c5")
        .storage_impaired("c4", Health::Unavailable)
        .storage_impaired("c5", Health::Unavailable);

    let diagnoses = cluster.diagnose();
    let shared = diagnoses
        .iter()
        .find(|d| d.is(kind::SHARED_STORAGE_FAILURE))
        .expect("the new domain is correlated with no code change");
    assert_eq!(shared.suspected_root_entities, vec![host_id("fs-c")]);
}

#[test]
fn a_storage_diagnosis_cites_the_observations_behind_it() {
    let cluster = Cluster::new()
        .observe("fs-a", PROBE_SERVER_PORT, ProbeStatus::Failed)
        .storage_impaired("c1", Health::Unavailable)
        .observe("c1", PROBE_CLIENT_IO, ProbeStatus::Timeout);

    for diagnosis in cluster.diagnose() {
        assert!(
            !diagnosis.evidence.is_empty(),
            "{} cites no evidence",
            diagnosis.diagnosis_type
        );
        assert!(!diagnosis.rule_id.as_str().is_empty());
    }
}
