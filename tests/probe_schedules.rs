//! Monitoring cadences come from the configuration file.
//!
//! The compiled-in schedules suit a few hundred nodes on a healthy network.
//! They are not right everywhere, and an operator who cannot change them has
//! to choose between running Sentinel at the wrong cadence and not running it.
//!
//! What these check is that an override actually reaches the probe, not merely
//! the config struct. Several probes bound their own work by their timeout, so
//! a value the runner honoured and the probe did not would be two different
//! timeouts wearing one name.

use std::sync::Arc;
use std::time::Duration;

use sentinel::agent::system::FakeInspector;
use sentinel::agent::{Agent, ControllerClient, PeerProbes, Spool, SpoolLimits};
use sentinel::config::Config;
use sentinel::controller::RemoteObserver;
use sentinel::entity::{EntityKey, EntityType};
use sentinel::probes::Probe;
use sentinel::protocol::ClusterCredential;

const TOKEN: &str = "0123456789abcdef0123456789abcdef";

fn config(probes: &str) -> Config {
    let text = format!(
        "config_version = 1\nenvironment = \"lab\"\n\n[agent]\ncontroller_address = \"127.0.0.1:1\"\n\n[probes]\n{probes}"
    );
    Config::from_toml(&text, std::path::Path::new("test.toml")).expect("config")
}

/// A host with an NFS mount, a GPU and a journal, so every local probe applies.
fn inspector() -> FakeInspector {
    FakeInspector::bare()
        .with_mount("fs1:/home", "/home", "nfs4")
        .with_program("nvidia-smi")
        .with_program("journalctl")
        .with_program("systemctl")
        .with_path("/run/systemd/system")
}

async fn agent(config: &Config) -> Agent {
    Agent::new(
        config,
        Arc::new(inspector()),
        ControllerClient::new("127.0.0.1:1", &ClusterCredential::new(TOKEN), Duration::from_millis(50))
            .expect("client"),
        Spool::open_in_memory(SpoolLimits::default()).await.expect("spool"),
    )
    .expect("agent")
}

fn scheduled<'a>(
    probes: impl Iterator<Item = &'a Arc<dyn Probe>>,
    id: &str,
) -> Option<sentinel::probes::ProbeDefinition> {
    probes
        .filter(|p| p.definition().id.as_str() == id)
        .map(|p| p.definition().clone())
        .next()
}

#[tokio::test]
async fn an_interval_from_the_configuration_reaches_the_probe() {
    let config = config("\"nfs.client.mount\" = { interval = \"5m\" }");
    let agent = agent(&config).await;

    let definition = scheduled(agent.local_probes().probes(), "nfs.client.mount").expect("mount probe");
    assert_eq!(definition.interval, Duration::from_secs(300));
}

#[tokio::test]
async fn a_timeout_reaches_the_probe_not_only_the_runner() {
    // The journal probe passes its own timeout to journalctl. A timeout the
    // runner enforced but the probe did not know about would mean the command
    // still used the compiled default.
    let config = config("\"journal.events\" = { timeout = \"45s\" }");
    let agent = agent(&config).await;

    let definition = scheduled(agent.local_probes().probes(), "journal.events").expect("journal probe");
    assert_eq!(definition.timeout, Duration::from_secs(45));
}

#[tokio::test]
async fn changing_one_probe_leaves_the_others_at_their_defaults() {
    let config = config("\"gpu.nvidia\" = { interval = \"120s\" }");
    let agent = agent(&config).await;

    let gpu = scheduled(agent.local_probes().probes(), "gpu.nvidia").expect("gpu probe");
    assert_eq!(gpu.interval, Duration::from_secs(120));

    let journal = scheduled(agent.local_probes().probes(), "journal.events").expect("journal probe");
    assert_eq!(journal.interval, Duration::from_secs(30));
}

#[tokio::test]
async fn a_probe_switched_off_is_never_scheduled() {
    let config = config("\"gpu.nvidia\" = { enabled = false }");
    let agent = agent(&config).await;

    assert!(scheduled(agent.local_probes().probes(), "gpu.nvidia").is_none());
    // And the others still are.
    assert!(scheduled(agent.local_probes().probes(), "journal.events").is_some());
}

#[tokio::test]
async fn concurrency_cannot_be_raised_above_what_the_probe_pins() {
    // The NFS I/O probe pins one outstanding execution because a blocked
    // syscall must never be joined by a second one. A configuration file is
    // not the place to overturn that (SPEC.md §76).
    let config = config("\"nfs.client.io\" = { max_outstanding = 32 }");
    let agent = agent(&config).await;

    let definition = scheduled(agent.local_probes().probes(), "nfs.client.io").expect("io probe");
    assert_eq!(definition.max_outstanding, 1);
}

#[test]
fn the_controllers_remote_probes_take_the_same_overrides() {
    let config = config("\"network.tcp\" = { interval = \"1m\" }");
    let observer = RemoteObserver::with_schedules(&config.probes);

    let definition = scheduled(observer.probes().iter(), "network.tcp").expect("tcp probe");
    assert_eq!(definition.interval, Duration::from_secs(60));
}

#[test]
fn a_probe_switched_off_is_not_run_by_the_controller_either() {
    let config = config("\"ssh.service\" = { enabled = false }");
    let observer = RemoteObserver::with_schedules(&config.probes);

    assert!(scheduled(observer.probes().iter(), "ssh.service").is_none());
    assert!(scheduled(observer.probes().iter(), "network.tcp").is_some());
}

#[test]
fn peers_use_the_same_schedules_as_everyone_else() {
    // A site that slowed the reachability probe down did not mean "except when
    // a peer runs it": the quorum would then be comparing observations taken
    // at different rates.
    let config = config("\"network.tcp\" = { interval = \"1m\" }");
    let observer = EntityKey::new("lab", EntityType::Host, "n1").entity_id();
    let peers = PeerProbes::with_schedules(observer, &config.probes);

    let definition = scheduled(peers.probes().iter(), "network.tcp").expect("tcp probe");
    assert_eq!(definition.interval, Duration::from_secs(60));
}

#[test]
fn a_misspelled_probe_id_is_an_error_not_a_silent_no_op() {
    // The worst outcome for a typo is nothing happening: the operator believes
    // they retuned a probe, and the cadence they were trying to fix stays.
    let config = config("\"network.tpc\" = { interval = \"1m\" }");
    let report = sentinel::config::validate(&config);

    let errors: Vec<String> = report.errors().map(|e| e.to_string()).collect();
    assert!(!errors.is_empty(), "an unknown probe id was accepted");
    assert!(
        errors.iter().any(|e| e.contains("network.tcp")),
        "the error should list the real probe ids: {errors:?}"
    );
}

#[test]
fn nothing_configured_leaves_every_compiled_schedule_alone() {
    let config = Config::default();
    for entry in sentinel::probes::catalog::catalog() {
        let mut definition = entry.definition.clone();
        config.probes.apply(&mut definition);
        assert_eq!(definition, entry.definition, "{} was changed", entry.id());
    }
}
