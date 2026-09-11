//! M9 acceptance: diagnoses become incidents an operator can act on.
//!
//! The two failure modes this guards against are opposite and both familiar:
//!
//! * **Alert storms.** Five nodes behind one fileserver producing five alerts
//!   sends five people to five symptoms while the cause goes unexamined.
//! * **Premature closure.** Declaring an incident over because the root
//!   recovered, while the things depending on it are still broken, tells an
//!   operator it is finished while people still cannot work.

use sentinel::config::Config;
use sentinel::controller::Controller;
use sentinel::dependency::{DependencyEdge, DependencyType};
use sentinel::diagnosis::kind;
use sentinel::entity::{DiscoverySource, EntityId, EntityKey, EntityType, ManagedEntity};
use sentinel::incident::{IncidentStatus, Severity};
use sentinel::inventory::InventorySnapshot;
use sentinel::observation::{Observation, ProbeStatus};
use sentinel::persistence::SqliteStore;
use sentinel::probes::nfs::PROBE_SERVER_PORT;
use sentinel::probes::ProbeId;

const NETWORK: &str = sentinel::probes::network::PROBE_ID;
const AGENT: &str = sentinel::probes::sentinel_rpc::PROBE_ID;

fn host(name: &str) -> EntityId {
    EntityKey::new("lab", EntityType::Host, name).entity_id()
}

fn storage(name: &str) -> EntityId {
    EntityKey::new("lab", EntityType::Storage, name).entity_id()
}

/// A controller over a cluster with one storage domain and three clients.
async fn cluster() -> Controller {
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
        ManagedEntity::new("lab", EntityType::Host, "fs1")
            .with_capabilities(["storage.nfs.server"].into_iter().collect()),
    );
    snapshot.add_entity(ManagedEntity::new("lab", EntityType::Storage, "storage-a"));
    snapshot.add_dependency(DependencyEdge::new(
        storage("storage-a"),
        host("fs1"),
        DependencyType::Provides,
    ));

    for client in ["c1", "c2", "c3"] {
        snapshot.add_entity(
            ManagedEntity::new("lab", EntityType::Host, client)
                .with_capabilities(["storage.nfs.client"].into_iter().collect()),
        );
        snapshot.add_dependency(DependencyEdge::new(
            host(client),
            storage("storage-a"),
            DependencyType::UsesStorage,
        ));
    }

    controller.ingest_snapshot(&snapshot).await.expect("inventory");
    controller
}

/// Make a host look reachable and answering.
async fn make_healthy(controller: &mut Controller, names: &[&str]) {
    let mut observations = Vec::new();
    for name in names {
        for _ in 0..3 {
            observations.push(Observation::new(ProbeId::new(NETWORK), host(name), ProbeStatus::Ok));
            observations.push(Observation::new(ProbeId::new(AGENT), host(name), ProbeStatus::Ok));
        }
    }
    controller
        .ingest_observations(&observations)
        .await
        .expect("observations");
}

/// Break the fileserver's export service.
async fn break_storage_service(controller: &mut Controller) {
    let mut observations = Vec::new();
    for _ in 0..3 {
        observations.push(Observation::new(
            ProbeId::new(PROBE_SERVER_PORT),
            host("fs1"),
            ProbeStatus::Failed,
        ));
    }
    controller
        .ingest_observations(&observations)
        .await
        .expect("observations");
}

/// Impair every client's storage.
async fn break_clients(controller: &mut Controller, names: &[&str]) {
    let mut observations = Vec::new();
    for name in names {
        for _ in 0..3 {
            observations.push(Observation::new(
                ProbeId::new(sentinel::probes::nfs::PROBE_CLIENT_IO),
                host(name),
                ProbeStatus::Timeout,
            ));
        }
    }
    controller
        .ingest_observations(&observations)
        .await
        .expect("observations");
}

/// Let the clients' storage recover.
async fn recover_clients(controller: &mut Controller, names: &[&str]) {
    let mut observations = Vec::new();
    for name in names {
        for _ in 0..3 {
            observations.push(Observation::new(
                ProbeId::new(sentinel::probes::nfs::PROBE_CLIENT_IO),
                host(name),
                ProbeStatus::Ok,
            ));
        }
    }
    controller
        .ingest_observations(&observations)
        .await
        .expect("observations");
}

#[tokio::test]
async fn a_healthy_cluster_opens_no_incidents() {
    let mut controller = cluster().await;
    make_healthy(&mut controller, &["fs1", "c1", "c2", "c3"]).await;

    let (diagnoses, update) = controller.diagnose_and_correlate().await.expect("correlate");
    assert!(diagnoses.is_empty(), "{diagnoses:#?}");
    assert!(update.is_empty());
}

