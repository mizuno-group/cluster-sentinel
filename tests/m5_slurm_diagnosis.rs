//! M5 acceptance: Slurm faults are named, and not confused with each other.
//!
//! Three situations that look identical to an up/down monitor, and for which an
//! operator's response is completely different:
//!
//! * the node is drained — nothing is wrong with the machine;
//! * `slurmd` is dead — one daemon needs restarting;
//! * the host is gone — someone has to go and look at it.
//!
//! Getting these confused is expensive in a way monitoring bugs usually are
//! not: it sends a person to the wrong place at the wrong hour.

use sentinel::config::Config;
use sentinel::controller::Controller;
use sentinel::diagnosis::{kind, Confidence};
use sentinel::entity::{EntityKey, EntityType};
use sentinel::integrations::slurm::observe::observations_from_view;
use sentinel::integrations::slurm::{parser, SlurmView};
use sentinel::inventory::slurm::snapshot_from_view;
use sentinel::observation::{Observation, ProbeStatus};
use sentinel::persistence::SqliteStore;
use sentinel::probes::ProbeId;
use sentinel::state::{classification, Health, StateComponent};

async fn controller() -> Controller {
    let config = Config {
        config_version: 1,
        environment: "lab".into(),
        ..Config::default()
    };
    Controller::new(config, SqliteStore::open_in_memory().await.expect("store"))
        .await
        .expect("controller")
}

fn view(nodes: &str, ping: &str) -> SlurmView {
    SlurmView {
        nodes: parser::parse_nodes(nodes),
        partitions: Vec::new(),
        controllers: parser::parse_ping(ping),
    }
}

/// Feed the controller enough remote observations to establish a host's health.
async fn set_host_health(controller: &mut Controller, name: &str, healthy: bool) {
    let host = EntityKey::new("lab", EntityType::Host, name).entity_id();
    let status = if healthy { ProbeStatus::Ok } else { ProbeStatus::Failed };

    let mut observations = Vec::new();
    // Three rounds, because network-facing probes debounce.
    for _ in 0..3 {
        observations.push(Observation::new(
            ProbeId::new(sentinel::probes::network::PROBE_ID),
            host,
            status,
        ));
        observations.push(Observation::new(
            ProbeId::new(sentinel::probes::sentinel_rpc::PROBE_ID),
            host,
            status,
        ));
    }
    controller
        .ingest_observations(&observations)
        .await
        .expect("host observations");
}

/// Set up a cluster from Slurm output and observe it.
async fn cluster(nodes: &str, ping: &str) -> Controller {
    let mut controller = controller().await;
    let view = view(nodes, ping);
    controller
        .ingest_snapshot(&snapshot_from_view("lab", "test-cluster", &view))
        .await
        .expect("inventory");
    controller
        .ingest_observations(&observations_from_view("lab", "test-cluster", &view))
        .await
        .expect("slurm observations");
    controller
}

#[tokio::test]
async fn a_healthy_cluster_is_diagnosed_as_nothing_wrong() {
    // Silence is the right answer when nothing is broken. A monitor that always
    // finds something teaches people to stop reading it.
    let mut controller = cluster(
        "NodeName=n1 State=IDLE\nNodeName=n2 State=ALLOCATED\n",
        "Slurmctld(primary) at ctl-a is UP",
    )
    .await;
    set_host_health(&mut controller, "n1", true).await;
    set_host_health(&mut controller, "n2", true).await;

    assert!(controller.diagnose().await.expect("diagnose").is_empty());
}

#[tokio::test]
async fn a_drained_node_is_diagnosed_as_slurm_only() {
    // SPEC.md §169: host healthy, SSH healthy, agent healthy, slurmd healthy,
    // Slurm DRAIN.
    let mut controller = cluster(
        "NodeName=n1 State=IDLE+DRAIN Reason=scheduled maintenance [operator@2026-09-07T10:00:00]\n",
        "Slurmctld(primary) at ctl-a is UP",
    )
    .await;
    set_host_health(&mut controller, "n1", true).await;

    let diagnoses = controller.diagnose_and_classify().await.expect("diagnose");
    assert_eq!(diagnoses.len(), 1, "{diagnoses:#?}");

    let diagnosis = &diagnoses[0];
    assert!(diagnosis.is(kind::SLURM_ONLY_DEGRADATION));
    assert_eq!(diagnosis.confidence, Confidence::High);
    assert!(
        diagnosis.summary.contains("scheduled maintenance"),
        "{}",
        diagnosis.summary
    );

    // And the classification an operator sees.
    let host = EntityKey::new("lab", EntityType::Host, "n1").entity_id();
    let state = controller.engine().state(host).expect("state");
    assert!(state.has_classification(classification::SCHEDULER_DEGRADED));
    assert_eq!(state.component(StateComponent::Scheduler), Health::Degraded);
}

