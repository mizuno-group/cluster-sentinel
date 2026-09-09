//! A storage domain's health comes from the machine that serves it.
//!
//! Nothing probes a storage entity directly -- it is a concept, deliberately
//! kept distinct from the fileserver so that "the machine is up but its
//! exports are gone" can be said at all. The cost was that its health stayed
//! `unknown` forever, and on a real cluster that meant five rows in `status`
//! that never said anything.
//!
//! The evidence it needs already exists: its provider's export probes. What
//! this file pins down is which half of that provider's evidence is used. A
//! host can be a fileserver *and* an NFS client -- a compute node exporting a
//! scratch tree while mounting someone else's home directories -- and both
//! sides land on the same `storage` component of the host. Taking that
//! component wholesale would report a wedged client mount as a failure of what
//! the host serves, and send someone to the wrong machine.

use sentinel::config::Config;
use sentinel::controller::Controller;
use sentinel::entity::{EntityId, EntityKey, EntityType};
use sentinel::observation::{Observation, ProbeStatus};
use sentinel::persistence::SqliteStore;
use sentinel::probes::ProbeId;
use sentinel::protocol::ObservationBatch;
use sentinel::state::{Health, StateComponent};

const SERVER_PORT: &str = sentinel::probes::nfs::PROBE_SERVER_PORT;
const CLIENT_IO: &str = sentinel::probes::nfs::PROBE_CLIENT_IO;
const MOUNT: &str = sentinel::probes::nfs::PROBE_CLIENT_MOUNT;

fn host(name: &str) -> EntityId {
    EntityKey::new("lab", EntityType::Host, name).entity_id()
}

fn storage(name: &str) -> EntityId {
    EntityKey::new("lab", EntityType::Storage, name).entity_id()
}

async fn controller(hosts: &[&str]) -> Controller {
    let mut text = String::from("config_version = 1\nenvironment = \"lab\"\n\n[controller]\nobserve = false\n");
    for name in hosts {
        text.push_str(&format!("\n[[entities]]\ntype = \"host\"\nname = \"{name}\"\n"));
    }
    let config = Config::from_toml(&text, std::path::Path::new("test.toml")).expect("config");
    let store = SqliteStore::open_in_memory().await.expect("store");
    let mut controller = Controller::new(config, store).await.expect("controller");
    controller.discover_once().await.expect("discovery");
    controller
}

/// One host's mount table, which is what creates the storage entity.
fn mount_report(client: &str, server: &str) -> Observation {
    Observation::new(ProbeId::new(MOUNT), host(client), ProbeStatus::Ok).with_payload(serde_json::json!({
        "mounts": [{
            "source": format!("{server}:/data"),
            "target": "/mnt/data",
            "fstype": "nfs4",
            "server": server,
            "read_only": false,
        }]
    }))
}

fn server_port(name: &str, status: ProbeStatus) -> Observation {
    Observation::new(ProbeId::new(SERVER_PORT), host(name), status)
}

fn client_io(name: &str, status: ProbeStatus) -> Observation {
    Observation::new(ProbeId::new(CLIENT_IO), host(name), status)
}

async fn health_of_storage(controller: &Controller, name: &str) -> Health {
    controller
        .store()
        .load_entity_states("lab")
        .await
        .expect("states")
        .get(&storage(name))
        .map(|state| state.component(StateComponent::Storage))
        .unwrap_or(Health::Unknown)
}

/// Build a two-node world: fs1 serves, node01 mounts.
async fn served_cluster() -> Controller {
    let mut controller = controller(&["node01", "fs1"]).await;
    controller
        .ingest_observations(&[mount_report("node01", "fs1")])
        .await
        .expect("mounts");
    controller.discover_once().await.expect("derive the topology");
    controller
}

#[tokio::test]
async fn a_storage_domain_takes_the_health_of_its_providers_exports() {
    let mut controller = served_cluster().await;

    let healthy: Vec<Observation> = (0..4).map(|_| server_port("fs1", ProbeStatus::Ok)).collect();
    controller.ingest_observations(&healthy).await.expect("observations");
    assert_eq!(health_of_storage(&controller, "fs1").await, Health::Healthy);

    let failing: Vec<Observation> = (0..4).map(|_| server_port("fs1", ProbeStatus::Failed)).collect();
    controller.ingest_observations(&failing).await.expect("observations");
    assert_ne!(
        health_of_storage(&controller, "fs1").await,
        Health::Healthy,
        "the export port stopped answering; the domain it serves is not fine"
    );
}