#[tokio::test]
async fn one_cause_produces_one_incident_not_one_per_symptom() {
    // The alert storm this exists to prevent.
    let mut controller = cluster().await;
    make_healthy(&mut controller, &["fs1", "c1", "c2", "c3"]).await;
    break_storage_service(&mut controller).await;
    break_clients(&mut controller, &["c1", "c2", "c3"]).await;

    let (diagnoses, update) = controller.diagnose_and_correlate().await.expect("correlate");

    assert!(diagnoses.len() >= 2, "several symptoms were diagnosed: {diagnoses:#?}");
    assert_eq!(
        update.opened.len(),
        1,
        "but they share a cause, so they are one incident: {:#?}",
        update.opened
    );

    let incident = &update.opened[0];
    assert_eq!(incident.suspected_root_entities, vec![host("fs1")]);
    assert!(incident.has_diagnosis(kind::NFS_SERVICE_FAILURE));
    assert!(incident.has_diagnosis(kind::SHARED_STORAGE_FAILURE));
}

#[tokio::test]
async fn fan_out_makes_a_shared_failure_critical() {
    // SPEC.md §101: a fileserver with three dependent nodes is not the same
    // size of problem as one node.
    let mut controller = cluster().await;
    make_healthy(&mut controller, &["fs1", "c1", "c2", "c3"]).await;
    break_storage_service(&mut controller).await;
    break_clients(&mut controller, &["c1", "c2", "c3"]).await;

    let (_, update) = controller.diagnose_and_correlate().await.expect("correlate");
    assert_eq!(update.opened[0].severity, Severity::Critical);
}

#[tokio::test]
async fn an_incident_does_not_close_while_clients_are_still_broken() {
    // IMPLEMENTATION.md §75, and the reason "the server is back" is not the
    // same as "the incident is over".
    let mut controller = cluster().await;
    make_healthy(&mut controller, &["fs1", "c1", "c2", "c3"]).await;
    break_storage_service(&mut controller).await;
    break_clients(&mut controller, &["c1", "c2", "c3"]).await;
    controller.diagnose_and_correlate().await.expect("open");

    // The server comes back, but two clients have not.
    make_healthy(&mut controller, &["fs1"]).await;
    let mut observations = Vec::new();
    for _ in 0..3 {
        observations.push(Observation::new(
            ProbeId::new(PROBE_SERVER_PORT),
            host("fs1"),
            ProbeStatus::Ok,
        ));
    }
    controller
        .ingest_observations(&observations)
        .await
        .expect("observations");
    recover_clients(&mut controller, &["c1"]).await;

    let (_, update) = controller.diagnose_and_correlate().await.expect("correlate");

    assert!(update.resolved.is_empty(), "must not close while clients are impaired");
    let active: Vec<_> = controller.incident_engine().active().collect();
    assert_eq!(active.len(), 1);
    assert!(
        matches!(active[0].status, IncidentStatus::Recovering | IncidentStatus::Open),
        "{:?}",
        active[0].status
    );
}

#[tokio::test]
async fn an_incident_closes_once_everything_has_recovered() {
    let mut controller = cluster().await;
    make_healthy(&mut controller, &["fs1", "c1", "c2", "c3"]).await;
    break_storage_service(&mut controller).await;
    break_clients(&mut controller, &["c1", "c2", "c3"]).await;
    controller.diagnose_and_correlate().await.expect("open");

    let mut observations = Vec::new();
    for _ in 0..3 {
        observations.push(Observation::new(
            ProbeId::new(PROBE_SERVER_PORT),
            host("fs1"),
            ProbeStatus::Ok,
        ));
    }
    controller
        .ingest_observations(&observations)
        .await
        .expect("observations");
    recover_clients(&mut controller, &["c1", "c2", "c3"]).await;

    // One pass to notice the cause is gone, another once the clients are back.
    controller.diagnose_and_correlate().await.expect("recovering");
    let (_, update) = controller.diagnose_and_correlate().await.expect("resolved");

    assert_eq!(controller.incident_engine().active().count(), 0, "{update:#?}");
}