#[tokio::test]
async fn undraining_a_node_takes_its_classification_away() {
    // A label that outlives the fault it describes is worse than no label at
    // all: an operator cannot tell a stale one from a live one, so every label
    // on the status board becomes untrustworthy.
    let mut controller = cluster(
        "NodeName=n1 State=IDLE+DRAIN Reason=scheduled maintenance [operator@2026-09-07T10:00:00]\n",
        "Slurmctld(primary) at ctl-a is UP",
    )
    .await;
    set_host_health(&mut controller, "n1", true).await;

    controller.diagnose_and_classify().await.expect("diagnose");
    let host = EntityKey::new("lab", EntityType::Host, "n1").entity_id();
    assert!(controller
        .engine()
        .state(host)
        .expect("state")
        .has_classification(classification::SCHEDULER_DEGRADED));

    // The operator resumes the node.
    let view = view("NodeName=n1 State=IDLE\n", "Slurmctld(primary) at ctl-a is UP");
    controller
        .ingest_observations(&observations_from_view("lab", "test-cluster", &view))
        .await
        .expect("slurm observations");
    set_host_health(&mut controller, "n1", true).await;

    let diagnoses = controller.diagnose_and_classify().await.expect("diagnose");
    assert!(diagnoses.is_empty(), "{diagnoses:#?}");

    let state = controller.engine().state(host).expect("state");
    assert!(
        state.classifications.is_empty(),
        "classification outlived the drain: {:?}",
        state.classifications
    );
}

#[tokio::test]
async fn a_dead_slurmd_is_diagnosed_as_a_service_failure_not_a_host_failure() {
    // SPEC.md §170.
    let mut controller = cluster(
        "NodeName=n1 State=DOWN* Reason=Not responding [slurm@2026-09-07T10:00:00]\n",
        "Slurmctld(primary) at ctl-a is UP",
    )
    .await;
    set_host_health(&mut controller, "n1", true).await;

    let diagnoses = controller.diagnose().await.expect("diagnose");
    assert_eq!(diagnoses.len(), 1, "{diagnoses:#?}");
    assert!(diagnoses[0].is(kind::SLURMD_SERVICE_FAILURE));

    // The suspected cause is the daemon, not the machine.
    let service = EntityKey::new("lab", EntityType::Service, "slurmd@n1").entity_id();
    assert_eq!(diagnoses[0].suspected_root_entities, vec![service]);
}

#[tokio::test]
async fn a_dead_host_is_not_diagnosed_as_a_dead_daemon() {
    // The mistake that costs an hour: without evidence the machine is up,
    // "slurmd is dead" is a guess, and the likelier explanation is worse.
    let mut controller = cluster(
        "NodeName=n1 State=DOWN* Reason=Not responding\n",
        "Slurmctld(primary) at ctl-a is UP",
    )
    .await;
    set_host_health(&mut controller, "n1", false).await;

    let diagnoses = controller.diagnose().await.expect("diagnose");
    assert!(
        !diagnoses.iter().any(|d| d.is(kind::SLURMD_SERVICE_FAILURE)),
        "must not blame the daemon for an unreachable host: {diagnoses:#?}"
    );
}

#[tokio::test]
async fn the_three_situations_produce_three_different_answers() {
    // The claim of this milestone, in one test.
    let mut controller = cluster(
        "NodeName=drained State=IDLE+DRAIN Reason=maintenance\n\
         NodeName=daemon-dead State=DOWN* Reason=Not responding\n\
         NodeName=host-gone State=DOWN* Reason=Not responding\n\
         NodeName=fine State=IDLE\n",
        "Slurmctld(primary) at ctl-a is UP",
    )
    .await;

    set_host_health(&mut controller, "drained", true).await;
    set_host_health(&mut controller, "daemon-dead", true).await;
    set_host_health(&mut controller, "host-gone", false).await;
    set_host_health(&mut controller, "fine", true).await;

    let diagnoses = controller.diagnose().await.expect("diagnose");

    let for_host = |name: &str| {
        let id = EntityKey::new("lab", EntityType::Host, name).entity_id();
        diagnoses
            .iter()
            .filter(|d| d.affected_entities.contains(&id))
            .map(|d| d.diagnosis_type.to_string())
            .collect::<Vec<_>>()
    };

    assert_eq!(for_host("drained"), [kind::SLURM_ONLY_DEGRADATION]);
    assert_eq!(for_host("daemon-dead"), [kind::SLURMD_SERVICE_FAILURE]);
    assert!(
        for_host("host-gone").is_empty(),
        "an unreachable host awaits quorum, not a guess"
    );
    assert!(for_host("fine").is_empty());
}

#[tokio::test]
async fn a_failed_control_plane_is_diagnosed_as_such() {
    // SPEC.md §97.
    let mut controller = cluster("NodeName=n1 State=IDLE\n", "Slurmctld(primary) at ctl-a is DOWN").await;
    set_host_health(&mut controller, "n1", true).await;

    let diagnoses = controller.diagnose().await.expect("diagnose");
    assert!(
        diagnoses.iter().any(|d| d.is(kind::SLURM_CONTROL_PLANE_FAILURE)),
        "{diagnoses:#?}"
    );
}

