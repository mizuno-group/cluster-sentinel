//! Service entities must be observed by the agent on the host that runs them.
//!
//! A service is its own entity so that "the daemon died" and "the machine
//! died" can be different answers. That distinction has nowhere to live if
//! nothing ever looks at the service: on a real cluster every `slurmd@node`
//! read UNKNOWN for ever, while the hosts beside them were healthy.
//!
//! Only the agent can answer it. Asking systemd about a unit is a local
//! question, so a controller cannot do it from across the network.

use std::sync::Arc;
use std::time::Duration;

use sentinel::agent::system::FakeInspector;
use sentinel::agent::{Agent, ControllerClient, Spool, SpoolLimits};
use sentinel::config::Config;
use sentinel::entity::{EntityKey, EntityType};
use sentinel::protocol::ClusterCredential;

const TOKEN: &str = "0123456789abcdef0123456789abcdef";

fn config() -> Config {
    Config::from_toml(
        "config_version = 1\nenvironment = \"lab\"\n\n[agent]\ncontroller_address = \"127.0.0.1:1\"\n",
        std::path::Path::new("test.toml"),
    )
    .expect("config")
}

/// A compute node: systemd present, and Slurm both configured to run slurmd
/// here and actually installed. The capability needs both -- a distribution
/// that ships the whole suite does not make every host a compute node
/// (docs/adr/0003).
fn compute_node() -> FakeInspector {
    FakeInspector::bare()
        .with_hostname("node01")
        .with_path("/run/systemd/system")
        .with_program("systemctl")
        .with_program("scontrol")
        .with_program("slurmd")
        .with_file(
            "/etc/slurm/slurm.conf",
            "SlurmctldHost=head01\nNodeName=node01 CPUs=8\n",
        )
}

async fn agent_on(inspector: FakeInspector) -> Agent {
    Agent::new(
        &config(),
        Arc::new(inspector),
        ControllerClient::new("127.0.0.1:1", &ClusterCredential::new(TOKEN), Duration::from_millis(50))
            .expect("client"),
        Spool::open_in_memory(SpoolLimits::default()).await.expect("spool"),
    )
    .expect("agent")
}

fn scheduled_ids(agent: &Agent) -> Vec<String> {
    agent
        .local_probes()
        .probes()
        .map(|p| p.definition().id.to_string())
        .collect()
}

#[tokio::test]
async fn a_compute_node_watches_its_own_slurmd() {
    let agent = agent_on(compute_node()).await;
    assert!(
        scheduled_ids(&agent).contains(&sentinel::probes::systemd::PROBE_ID.to_string()),
        "{:?}",
        scheduled_ids(&agent)
    );
}

#[tokio::test]
async fn the_observation_is_about_the_service_not_the_host() {
    // The whole point. Attributing it to the host would fold the daemon's
    // state into the machine's, and the two are exactly what this system
    // exists to tell apart.
    let mut agent = agent_on(compute_node()).await;
    let observations = agent.local_probes_mut().run_all().await;

    let service = EntityKey::new("lab", EntityType::Service, "slurmd@node01").entity_id();
    let host = EntityKey::new("lab", EntityType::Host, "node01").entity_id();

    let unit = observations
        .iter()
        .find(|o| o.probe_id.as_str() == sentinel::probes::systemd::PROBE_ID)
        .expect("the unit probe ran");

    assert_eq!(unit.target_entity, service);
    assert_ne!(unit.target_entity, host);
}

#[tokio::test]
async fn a_host_without_systemd_schedules_no_unit_probe() {
    // The capability gate asks about this host, because this host has to run
    // `systemctl`. Scheduling it without systemd would report UNSUPPORTED for
    // ever, which is worse than reporting nothing.
    let no_systemd = FakeInspector::bare()
        .with_hostname("node01")
        .with_program("scontrol")
        .with_program("slurmd")
        .with_file(
            "/etc/slurm/slurm.conf",
            "SlurmctldHost=head01\nNodeName=node01 CPUs=8\n",
        );

    let agent = agent_on(no_systemd).await;
    assert!(!scheduled_ids(&agent).contains(&sentinel::probes::systemd::PROBE_ID.to_string()));
}

#[tokio::test]
async fn a_host_that_runs_no_known_service_schedules_none() {
    // Capability, never role: a machine with systemd but no Slurm capability
    // is not running slurmd, and nothing should claim to watch it.
    let plain = FakeInspector::bare()
        .with_hostname("node01")
        .with_path("/run/systemd/system")
        .with_program("systemctl");

    let agent = agent_on(plain).await;
    assert!(!scheduled_ids(&agent).contains(&sentinel::probes::systemd::PROBE_ID.to_string()));
}

#[tokio::test]
async fn the_controller_host_watches_slurmctld() {
    let head = FakeInspector::bare()
        .with_hostname("head01")
        .with_path("/run/systemd/system")
        .with_program("systemctl")
        .with_program("scontrol")
        .with_program("slurmctld")
        .with_file(
            "/etc/slurm/slurm.conf",
            "SlurmctldHost=head01\nNodeName=node01 CPUs=8\n",
        );

    let mut agent = agent_on(head).await;
    let observations = agent.local_probes_mut().run_all().await;

    let service = EntityKey::new("lab", EntityType::Service, "slurmctld@head01").entity_id();
    assert!(
        observations.iter().any(|o| o.target_entity == service),
        "no observation about slurmctld@head01"
    );
}