#[tokio::test]
async fn a_wedged_client_mount_is_not_a_failure_of_what_that_host_serves() {
    // The reason only the server side is copied. fs1 both serves and mounts;
    // its own mount of someone else's export goes wrong. That is a fault on
    // fs1, and the host says so -- but the domain fs1 *serves* is untouched,
    // and reporting it as broken sends people to the wrong machine.
    let mut controller = served_cluster().await;

    let healthy: Vec<Observation> = (0..4).map(|_| server_port("fs1", ProbeStatus::Ok)).collect();
    controller.ingest_observations(&healthy).await.expect("observations");
    assert_eq!(health_of_storage(&controller, "fs1").await, Health::Healthy);

    let wedged: Vec<Observation> = (0..4).map(|_| client_io("fs1", ProbeStatus::Failed)).collect();
    controller.ingest_observations(&wedged).await.expect("observations");

    let states = controller.store().load_entity_states("lab").await.expect("states");
    assert_ne!(
        states[&host("fs1")].component(StateComponent::Storage),
        Health::Healthy,
        "the host has a storage problem and must say so"
    );
    assert_eq!(
        health_of_storage(&controller, "fs1").await,
        Health::Healthy,
        "but its exports never stopped answering"
    );
}

#[tokio::test]
async fn one_bad_reading_does_not_condemn_a_storage_domain() {
    // The copies go through the state engine rather than setting a health
    // directly, so they debounce like everything else.
    let mut controller = served_cluster().await;

    let healthy: Vec<Observation> = (0..4).map(|_| server_port("fs1", ProbeStatus::Ok)).collect();
    controller.ingest_observations(&healthy).await.expect("observations");

    controller
        .ingest_observations(&[server_port("fs1", ProbeStatus::Failed)])
        .await
        .expect("observations");

    assert_eq!(
        health_of_storage(&controller, "fs1").await,
        Health::Healthy,
        "one dropped packet is not an outage"
    );
}

#[tokio::test]
async fn the_evidence_leads_back_to_a_real_observation() {
    // The copy keeps the original's id so a verdict about a storage domain can
    // still be followed down to something stored, rather than to a synthetic
    // observation that exists only in memory.
    let mut controller = served_cluster().await;

    let observations: Vec<Observation> = (0..4).map(|_| server_port("fs1", ProbeStatus::Ok)).collect();
    let ids: Vec<_> = observations.iter().map(|o| o.id).collect();
    controller
        .ingest_observations(&observations)
        .await
        .expect("observations");

    let states = controller.store().load_entity_states("lab").await.expect("states");
    let evidence = &states[&storage("fs1")].components[&StateComponent::Storage].evidence;

    assert!(!evidence.is_empty(), "a verdict with no evidence is not checkable");
    assert!(
        evidence.iter().all(|id| ids.contains(id)),
        "evidence must name observations that were actually stored"
    );
    for id in evidence {
        assert!(
            controller
                .store()
                .recent_observations(host("fs1"), 64)
                .await
                .expect("stored")
                .iter()
                .any(|o| &o.id == id),
            "the evidence is the provider's own export probe"
        );
    }
}

#[tokio::test]
async fn a_provider_nothing_has_probed_leaves_its_domain_unknown() {
    // Better than a guess. A fileserver with no agent and no reachable export
    // port has told us nothing, and "unknown" is the honest answer.
    let controller = served_cluster().await;
    assert_eq!(health_of_storage(&controller, "fs1").await, Health::Unknown);
}

#[tokio::test]
async fn an_agents_own_export_probe_reaches_the_domain_it_serves() {
    // `nfs.server.exports` reads /etc/exports, so it only ever runs on the
    // fileserver itself and only ever arrives in that agent's batch. That
    // route bypassed the derivation, which left the authoritative evidence out
    // of it: on a real cluster the storage domain stayed UNKNOWN while its
    // provider was reporting healthy exports every minute.
    let mut controller = served_cluster().await;

    let exports: Vec<Observation> = (0..4)
        .map(|_| {
            Observation::new(
                ProbeId::new(sentinel::probes::nfs::PROBE_SERVER_EXPORTS),
                host("fs1"),
                ProbeStatus::Ok,
            )
        })
        .collect();

    controller
        .ingest_agent_batch(&ObservationBatch {
            protocol_version: sentinel::PROTOCOL_VERSION,
            agent_id: uuid::Uuid::new_v4(),
            session_id: uuid::Uuid::new_v4(),
            observations: exports,
        })
        .await
        .expect("agent batch");

    assert_eq!(
        health_of_storage(&controller, "fs1").await,
        Health::Healthy,
        "the provider says its exports are fine; the domain it serves is fine"
    );
}

#[tokio::test]
async fn an_agent_reporting_broken_exports_condemns_the_domain() {
    let mut controller = served_cluster().await;
    let batch = |status| ObservationBatch {
        protocol_version: sentinel::PROTOCOL_VERSION,
        agent_id: uuid::Uuid::new_v4(),
        session_id: uuid::Uuid::new_v4(),
        observations: (0..4)
            .map(|_| {
                Observation::new(
                    ProbeId::new(sentinel::probes::nfs::PROBE_SERVER_EXPORTS),
                    host("fs1"),
                    status,
                )
            })
            .collect(),
    };

    controller
        .ingest_agent_batch(&batch(ProbeStatus::Ok))
        .await
        .expect("healthy");
    assert_eq!(health_of_storage(&controller, "fs1").await, Health::Healthy);

    controller
        .ingest_agent_batch(&batch(ProbeStatus::Failed))
        .await
        .expect("failing");
    assert_ne!(
        health_of_storage(&controller, "fs1").await,
        Health::Healthy,
        "nothing is exported any more"
    );
}
