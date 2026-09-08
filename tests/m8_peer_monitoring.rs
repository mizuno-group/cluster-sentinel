//! M8 acceptance: several viewpoints, and what they together justify.
//!
//! This is the milestone the distributed design exists for. Until now Sentinel
//! could see that it could not reach a host, and had to stop there. With
//! independent observers it can finally say which of two very different things
//! is happening — and, just as importantly, still refuse to say when the
//! evidence does not support either.

use std::collections::HashMap;

use sentinel::agent::peer_probes::assigned_target;
use sentinel::agent::PeerProbes;
use sentinel::capability::{well_known, CapabilitySet};
use sentinel::config::Config;
use sentinel::controller::{observer_candidates_for_test, Controller};
use sentinel::diagnosis::{builtin_rules, kind, Confidence, DiagnosisContext, ObservationIndex};
use sentinel::entity::{DiscoverySource, EntityId, EntityKey, EntityType, ManagedEntity};
use sentinel::inventory::{Inventory, InventorySnapshot};
use sentinel::observation::{Observation, ProbeStatus};
use sentinel::persistence::SqliteStore;
use sentinel::probes::network::PROBE_ID as NETWORK_PROBE;
use sentinel::probes::ProbeId;
use sentinel::state::EntityState;

fn host(name: &str) -> EntityId {
    EntityKey::new("lab", EntityType::Host, name).entity_id()
}

/// Build a world from a set of observer verdicts.
fn diagnose(target: &str, verdicts: &[(&str, bool)]) -> Vec<sentinel::diagnosis::Diagnosis> {
    let mut inventory = Inventory::new();
    inventory.insert_entity(
        ManagedEntity::new("lab", EntityType::Host, target)
            .with_capabilities(CapabilitySet::from_iter(["network.tcp"])),
    );

    let mut observations = ObservationIndex::new();
    for (observer, responded) in verdicts {
        inventory.insert_entity(
            ManagedEntity::new("lab", EntityType::Host, *observer)
                .with_capabilities(CapabilitySet::from_iter([well_known::OBSERVER_PEER])),
        );
        let status = if *responded {
            ProbeStatus::Ok
        } else {
            ProbeStatus::Timeout
        };
        observations.insert(
            Observation::new(ProbeId::new(NETWORK_PROBE), host(target), status)
                .with_observer(host(observer))
                .with_payload(serde_json::json!({"host_responded": responded})),
        );
    }

    let states: HashMap<EntityId, EntityState> = HashMap::new();
    let context = DiagnosisContext {
        environment: "lab",
        inventory: &inventory,
        states: &states,
        observations: &observations,
    };
    builtin_rules().diagnose(&context)
}

fn types(diagnoses: &[sentinel::diagnosis::Diagnosis]) -> Vec<String> {
    diagnoses.iter().map(|d| d.diagnosis_type.to_string()).collect()
}

#[test]
fn all_observers_failing_gives_host_unreachable() {
    // SPEC.md §51.
    let diagnoses = diagnose("target", &[("a", false), ("b", false), ("c", false)]);
    assert!(
        types(&diagnoses).contains(&kind::HOST_UNREACHABLE.to_string()),
        "{:?}",
        types(&diagnoses)
    );

    let unreachable = diagnoses
        .iter()
        .find(|d| d.is(kind::HOST_UNREACHABLE))
        .expect("diagnosed");
    assert_eq!(unreachable.confidence, Confidence::High);
}

#[test]
fn one_observer_disagreeing_gives_a_path_failure_instead() {
    // SPEC.md §50 and §175: the controller's blind spot is not the host's fault.
    let diagnoses = diagnose("target", &[("controller", false), ("b", true), ("c", true)]);
    let names = types(&diagnoses);

    assert!(
        names.contains(&kind::PATH_SPECIFIC_NETWORK_FAILURE.to_string()),
        "{names:?}"
    );
    assert!(
        !names.contains(&kind::HOST_UNREACHABLE.to_string()),
        "the host is demonstrably up; declaring it unreachable would send someone to a working machine"
    );

    let path = diagnoses
        .iter()
        .find(|d| d.is(kind::PATH_SPECIFIC_NETWORK_FAILURE))
        .expect("diagnosed");
    assert!(path.summary.contains("controller"), "{}", path.summary);
    assert!(path.summary.contains("the host is up"), "{}", path.summary);
}

#[test]
fn a_single_observer_justifies_no_reachability_conclusion() {
    // The restraint that distinguishes this design from an up/down monitor.
    let diagnoses = diagnose("target", &[("a", false)]);
    let names = types(&diagnoses);

    assert!(!names.contains(&kind::HOST_UNREACHABLE.to_string()));
    assert!(!names.contains(&kind::PATH_SPECIFIC_NETWORK_FAILURE.to_string()));
}

#[test]
fn a_healthy_host_produces_nothing() {
    assert!(diagnose("target", &[("a", true), ("b", true), ("c", true)]).is_empty());
}

