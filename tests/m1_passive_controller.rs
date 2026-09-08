//! M1 acceptance: a passive controller discovers a cluster and `sentinel
//! status` explains it.
//!
//! The whole path is exercised — `scontrol` invocation, parsing, inventory
//! merge, observation storage, state derivation, presentation — using a
//! stand-in `scontrol` that replays the saved fixtures. Nothing here needs a
//! Slurm installation, and nothing here mocks Sentinel's own code.

use std::path::{Path, PathBuf};

use sentinel::cli::StatusReport;
use sentinel::config::Config;
use sentinel::controller::Controller;
use sentinel::entity::{EntityKey, EntityType};
use sentinel::persistence::SqliteStore;
use sentinel::state::{Health, StateComponent};

/// Write an executable stand-in for `scontrol` that replays fixtures.
///
/// It must be named `scontrol`: the command allowlist matches on the file name,
/// so this also demonstrates that an operator may point at a non-standard
/// install path without widening the allowlist.
fn fake_scontrol(dir: &Path, ping_fixture: &str) -> PathBuf {
    let fixtures = Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures/slurm");
    let path = dir.join("scontrol");

    let script = format!(
        r#"#!/bin/sh
case "$1 $2" in
  "show nodes")      cat "{fixtures}/show_nodes.txt" ;;
  "show partitions") cat "{fixtures}/show_partitions.txt" ;;
  *) case "$1" in
       ping) cat "{fixtures}/{ping_fixture}" ;;
       *) echo "unexpected: $*" >&2; exit 1 ;;
     esac ;;
esac
"#,
        fixtures = fixtures.display()
    );

    std::fs::write(&path, script).expect("write fake scontrol");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).expect("chmod");
    }
    path
}

fn config_for(dir: &Path, scontrol: &Path, extra: &str) -> Config {
    let toml = format!(
        r#"
config_version = 1
environment = "lab"

[controller]
# This suite is about Slurm discovery. Remote probing of fixture host names
# that do not resolve would spend a connection timeout each and assert nothing;
# observation is covered by the M4 acceptance tests.
observe = false

[database]
path = "{db}"

[discovery.slurm]
enabled = true
scontrol_path = "{scontrol}"

[[entities]]
type = "scheduler"
name = "test-cluster"

{extra}
"#,
        db = dir.join("sentinel.db").display(),
        scontrol = scontrol.display(),
    );
    Config::from_toml(&toml, Path::new("test.toml")).expect("parse config")
}

async fn run_cycle(config: Config) -> (SqliteStore, StatusReport) {
    let store = SqliteStore::open(&config.database.path).await.expect("open store");
    let mut controller = Controller::new(config.clone(), store.clone())
        .await
        .expect("controller");

    let report = controller.discover_once().await.expect("discovery cycle");
    assert!(
        report.all_providers_ok(),
        "providers failed: {:?}",
        report.failures().collect::<Vec<_>>()
    );

    let inventory = store.load_inventory(&config.environment).await.expect("inventory");
    let states = store.load_entity_states(&config.environment).await.expect("states");
    let status = StatusReport::build(&config.environment, &inventory, &states);
    (store, status)
}

fn status_of(report: &StatusReport, entity_type: &str, name: &str) -> String {
    report
        .entities
        .iter()
        .find(|e| e.entity_type == entity_type && e.name == name)
        .unwrap_or_else(|| panic!("no {entity_type}/{name} in {:?}", report.entities))
        .health
        .clone()
}

#[tokio::test]
async fn a_discovery_cycle_builds_the_whole_picture_from_slurm() {
    let dir = tempfile::tempdir().expect("tempdir");
    let scontrol = fake_scontrol(dir.path(), "ping_up.txt");
    let (_store, report) = run_cycle(config_for(dir.path(), &scontrol, "")).await;

    // Four compute nodes, one controller host, one scheduler, five services.
    let count = |ty: &str| report.entities.iter().filter(|e| e.entity_type == ty).count();
    assert_eq!(count("host"), 5);
    assert_eq!(count("scheduler"), 1);
    assert_eq!(count("service"), 5);
}

#[tokio::test]
async fn the_four_slurm_states_produce_four_different_verdicts() {
    // This is the whole point of the milestone: not up/down, but *which* kind
    // of not-up.
    let dir = tempfile::tempdir().expect("tempdir");
    let scontrol = fake_scontrol(dir.path(), "ping_up.txt");
    let (_store, report) = run_cycle(config_for(dir.path(), &scontrol, "")).await;

    assert_eq!(status_of(&report, "host", "compute-01"), "healthy", "IDLE");
    assert_eq!(
        status_of(&report, "host", "compute-02"),
        "healthy",
        "MIXED is still usable"
    );
    assert_eq!(
        status_of(&report, "host", "compute-03"),
        "degraded",
        "IDLE+DRAIN: the machine works, the scheduler will not use it"
    );
    assert_eq!(status_of(&report, "host", "compute-04"), "unavailable", "DOWN*");
}

