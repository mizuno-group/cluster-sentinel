//! The storage dependency graph is derived, not transcribed.
//!
//! Writing it by hand works and is exact, but it is a second copy of something
//! the cluster already reports: every agent sends its own NFS mounts on every
//! cycle, and each mount is an edge. The second copy has to be maintained by
//! hand as nodes are added, moved and re-exported, and when it falls behind
//! nothing complains -- diagnosis quietly degrades from one incident naming
//! the fileserver to one incident per client, and that is discovered during
//! the outage that needed it.
//!
//! On a real cluster this was 24 hand-written dependency stanzas for eleven
//! nodes and five fileservers.

use sentinel::config::Config;
use sentinel::controller::Controller;
use sentinel::dependency::DependencyType;
use sentinel::diagnosis::kind;
use sentinel::entity::{EntityId, EntityKey, EntityType};
use sentinel::observation::{Observation, ProbeStatus};
use sentinel::persistence::SqliteStore;
use sentinel::probes::ProbeId;

const MOUNT: &str = sentinel::probes::nfs::PROBE_CLIENT_MOUNT;
const IO: &str = sentinel::probes::nfs::PROBE_CLIENT_IO;

fn host(name: &str) -> EntityId {
    EntityKey::new("lab", EntityType::Host, name).entity_id()
}

fn storage(name: &str) -> EntityId {
    EntityKey::new("lab", EntityType::Storage, name).entity_id()
}

/// A controller that has been told nothing about storage.
async fn controller(hosts: &[&str]) -> Controller {
    let mut text = String::from("config_version = 1\nenvironment = \"lab\"\n\n[controller]\nobserve = false\n");
    for name in hosts {
        text.push_str(&format!("\n[[entities]]\ntype = \"host\"\nname = \"{name}\"\n"));
    }

    let config = Config::from_toml(&text, std::path::Path::new("test.toml")).expect("config");
    let store = SqliteStore::open_in_memory().await.expect("store");
    Controller::new(config, store).await.expect("controller")
}

/// One host's mount table, as its agent reports it.
fn mount_report(client: &str, sources: &[&str]) -> Observation {
    let mounts: Vec<serde_json::Value> = sources
        .iter()
        .map(|source| {
            let (server, export) = source.split_once(":/").expect("server:/export");
            serde_json::json!({
                "source": source,
                "target": format!("/mnt/{export}"),
                "fstype": "nfs4",
                "server": server,
                "read_only": false,
            })
        })
        .collect();

    Observation::new(ProbeId::new(MOUNT), host(client), ProbeStatus::Ok)
        .with_payload(serde_json::json!({"mounts": mounts}))
}

#[tokio::test]
async fn the_graph_comes_from_the_mounts_the_agents_report() {
    let mut controller = controller(&["node01", "node02", "node03"]).await;
    controller.discover_once().await.expect("initial discovery");

    controller
        .ingest_observations(&[
            mount_report("node01", &["filesrv01:/data", "filesrv02:/home"]),
            mount_report("node02", &["filesrv01:/data", "filesrv02:/home"]),
            mount_report("node03", &["filesrv02:/home"]),
        ])
        .await
        .expect("observations");

    let report = controller.discover_once().await.expect("discovery");
    assert_eq!(report.storage_edges, 5, "{report:#?}");
    assert!(report.unresolved_storage_servers.is_empty());

    let inventory = controller.store().load_inventory("lab").await.expect("inventory");

    // The fileservers and their storage domains exist without being declared.
    for name in ["filesrv01", "filesrv02"] {
        assert!(inventory.get(host(name)).is_some(), "{name} was not created");
        assert!(inventory.get(storage(name)).is_some(), "storage/{name} was not created");
    }

    let graph = inventory.graph();
    let clients_of = |name: &str| -> Vec<String> {
        let mut names: Vec<String> = graph
            .downstream(storage(name), None)
            .into_iter()
            .filter_map(|r| inventory.get(r.entity))
            .filter(|e| e.entity_type == EntityType::Host)
            .map(|e| e.canonical_name.clone())
            .collect();
        names.sort();
        names
    };

    assert_eq!(clients_of("filesrv01"), vec!["node01", "node02"]);
    assert_eq!(clients_of("filesrv02"), vec!["node01", "node02", "node03"]);

    assert_eq!(
        graph
            .dependencies_of_type(storage("filesrv01"), &DependencyType::Provides)
            .iter()
            .map(|edge| edge.target)
            .collect::<Vec<_>>(),
        vec![host("filesrv01")],
        "so the suspected cause is a machine, not a concept"
    );
}