#[test]
fn unreachability_never_becomes_a_claim_about_power() {
    // SPEC.md §52. A host that is off, one with a dead NIC and one behind a
    // failed switch are the same picture from here.
    let diagnoses = diagnose("target", &[("a", false), ("b", false)]);
    let unreachable = diagnoses
        .iter()
        .find(|d| d.is(kind::HOST_UNREACHABLE))
        .expect("diagnosed");

    assert!(unreachable.confidence < Confidence::Confirmed);

    let rendered = format!(
        "{} {} {:?}",
        unreachable.diagnosis_type, unreachable.summary, unreachable.recommended_actions
    );
    for claim in ["POWER_OFF", "powered off", "is off"] {
        assert!(!rendered.contains(claim), "{rendered}");
    }
}

#[tokio::test]
async fn the_controller_assigns_observers_from_different_domains() {
    // SPEC.md §48: three observers behind the same storage are one viewpoint.
    let mut config = Config {
        config_version: 1,
        environment: "lab".into(),
        ..Config::default()
    };
    config.controller.observe = false;

    let store = SqliteStore::open_in_memory().await.expect("store");
    let mut controller = Controller::new(config, store).await.expect("controller");

    let mut snapshot = InventorySnapshot::new(DiscoverySource::StaticConfig);
    for name in ["c1", "c2", "c3", "fs1"] {
        snapshot.add_entity(
            ManagedEntity::new("lab", EntityType::Host, name)
                .with_capabilities(CapabilitySet::from_iter([well_known::OBSERVER_PEER])),
        );
    }
    snapshot.add_entity(ManagedEntity::new("lab", EntityType::Storage, "s1"));
    for client in ["c1", "c2"] {
        snapshot.add_dependency(sentinel::dependency::DependencyEdge::new(
            host(client),
            EntityKey::new("lab", EntityType::Storage, "s1").entity_id(),
            sentinel::dependency::DependencyType::UsesStorage,
        ));
    }
    controller.ingest_snapshot(&snapshot).await.expect("inventory");

    let plan = controller.assignment_plan().await.expect("plan");
    let assignment = plan.for_target(host("c1")).expect("c1 is assigned observers");

    assert!(!assignment.is_observed_by(host("c1")), "never itself");
    assert!(
        assignment.independent_viewpoints() >= 2,
        "observers must be unlike each other: {assignment:?}"
    );
}

#[tokio::test]
async fn an_agent_observes_the_peers_it_is_assigned_and_signs_its_observations() {
    // The observations that make quorum possible must say who made them.
    let port = {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("bind");
        listener.local_addr().expect("addr").port()
    };

    let mut peers = PeerProbes::new(host("watcher"));
    peers.set_targets(
        1,
        vec![assigned_target(
            host("target"),
            "target",
            "127.0.0.1",
            CapabilitySet::from_iter(["network.tcp", "ssh.server"]),
            serde_json::json!({"address": "127.0.0.1", "port": port, "ssh_port": port}),
        )],
    );

    let observations = peers.observe_all().await;
    assert!(!observations.is_empty());
    for observation in &observations {
        assert_eq!(observation.observer_entity, Some(host("watcher")));
        assert_eq!(observation.target_entity, host("target"));
    }
}

#[tokio::test]
async fn two_agents_watching_one_target_produce_two_distinguishable_verdicts() {
    // The end-to-end shape of quorum: two observers, two observations, and a
    // diagnosis that depends on their disagreement.
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let live_port = listener.local_addr().expect("addr").port();
    tokio::spawn(async move { while listener.accept().await.is_ok() {} });

    let dead_port = {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("bind");
        listener.local_addr().expect("addr").port()
    };

    // One observer reaches the target; the other is pointed at a closed port
    // that nothing answers, standing in for a broken path.
    let mut seeing = PeerProbes::new(host("seeing"));
    seeing.set_targets(
        1,
        vec![assigned_target(
            host("target"),
            "target",
            "127.0.0.1",
            CapabilitySet::from_iter(["network.tcp"]),
            serde_json::json!({"address": "127.0.0.1", "port": live_port}),
        )],
    );

    let mut blind = PeerProbes::new(host("blind"));
    blind.set_targets(
        1,
        vec![assigned_target(
            host("target"),
            "target",
            "203.0.113.1",
            CapabilitySet::from_iter(["network.tcp"]),
            serde_json::json!({"address": "203.0.113.1", "port": dead_port}),
        )],
    );

    let mut observations = seeing.observe_all().await;
    observations.extend(blind.observe_all().await);

    // Both verdicts survive into the index, keyed by observer.
    let index = ObservationIndex::from_observations(observations);
    let all = index.all(host("target"), NETWORK_PROBE);
    assert_eq!(all.len(), 2, "one observer must not overwrite the other");

    let observers: std::collections::BTreeSet<_> = all.iter().filter_map(|o| o.observer_entity).collect();
    assert_eq!(observers.len(), 2);
}

#[test]
fn the_observer_candidate_set_is_capability_gated() {
    // SPEC.md §15, at the point where it decides who gets to vouch for whom.
    let entities = vec![
        ManagedEntity::new("lab", EntityType::Host, "watcher")
            .with_capabilities(CapabilitySet::from_iter([well_known::OBSERVER_PEER])),
        ManagedEntity::new("lab", EntityType::Host, "labelled").with_label("role", "observer"),
    ];

    let candidates = observer_candidates_for_test(&entities);
    assert_eq!(
        candidates,
        vec![host("watcher")],
        "a label must not make something an observer"
    );
}