#[tokio::test]
async fn a_gpu_count_mismatch_is_reported_without_calling_the_node_broken() {
    // The node may be working perfectly and simply be described wrongly.
    let mut controller = controller().await;
    let view = view("NodeName=n1 State=IDLE Gres=gpu:a100:4\n", "");

    let mut snapshot = snapshot_from_view("lab", "test-cluster", &view);
    for entity in &mut snapshot.entities {
        if entity.canonical_name == "n1" {
            entity.metadata = serde_json::json!({
                "hardware": { "gpus": 2, "cpus": 64 },
                "slurm": entity.metadata.get("slurm").cloned().unwrap_or(serde_json::Value::Null),
            });
        }
    }
    controller.ingest_snapshot(&snapshot).await.expect("inventory");
    controller
        .ingest_observations(&observations_from_view("lab", "test-cluster", &view))
        .await
        .expect("observations");

    let diagnoses = controller.diagnose().await.expect("diagnose");
    let mismatch = diagnoses
        .iter()
        .find(|d| d.is(kind::GPU_CONFIGURATION_MISMATCH))
        .expect("mismatch diagnosed");

    assert!(mismatch.summary.contains("4 GPU"), "{}", mismatch.summary);
    assert!(mismatch.summary.contains("2"), "{}", mismatch.summary);

    // The node itself is still schedulable and healthy; the configuration is
    // what is wrong.
    let host = EntityKey::new("lab", EntityType::Host, "n1").entity_id();
    assert_eq!(
        controller
            .engine()
            .state(host)
            .expect("state")
            .component(StateComponent::Scheduler),
        Health::Healthy
    );
}

#[tokio::test]
async fn every_diagnosis_can_be_traced_back_to_stored_observations() {
    // IMPLEMENTATION.md §72: a diagnosis that cannot be checked is an opinion.
    let mut controller = cluster(
        "NodeName=n1 State=IDLE+DRAIN Reason=maintenance\n",
        "Slurmctld(primary) at ctl-a is UP",
    )
    .await;
    set_host_health(&mut controller, "n1", true).await;

    let diagnoses = controller.diagnose().await.expect("diagnose");
    assert!(!diagnoses.is_empty());

    for diagnosis in &diagnoses {
        assert!(
            !diagnosis.evidence.is_empty(),
            "{} cites no evidence",
            diagnosis.diagnosis_type
        );
        assert!(!diagnosis.rule_id.as_str().is_empty());
        assert!(!diagnosis.summary.is_empty());

        for entity in &diagnosis.affected_entities {
            let stored: Vec<_> = controller
                .store()
                .recent_observations(*entity, 64)
                .await
                .expect("observations")
                .into_iter()
                .map(|o| o.id)
                .collect();
            let cited_here: Vec<_> = diagnosis.evidence.iter().filter(|id| stored.contains(id)).collect();
            assert!(
                !cited_here.is_empty(),
                "no cited observation belongs to an affected entity"
            );
        }
    }
}

#[tokio::test]
async fn no_diagnosis_ever_recommends_changing_anything() {
    // SPEC.md §113: v1 diagnoses, it does not act, and it does not tell an
    // operator to act before they have looked.
    let mut controller = cluster(
        "NodeName=drained State=IDLE+DRAIN Reason=maintenance\n\
         NodeName=daemon-dead State=DOWN* Reason=Not responding\n",
        "Slurmctld(primary) at ctl-a is DOWN",
    )
    .await;
    set_host_health(&mut controller, "drained", true).await;
    set_host_health(&mut controller, "daemon-dead", true).await;

    let diagnoses = controller.diagnose().await.expect("diagnose");
    assert!(!diagnoses.is_empty());

    for diagnosis in &diagnoses {
        for action in &diagnosis.recommended_actions {
            for mutating in [
                "scontrol update",
                "scontrol resume",
                "systemctl restart",
                "systemctl start",
                "reboot",
                "mount ",
            ] {
                assert!(
                    !action.contains(mutating),
                    "{} recommended a mutating command: {action}",
                    diagnosis.diagnosis_type
                );
            }
        }
    }
}

#[tokio::test]
async fn a_recovered_node_stops_being_diagnosed() {
    let mut controller = cluster(
        "NodeName=n1 State=IDLE+DRAIN Reason=maintenance\n",
        "Slurmctld(primary) at ctl-a is UP",
    )
    .await;
    set_host_health(&mut controller, "n1", true).await;
    assert_eq!(controller.diagnose().await.expect("diagnose").len(), 1);

    // The node comes back.
    let recovered = view("NodeName=n1 State=IDLE\n", "Slurmctld(primary) at ctl-a is UP");
    controller
        .ingest_observations(&observations_from_view("lab", "test-cluster", &recovered))
        .await
        .expect("observations");

    assert!(
        controller.diagnose().await.expect("diagnose").is_empty(),
        "a recovered node must stop being reported"
    );
}