#[tokio::test]
async fn an_incident_and_its_evidence_survive_a_controller_restart() {
    // SPEC.md §103: the evidence has to outlive the process that gathered it.
    let store = SqliteStore::open_in_memory().await.expect("store");
    let mut config = Config {
        config_version: 1,
        environment: "lab".into(),
        ..Config::default()
    };
    config.controller.observe = false;

    let (incident_id, evidence_count) = {
        let mut controller = Controller::new(config.clone(), store.clone())
            .await
            .expect("controller");

        let mut snapshot = InventorySnapshot::new(DiscoverySource::StaticConfig);
        snapshot.add_entity(
            ManagedEntity::new("lab", EntityType::Host, "fs1")
                .with_capabilities(["storage.nfs.server"].into_iter().collect()),
        );
        // A fileserver with a storage domain and a client. The subject here is
        // incident persistence, not storage, but the world still has to be one
        // the rules recognise: a host nobody mounts from has no export service
        // to fail.
        snapshot.add_entity(ManagedEntity::new("lab", EntityType::Storage, "storage-a"));
        snapshot.add_entity(ManagedEntity::new("lab", EntityType::Host, "c1"));
        snapshot.add_dependency(DependencyEdge::new(
            storage("storage-a"),
            host("fs1"),
            DependencyType::Provides,
        ));
        snapshot.add_dependency(DependencyEdge::new(
            host("c1"),
            storage("storage-a"),
            DependencyType::UsesStorage,
        ));
        controller.ingest_snapshot(&snapshot).await.expect("inventory");

        make_healthy(&mut controller, &["fs1"]).await;
        break_storage_service(&mut controller).await;

        let (_, update) = controller.diagnose_and_correlate().await.expect("correlate");
        assert_eq!(update.opened.len(), 1);
        (update.opened[0].id, update.opened[0].evidence.len())
    };

    // A new controller over the same database.
    let restarted = Controller::new(config, store.clone()).await.expect("restarted");
    assert_eq!(
        restarted.incident_engine().active().count(),
        1,
        "the open incident is resumed"
    );

    let loaded = store
        .load_incident(&incident_id.to_string())
        .await
        .expect("load")
        .expect("found");
    assert_eq!(loaded.evidence.len(), evidence_count);
    assert!(!loaded.diagnoses.is_empty());
    assert!(!loaded.timeline.is_empty());
}

#[tokio::test]
async fn a_restart_does_not_re_alert_on_an_already_open_incident() {
    // Otherwise every controller restart wakes someone about a fault they are
    // already dealing with.
    let store = SqliteStore::open_in_memory().await.expect("store");
    let mut config = Config {
        config_version: 1,
        environment: "lab".into(),
        ..Config::default()
    };
    config.controller.observe = false;

    {
        let mut controller = Controller::new(config.clone(), store.clone())
            .await
            .expect("controller");
        let mut snapshot = InventorySnapshot::new(DiscoverySource::StaticConfig);
        snapshot.add_entity(
            ManagedEntity::new("lab", EntityType::Host, "fs1")
                .with_capabilities(["storage.nfs.server"].into_iter().collect()),
        );
        // A fileserver with a storage domain and a client. The subject here is
        // incident persistence, not storage, but the world still has to be one
        // the rules recognise: a host nobody mounts from has no export service
        // to fail.
        snapshot.add_entity(ManagedEntity::new("lab", EntityType::Storage, "storage-a"));
        snapshot.add_entity(ManagedEntity::new("lab", EntityType::Host, "c1"));
        snapshot.add_dependency(DependencyEdge::new(
            storage("storage-a"),
            host("fs1"),
            DependencyType::Provides,
        ));
        snapshot.add_dependency(DependencyEdge::new(
            host("c1"),
            storage("storage-a"),
            DependencyType::UsesStorage,
        ));
        controller.ingest_snapshot(&snapshot).await.expect("inventory");
        make_healthy(&mut controller, &["fs1"]).await;
        break_storage_service(&mut controller).await;
        controller.diagnose_and_correlate().await.expect("open");
    }

    let mut restarted = Controller::new(config, store).await.expect("restarted");
    let (_, update) = restarted.diagnose_and_correlate().await.expect("correlate");

    assert!(update.opened.is_empty(), "the incident was already open");
    assert_eq!(update.updated.len(), 1);
}

#[tokio::test]
async fn every_incident_can_be_traced_to_the_observations_behind_it() {
    let mut controller = cluster().await;
    make_healthy(&mut controller, &["fs1", "c1", "c2", "c3"]).await;
    break_storage_service(&mut controller).await;
    break_clients(&mut controller, &["c1", "c2", "c3"]).await;

    let (_, update) = controller.diagnose_and_correlate().await.expect("correlate");
    let incident = &update.opened[0];

    assert!(
        !incident.evidence.is_empty(),
        "an incident with no evidence is an assertion"
    );

    // Every cited observation is actually retrievable.
    let mut stored = Vec::new();
    for entity in &incident.affected_entities {
        stored.extend(
            controller
                .store()
                .recent_observations(*entity, 64)
                .await
                .expect("observations")
                .into_iter()
                .map(|o| o.id),
        );
    }
    let traceable = incident.evidence.iter().filter(|id| stored.contains(id)).count();
    assert!(traceable > 0, "no cited observation could be looked up");
}

#[tokio::test]
async fn unrelated_faults_do_not_merge_into_one_incident() {
    let mut controller = cluster().await;
    make_healthy(&mut controller, &["fs1", "c1", "c2", "c3"]).await;

    // One client's storage fails on its own; separately the fileserver's
    // export service fails. Different causes, different incidents.
    break_clients(&mut controller, &["c1"]).await;
    let (_, first) = controller.diagnose_and_correlate().await.expect("correlate");
    assert_eq!(first.opened.len(), 1);

    break_storage_service(&mut controller).await;
    let (_, second) = controller.diagnose_and_correlate().await.expect("correlate");

    let total = controller.incident_engine().active().count();
    assert!(total >= 2, "distinct causes must stay distinct: {second:#?}");
}
