//! An observation is evidence about the moment it was taken.
//!
//! Diagnosis used to read "the last 32 observations per entity", which mixed
//! two questions it could not tell apart. The count bound how much was loaded,
//! and it also -- by accident -- bounded how old any of it could be. Replacing
//! it with "the newest of each probe, from each observer" fixed the first and
//! removed the second, and the difference showed up immediately: a host that
//! was switched off was reported as a broken network path, because an observer
//! that had stopped watching it kept its last "reachable" answer as the newest
//! one it would ever have.
//!
//! Staleness is relative to the probe. A reachability answer from a minute ago
//! is worthless; a Slurm node view from a minute ago is current. So the
//! horizon comes from each probe's own interval.

use sentinel::config::Config;
use sentinel::controller::Controller;
use sentinel::diagnosis::kind;
use sentinel::entity::{EntityId, EntityKey, EntityType};
use sentinel::observation::{Observation, ProbeStatus};
use sentinel::persistence::SqliteStore;
use sentinel::probes::ProbeId;

const NETWORK: &str = sentinel::probes::network::PROBE_ID;
const SLURM_NODE: &str = "slurm.node";

fn host(name: &str) -> EntityId {
    EntityKey::new("lab", EntityType::Host, name).entity_id()
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

/// What one observer saw of one target, `age` seconds ago.
fn saw(observer: &str, target: &str, reached: bool, age_s: i64) -> Observation {
    let status = if reached { ProbeStatus::Ok } else { ProbeStatus::Timeout };
    let outcome = if reached { "connected" } else { "timed_out" };
    let mut observation = Observation::new(ProbeId::new(NETWORK), host(target), status)
        .with_observer(host(observer))
        .with_payload(serde_json::json!({"host_responded": reached, "outcome": outcome}));
    observation.finished_at = sentinel::time::now() - chrono::Duration::seconds(age_s);
    observation
}

#[tokio::test]
async fn a_stale_reachable_answer_does_not_outvote_a_stopped_host() {
    // Caught by the pseudo-cluster acceptance run: a host was switched off,
    // every observer still watching it failed, and the verdict came back
    // PATH_SPECIFIC_NETWORK_FAILURE because one observer -- which had stopped
    // probing it -- still had a "reachable" answer from before the outage.
    let mut controller = controller(&["target", "a", "b", "c"]).await;

    let mut observations = Vec::new();
    for _ in 0..4 {
        observations.push(saw("a", "target", false, 0));
        observations.push(saw("b", "target", false, 0));
    }
    // c stopped watching, and its last word was from five minutes ago.
    observations.push(saw("c", "target", true, 300));

    controller.ingest_observations(&observations).await.expect("ingest");
    let diagnoses = controller.diagnose().await.expect("diagnose");

    let kinds: Vec<&str> = diagnoses.iter().map(|d| d.diagnosis_type.as_str()).collect();
    assert!(
        kinds.contains(&kind::HOST_UNREACHABLE),
        "nobody who is still looking can see it: {kinds:?}"
    );
    assert!(
        !kinds.contains(&kind::PATH_SPECIFIC_NETWORK_FAILURE),
        "a five-minute-old answer is not a second opinion about now: {kinds:?}"
    );
}

#[tokio::test]
async fn a_current_disagreement_is_still_a_path_fault() {
    // The bound must not swallow real disagreement: this is the case the
    // quorum design exists for, and it has to survive the fix for the other.
    let mut controller = controller(&["target", "a", "b", "c"]).await;

    let mut observations = Vec::new();
    for _ in 0..4 {
        observations.push(saw("a", "target", true, 0));
        observations.push(saw("b", "target", false, 0));
        observations.push(saw("c", "target", false, 0));
    }

    controller.ingest_observations(&observations).await.expect("ingest");
    let diagnoses = controller.diagnose().await.expect("diagnose");
    let kinds: Vec<&str> = diagnoses.iter().map(|d| d.diagnosis_type.as_str()).collect();

    assert!(kinds.contains(&kind::PATH_SPECIFIC_NETWORK_FAILURE), "{kinds:?}");
    assert!(!kinds.contains(&kind::HOST_UNREACHABLE), "{kinds:?}");
}

#[tokio::test]
async fn a_slow_probes_answer_is_still_current_when_a_fast_one_would_be_stale() {
    // The other half. The Slurm node view arrives once per discovery cycle, so
    // a five-minute-old one is the newest there is and must still count -- the
    // horizon is each probe's own cadence, not one number for all of them.
    let mut controller = controller(&["node01"]).await;

    let mut slurm = Observation::new(ProbeId::new(SLURM_NODE), host("node01"), ProbeStatus::Ok)
        .with_payload(serde_json::json!({"configured_gpu_count": 4, "cpu_total": 64, "schedulable": true}));
    slurm.finished_at = sentinel::time::now() - chrono::Duration::seconds(300);

    controller.ingest_observations(&[slurm]).await.expect("ingest");

    // A GPU disagreement can only be diagnosed if the Slurm side survived.
    let mut inventory = controller.store().load_inventory("lab").await.expect("inventory");
    let mut entity = inventory.get(host("node01")).expect("node01").clone();
    entity.metadata = serde_json::json!({"hardware": {"gpus": 2, "cpus": 64}});
    inventory.insert_entity(entity);
    controller.store().save_inventory(&inventory).await.expect("save");

    let diagnoses = controller.diagnose().await.expect("diagnose");
    let kinds: Vec<&str> = diagnoses.iter().map(|d| d.diagnosis_type.as_str()).collect();

    assert!(
        kinds.contains(&kind::GPU_CONFIGURATION_MISMATCH),
        "a five-minute-old view from a five-minute probe is current: {kinds:?}"
    );
}