#[tokio::test]
async fn a_fileserver_failure_is_one_incident_naming_the_fileserver() {
    // The reason the graph is worth deriving at all. Without these edges the
    // same evidence produces one client-local fault per node and nothing that
    // points at the machine to go and look at.
    let mut controller = controller(&["node01", "node02", "node03"]).await;
    controller.discover_once().await.expect("initial discovery");

    controller
        .ingest_observations(&[
            mount_report("node01", &["filesrv01:/data"]),
            mount_report("node02", &["filesrv01:/data"]),
            mount_report("node03", &["filesrv02:/home"]),
        ])
        .await
        .expect("mount reports");
    controller.discover_once().await.expect("discovery");

    // Both clients of filesrv01 stop being able to use it. node03, on the
    // other fileserver, is fine.
    let mut failures = Vec::new();
    for _ in 0..4 {
        for client in ["node01", "node02"] {
            failures.push(
                Observation::new(ProbeId::new(IO), host(client), ProbeStatus::Failed)
                    .with_error("timeout", "the mount did not answer"),
            );
        }
    }
    controller.ingest_observations(&failures).await.expect("failures");

    let diagnoses = controller.diagnose().await.expect("diagnose");
    let shared: Vec<_> = diagnoses
        .iter()
        .filter(|d| d.is(kind::SHARED_STORAGE_FAILURE))
        .collect();

    assert_eq!(shared.len(), 1, "{diagnoses:#?}");
    assert!(
        shared[0].suspected_root_entities.contains(&host("filesrv01")),
        "the suspected cause must be the fileserver: {:?}",
        shared[0].suspected_root_entities
    );
    assert!(
        !shared[0].suspected_root_entities.contains(&host("filesrv02")),
        "the other fileserver has a healthy client and is innocent"
    );
}

#[tokio::test]
async fn a_mount_naming_an_unknown_address_is_reported_not_invented() {
    // An address is not an identity (ADR 0001): creating `host/10.0.0.9` would
    // give one machine a second, permanent identity the moment it registers
    // under its own name.
    let mut controller = controller(&["node01", "node02"]).await;
    controller.discover_once().await.expect("initial discovery");

    controller
        .ingest_observations(&[
            mount_report("node01", &["10.0.0.9:/data"]),
            mount_report("node02", &["10.0.0.9:/data"]),
        ])
        .await
        .expect("mount reports");

    let report = controller.discover_once().await.expect("discovery");

    assert_eq!(report.storage_edges, 0);
    assert_eq!(report.unresolved_storage_servers.len(), 1, "{report:#?}");
    assert_eq!(report.unresolved_storage_servers[0].server, "10.0.0.9");
    assert_eq!(
        report.unresolved_storage_servers[0].clients,
        vec!["node01".to_string(), "node02".to_string()],
        "and it says which hosts are affected, so the operator knows what to declare"
    );
}

#[tokio::test]
async fn unmounting_retracts_the_edge() {
    // Otherwise the graph only ever grows, and a node that moved to another
    // fileserver keeps voting in the old one's failures forever.
    let mut controller = controller(&["node01", "node02"]).await;
    controller.discover_once().await.expect("initial discovery");

    controller
        .ingest_observations(&[
            mount_report("node01", &["filesrv01:/data"]),
            mount_report("node02", &["filesrv01:/data"]),
        ])
        .await
        .expect("mount reports");
    assert_eq!(controller.discover_once().await.expect("discovery").storage_edges, 2);

    // node01 moves to the other fileserver.
    controller
        .ingest_observations(&[mount_report("node01", &["filesrv02:/data"])])
        .await
        .expect("mount reports");
    controller.discover_once().await.expect("discovery");

    let inventory = controller.store().load_inventory("lab").await.expect("inventory");
    let still_a_client = inventory
        .graph()
        .downstream(storage("filesrv01"), None)
        .into_iter()
        .any(|r| r.entity == host("node01"));

    assert!(!still_a_client, "node01 no longer mounts filesrv01");
}

#[tokio::test]
async fn declaring_the_topology_by_hand_still_works() {
    // Deriving must not take the option away: a fileserver with no agent and
    // no client that reports it is invisible to this and has to be declared.
    let text = r#"
config_version = 1
environment = "lab"

[controller]
observe = false

[discovery.nfs]
enabled = false

[[entities]]
type = "host"
name = "node01"

[[entities]]
type = "storage"
name = "declared"

[[dependencies]]
from = "host/node01"
to   = "storage/declared"
type = "uses_storage"
"#;
    let config = Config::from_toml(text, std::path::Path::new("test.toml")).expect("config");
    let store = SqliteStore::open_in_memory().await.expect("store");
    let mut controller = Controller::new(config, store).await.expect("controller");
    controller.discover_once().await.expect("initial discovery");

    controller
        .ingest_observations(&[mount_report("node01", &["filesrv01:/data"])])
        .await
        .expect("mount reports");
    let report = controller.discover_once().await.expect("discovery");

    assert_eq!(report.storage_edges, 0, "derivation is switched off");

    let inventory = controller.store().load_inventory("lab").await.expect("inventory");
    assert!(
        inventory
            .graph()
            .downstream(storage("declared"), None)
            .into_iter()
            .any(|r| r.entity == host("node01")),
        "the declared edge stands on its own"
    );
}