#[tokio::test]
async fn a_drained_node_is_degraded_on_the_scheduler_component_only() {
    // Nothing has probed the host itself, so no other component may claim to
    // know anything about it.
    let dir = tempfile::tempdir().expect("tempdir");
    let scontrol = fake_scontrol(dir.path(), "ping_up.txt");
    let (store, _) = run_cycle(config_for(dir.path(), &scontrol, "")).await;

    let host = EntityKey::new("lab", EntityType::Host, "compute-03").entity_id();
    let states = store.load_entity_states("lab").await.expect("states");
    let state = states.get(&host).expect("state");

    assert_eq!(state.component(StateComponent::Scheduler), Health::Degraded);
    assert_eq!(state.component(StateComponent::Host), Health::NotApplicable);
    assert_eq!(state.component(StateComponent::Ssh), Health::NotApplicable);
    assert_eq!(state.component(StateComponent::Availability), Health::NotApplicable);
}

#[tokio::test]
async fn a_node_slurm_calls_down_is_not_declared_unreachable() {
    // SPEC.md §52 and §175: Slurm saying DOWN is one observer's opinion about
    // scheduling, not evidence about the machine or its power state.
    let dir = tempfile::tempdir().expect("tempdir");
    let scontrol = fake_scontrol(dir.path(), "ping_up.txt");
    let (store, report) = run_cycle(config_for(dir.path(), &scontrol, "")).await;

    let entity = report.entities.iter().find(|e| e.name == "compute-04").expect("node");
    assert!(
        !entity
            .classifications
            .iter()
            .any(|c| c.contains("UNREACHABLE") || c.contains("POWER")),
        "unexpected classification: {:?}",
        entity.classifications
    );

    let host = EntityKey::new("lab", EntityType::Host, "compute-04").entity_id();
    let observations = store.recent_observations(host, 10).await.expect("observations");
    assert_eq!(observations.len(), 1);
    assert_eq!(observations[0].probe_id.as_str(), "slurm.node");
    assert_eq!(observations[0].payload["state"], "DOWN*");
}

#[tokio::test]
async fn the_scheduler_and_its_controller_daemon_are_reported_separately() {
    let dir = tempfile::tempdir().expect("tempdir");
    let scontrol = fake_scontrol(dir.path(), "ping_up.txt");
    let (_store, report) = run_cycle(config_for(dir.path(), &scontrol, "")).await;

    assert_eq!(status_of(&report, "scheduler", "test-cluster"), "healthy");
    assert_eq!(status_of(&report, "service", "slurmctld@ctl-a"), "healthy");
    assert_eq!(
        status_of(&report, "host", "ctl-a"),
        "unknown",
        "nothing has probed the controller host itself yet"
    );
}

#[tokio::test]
async fn a_down_backup_controller_is_visible_without_condemning_the_scheduler() {
    let dir = tempfile::tempdir().expect("tempdir");
    let scontrol = fake_scontrol(dir.path(), "ping_ha.txt");
    let (_store, report) = run_cycle(config_for(dir.path(), &scontrol, "")).await;

    assert_eq!(status_of(&report, "service", "slurmctld@ctl-a"), "healthy");
    assert_eq!(status_of(&report, "service", "slurmctld@ctl-b"), "unavailable");
}

#[tokio::test]
async fn the_dependency_graph_links_nodes_through_services_to_the_scheduler() {
    let dir = tempfile::tempdir().expect("tempdir");
    let scontrol = fake_scontrol(dir.path(), "ping_up.txt");
    let (store, _) = run_cycle(config_for(dir.path(), &scontrol, "")).await;

    let inventory = store.load_inventory("lab").await.expect("inventory");
    let graph = inventory.graph();

    let node = EntityKey::new("lab", EntityType::Host, "compute-01").entity_id();
    let scheduler = EntityKey::new("lab", EntityType::Scheduler, "test-cluster").entity_id();
    let slurmctld = EntityKey::new("lab", EntityType::Service, "slurmctld@ctl-a").entity_id();

    let upstream: Vec<_> = graph.upstream(node, None).into_iter().map(|r| r.entity).collect();
    assert!(upstream.contains(&scheduler), "a node depends on the scheduler");
    assert!(
        upstream.contains(&slurmctld),
        "and transitively on the daemon providing it"
    );

    // Blast radius the other way: all four compute nodes hang off the daemon.
    let downstream: Vec<_> = graph
        .downstream(slurmctld, None)
        .into_iter()
        .map(|r| r.entity)
        .collect();
    assert!(downstream.contains(&node));
}

