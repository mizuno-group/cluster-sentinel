//! Restarting the controller must not wake anyone up.
//!
//! Reported from a live cluster: every `systemctl restart sentinel-controller`
//! produced a GPU warning for every node, each of which resolved seconds
//! later. Two mechanisms met to cause it.
//!
//! The agent registry lives in memory, so after a restart every agent's next
//! heartbeat is answered with `reregister: true` and the whole fleet registers
//! again at once. That part is deliberate: a session the controller cannot
//! vouch for should not be silently accepted.
//!
//! What made it a page was what the registration said. `registration()` looked
//! at whether the host had the NVIDIA capability and then reported a hardcoded
//! zero, so every re-registration simultaneously overwrote every GPU node's
//! hardware with "zero GPUs" -- and the scheduler-comparison rule dutifully
//! reported a fleet-wide disagreement with Slurm. It then cleared itself when
//! the Slurm observation fell out of the old fixed-size observation window,
//! which is what made each one resolve moments after it opened.

use sentinel::config::Config;
use sentinel::controller::Controller;
use sentinel::diagnosis::kind;
use sentinel::entity::{EntityId, EntityKey, EntityType};
use sentinel::observation::{Observation, ProbeStatus};
use sentinel::persistence::SqliteStore;
use sentinel::probes::ProbeId;
use sentinel::protocol::RegisterRequest;

const SLURM_NODE: &str = "slurm.node";
const GPU: &str = sentinel::probes::gpu::PROBE_ID;

fn host(name: &str) -> EntityId {
    EntityKey::new("lab", EntityType::Host, name).entity_id()
}

fn config() -> Config {
    Config::from_toml(
        "config_version = 1\nenvironment = \"lab\"\n\n[controller]\nobserve = false\n",
        std::path::Path::new("test.toml"),
    )
    .expect("config")
}

/// What a GPU node's agent sends, with `gpus` as the agent would report it.
fn registration(name: &str, gpus: Option<u32>) -> RegisterRequest {
    RegisterRequest {
        protocol_version: sentinel::PROTOCOL_VERSION,
        agent_version: sentinel::VERSION.to_string(),
        environment: "lab".to_string(),
        hostname: name.to_string(),
        fqdn: None,
        boot_id: Some("boot-1".to_string()),
        addresses: vec!["192.0.2.10".to_string()],
        ports: Default::default(),
        capabilities: ["gpu.nvidia", "sentinel.agent"].into_iter().collect(),
        hardware: serde_json::json!({"cpus": 64, "memory_mb": 512000, "gpus": gpus}),
        roles: Vec::new(),
    }
}

/// Slurm's view: this node is configured with one GPU.
fn slurm_view(name: &str) -> Observation {
    Observation::new(ProbeId::new(SLURM_NODE), host(name), ProbeStatus::Ok)
        .with_payload(serde_json::json!({"configured_gpu_count": 1, "cpu_total": 64, "schedulable": true}))
}

/// The agent's GPU probe, having counted the card that is really there.
fn gpu_report(name: &str) -> Observation {
    Observation::new(ProbeId::new(GPU), host(name), ProbeStatus::Ok)
        .with_payload(serde_json::json!({"gpu_count": 1, "expected_gpu_count": 1}))
}

const NODES: [&str; 4] = ["gpu01", "gpu02", "gpu03", "gpu04"];

#[tokio::test]
async fn a_controller_restart_does_not_wake_the_fleet_over_its_gpus() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("sentinel.db");

    // Before the restart: every node registered, counted its GPU, and agreed
    // with Slurm.
    {
        let store = SqliteStore::open(&path).await.expect("store");
        let mut controller = Controller::new(config(), store).await.expect("controller");
        for name in NODES {
            controller
                .register_agent(&registration(name, Some(1)))
                .await
                .expect("register");
            controller
                .ingest_observations(&[slurm_view(name), gpu_report(name)])
                .await
                .expect("observations");
        }
        assert!(
            controller.diagnose().await.expect("diagnose").is_empty(),
            "the baseline must be quiet, or this test proves nothing"
        );
    }

    // The controller restarts. Its agent registry was in memory, so every
    // agent is told to register again, and they all do at once.
    let store = SqliteStore::open(&path).await.expect("store");
    let mut controller = Controller::new(config(), store).await.expect("controller");
    for name in NODES {
        controller
            .register_agent(&registration(name, Some(1)))
            .await
            .expect("re-register");
    }

    let diagnoses = controller.diagnose().await.expect("diagnose");
    assert!(
        diagnoses.is_empty(),
        "a restart is not a fault: {:#?}",
        diagnoses.iter().map(|d| d.summary.clone()).collect::<Vec<_>>()
    );
}

#[tokio::test]
async fn an_agent_that_has_not_counted_its_gpus_yet_does_not_contradict_slurm() {
    // A restarted agent has counted nothing until its probe first runs, so its
    // registration says nothing about GPUs. "Not stated" must stay out of the
    // comparison -- reporting zero here is what produced the fleet-wide
    // warning, because every agent re-registers at once after a restart and
    // every one of them said zero.
    let store = SqliteStore::open_in_memory().await.expect("store");
    let mut controller = Controller::new(config(), store).await.expect("controller");

    for name in NODES {
        controller
            .register_agent(&registration(name, None))
            .await
            .expect("register");
        controller
            .ingest_observations(&[slurm_view(name)])
            .await
            .expect("observations");
    }

    let diagnoses = controller.diagnose().await.expect("diagnose");
    assert!(
        !diagnoses.iter().any(|d| d.is(kind::GPU_CONFIGURATION_MISMATCH)),
        "{:#?}",
        diagnoses.iter().map(|d| d.summary.clone()).collect::<Vec<_>>()
    );
}

#[tokio::test]
async fn a_real_gpu_disagreement_is_still_reported() {
    // The bound must not be bought by going blind: a node that genuinely has
    // fewer cards than Slurm was told about is still worth saying.
    let store = SqliteStore::open_in_memory().await.expect("store");
    let mut controller = Controller::new(config(), store).await.expect("controller");

    controller
        .register_agent(&registration("gpu01", Some(0)))
        .await
        .expect("register");
    controller
        .ingest_observations(&[slurm_view("gpu01")])
        .await
        .expect("observations");

    let diagnoses = controller.diagnose().await.expect("diagnose");
    assert!(
        diagnoses.iter().any(|d| d.is(kind::GPU_CONFIGURATION_MISMATCH)),
        "an agent that counted zero cards where Slurm expects one is a real finding: {:#?}",
        diagnoses.iter().map(|d| d.summary.clone()).collect::<Vec<_>>()
    );
}