#[tokio::test]
async fn static_entities_coexist_with_slurm_discovered_ones() {
    // SPEC.md §180: a host Slurm has never heard of is a first-class citizen.
    let dir = tempfile::tempdir().expect("tempdir");
    let scontrol = fake_scontrol(dir.path(), "ping_up.txt");
    let config = config_for(
        dir.path(),
        &scontrol,
        r#"
[[entities]]
type = "host"
name = "fileserver-a"
capabilities = ["storage.nfs.server"]

[[entities]]
type = "storage"
name = "shared-a"

[[dependencies]]
from = "host/compute-01"
to = "storage/shared-a"
type = "uses_storage"

[[dependencies]]
from = "storage/shared-a"
to = "host/fileserver-a"
type = "provides"
"#,
    );
    let (store, report) = run_cycle(config).await;

    assert!(report.entities.iter().any(|e| e.name == "fileserver-a"));
    assert!(report.entities.iter().any(|e| e.name == "shared-a"));
    assert!(report.entities.iter().any(|e| e.name == "compute-01"));

    // A Slurm-discovered node correctly depends on a statically declared
    // storage domain, through to the fileserver behind it.
    let inventory = store.load_inventory("lab").await.expect("inventory");
    let node = EntityKey::new("lab", EntityType::Host, "compute-01").entity_id();
    let fileserver = EntityKey::new("lab", EntityType::Host, "fileserver-a").entity_id();
    let upstream: Vec<_> = inventory
        .graph()
        .upstream(node, None)
        .into_iter()
        .map(|r| r.entity)
        .collect();
    assert!(upstream.contains(&fileserver));
}

#[tokio::test]
async fn a_host_reported_by_two_providers_is_one_entity() {
    let dir = tempfile::tempdir().expect("tempdir");
    let scontrol = fake_scontrol(dir.path(), "ping_up.txt");
    let config = config_for(
        dir.path(),
        &scontrol,
        "\n[[entities]]\ntype = \"host\"\nname = \"compute-01\"\ncapabilities = [\"observer.peer\"]\n",
    );
    let (store, report) = run_cycle(config).await;

    assert_eq!(report.entities.iter().filter(|e| e.name == "compute-01").count(), 1);

    let entities = store.load_entities("lab").await.expect("entities");
    let node = entities
        .iter()
        .find(|e| e.canonical_name == "compute-01")
        .expect("node");
    assert!(node.capabilities.has("observer.peer"), "from configuration");
    assert!(node.capabilities.has("slurm.compute"), "from Slurm");
    assert_eq!(node.discovery_sources.len(), 2);
}

#[tokio::test]
async fn a_second_cycle_changes_nothing_and_duplicates_nothing() {
    let dir = tempfile::tempdir().expect("tempdir");
    let scontrol = fake_scontrol(dir.path(), "ping_up.txt");
    let config = config_for(dir.path(), &scontrol, "");

    let store = SqliteStore::open(&config.database.path).await.expect("store");
    let mut controller = Controller::new(config.clone(), store.clone())
        .await
        .expect("controller");

    let first = controller.discover_once().await.expect("first cycle");
    let second = controller.discover_once().await.expect("second cycle");

    assert_eq!(first.entities, second.entities, "inventory converged");
    assert_eq!(second.transitions.len(), 0, "unchanged state produces no transitions");
    assert!(second.observations > 0, "but observations keep accruing as history");
}

#[tokio::test]
async fn losing_slurm_does_not_wipe_the_inventory_or_rewrite_state_as_healthy() {
    // SPEC.md §31 and §176: not being able to look is not the same as finding
    // nothing wrong.
    let dir = tempfile::tempdir().expect("tempdir");
    let scontrol = fake_scontrol(dir.path(), "ping_up.txt");
    let config = config_for(dir.path(), &scontrol, "");

    let store = SqliteStore::open(&config.database.path).await.expect("store");
    let mut controller = Controller::new(config.clone(), store.clone())
        .await
        .expect("controller");
    controller.discover_once().await.expect("first cycle");

    // Break scontrol, then run again.
    std::fs::write(&scontrol, "#!/bin/sh\nexit 1\n").expect("break scontrol");
    let report = controller.discover_once().await.expect("second cycle");

    assert!(!report.all_providers_ok(), "the failure must be reported");
    assert_eq!(report.entities, 11, "every entity is still known");

    let states = store.load_entity_states("lab").await.expect("states");
    let drained = EntityKey::new("lab", EntityType::Host, "compute-03").entity_id();
    assert_eq!(
        states
            .get(&drained)
            .expect("state")
            .component(StateComponent::Scheduler),
        Health::Degraded,
        "the last known verdict stands; it must not be reset to healthy"
    );
}

#[tokio::test]
async fn the_rendered_status_names_the_degraded_nodes() {
    let dir = tempfile::tempdir().expect("tempdir");
    let scontrol = fake_scontrol(dir.path(), "ping_up.txt");
    let (_store, report) = run_cycle(config_for(dir.path(), &scontrol, "")).await;

    assert!(!report.is_healthy());
    let problems: Vec<&str> = report.problems().map(|e| e.name.as_str()).collect();
    assert!(problems.contains(&"compute-03"));
    assert!(problems.contains(&"compute-04"));
    assert!(!problems.contains(&"compute-01"));
}
