//! M4 acceptance: agent, SSH, network and host are observed separately.
//!
//! The claim being tested is the one the whole architecture rests on: four
//! different failures produce four different pictures, rather than all
//! collapsing into "the host is down".
//!
//! Real listeners on loopback stand in for the four services. Nothing here is
//! mocked at the Sentinel layer — these are the real probes doing real
//! connections.

use std::sync::Arc;
use std::time::Duration;

use sentinel::agent::rpc::{health_snapshot, serve as serve_agent, RpcHandle, RpcState, StaticHealth};
use sentinel::capability::CapabilitySet;
use sentinel::config::Config;
use sentinel::controller::{Controller, RemoteObserver};
use sentinel::entity::{EntityKey, EntityType, ManagedEntity};
use sentinel::observation::ProbeStatus;
use sentinel::persistence::SqliteStore;
use sentinel::probes::{network, sentinel_rpc, ssh};
use sentinel::state::{Health, StateComponent};
use tokio::io::AsyncWriteExt;

/// A host with each service either running or not.
struct Fixture {
    entity: ManagedEntity,
    _ssh: Option<tokio::task::JoinHandle<()>>,
    _agent: Option<RpcHandle>,
}

/// Bind a port and immediately release it, so nothing is listening there.
async fn dead_port() -> u16 {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("bind");
    listener.local_addr().expect("addr").port()
}

/// A listener that answers with an SSH banner.
async fn ssh_server() -> (u16, tokio::task::JoinHandle<()>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let port = listener.local_addr().expect("addr").port();
    let handle = tokio::spawn(async move {
        while let Ok((mut stream, _)) = listener.accept().await {
            let _ = stream.write_all(b"SSH-2.0-OpenSSH_9.6p1\r\n").await;
            let _ = stream.shutdown().await;
        }
    });
    (port, handle)
}

/// A running Sentinel agent health endpoint.
async fn agent_endpoint() -> RpcHandle {
    serve_agent(
        "127.0.0.1:0",
        RpcState::new(Arc::new(StaticHealth(health_snapshot(
            "lab",
            "node-a",
            Some("boot-1".into()),
            CapabilitySet::from_iter(["host.metrics", "ssh.server", "sentinel.agent"]),
            std::time::Instant::now(),
            0,
            true,
        )))),
    )
    .await
    .expect("serve")
}

/// Build a host fixture with the requested services running.
async fn host(name: &str, ssh_running: bool, agent_running: bool) -> Fixture {
    let (ssh_port, ssh_handle) = if ssh_running {
        let (port, handle) = ssh_server().await;
        (port, Some(handle))
    } else {
        (dead_port().await, None)
    };

    let (agent_port, agent_handle) = if agent_running {
        let handle = agent_endpoint().await;
        (handle.local_addr.port(), Some(handle))
    } else {
        (dead_port().await, None)
    };

    let mut entity = ManagedEntity::new("lab", EntityType::Host, name).with_capabilities(CapabilitySet::from_iter([
        "network.tcp",
        "ssh.server",
        "sentinel.agent",
        "host.metrics",
    ]));
    entity.metadata = serde_json::json!({
        "host": { "addresses": ["127.0.0.1"] },
        // The reachability probe uses the SSH port as its generic TCP target,
        // which is what a real deployment does too.
        "ports": { "ssh": ssh_port, "agent": agent_port },
    });

    Fixture {
        entity,
        _ssh: ssh_handle,
        _agent: agent_handle,
    }
}

/// Probe a fixture and index the results by probe.
async fn observe(fixture: &Fixture) -> std::collections::BTreeMap<String, ProbeStatus> {
    RemoteObserver::new()
        .observe(&fixture.entity)
        .await
        .into_iter()
        .map(|o| (o.probe_id.as_str().to_string(), o.status))
        .collect()
}

#[tokio::test]
async fn a_fully_healthy_host_reports_healthy_on_every_component() {
    let fixture = host("node-a", true, true).await;
    let statuses = observe(&fixture).await;

    assert_eq!(statuses[ssh::PROBE_ID], ProbeStatus::Ok);
    assert_eq!(statuses[sentinel_rpc::PROBE_ID], ProbeStatus::Ok);
}

#[tokio::test]
async fn an_agent_failure_leaves_ssh_healthy() {
    // The distinction that stops an operator being sent to a working machine
    // because the monitoring on it stopped.
    let fixture = host("node-a", true, false).await;
    let statuses = observe(&fixture).await;

    assert_eq!(
        statuses[sentinel_rpc::PROBE_ID],
        ProbeStatus::Failed,
        "the agent is gone"
    );
    assert_eq!(
        statuses[ssh::PROBE_ID],
        ProbeStatus::Ok,
        "but SSH answers, so the host is fine"
    );
}

#[tokio::test]
async fn an_ssh_failure_leaves_the_agent_healthy() {
    // The mirror image: an operator locked out of a machine that is otherwise
    // working perfectly.
    let fixture = host("node-a", false, true).await;
    let statuses = observe(&fixture).await;

    assert_eq!(statuses[ssh::PROBE_ID], ProbeStatus::Failed, "sshd is gone");
    assert_eq!(
        statuses[sentinel_rpc::PROBE_ID],
        ProbeStatus::Ok,
        "but the agent answers"
    );
}

#[tokio::test]
async fn both_services_down_still_shows_the_host_answering() {
    // Both services refused rather than timing out, which is positive evidence
    // that the host itself is alive. A monitor that read this as
    // HOST_UNREACHABLE would be discarding the evidence in front of it.
    let fixture = host("node-a", false, false).await;
    let statuses = observe(&fixture).await;

    assert_eq!(statuses[ssh::PROBE_ID], ProbeStatus::Failed);
    assert_eq!(statuses[sentinel_rpc::PROBE_ID], ProbeStatus::Failed);

    let network = RemoteObserver::new()
        .observe(&fixture.entity)
        .await
        .into_iter()
        .find(|o| o.probe_id.as_str() == network::PROBE_ID)
        .expect("reachability probe ran");
    assert_eq!(
        network.payload["host_responded"], true,
        "a refusal proves something is there; a dead host sends nothing"
    );
}

#[tokio::test]
async fn the_four_failures_produce_four_different_component_states() {
    // The heart of M4, asserted through the real state engine.
    let store = SqliteStore::open_in_memory().await.expect("store");
    let config = Config {
        config_version: 1,
        environment: "lab".into(),
        ..Config::default()
    };
    let mut controller = Controller::new(config, store).await.expect("controller");

    let fixtures = [
        ("healthy", host("healthy", true, true).await),
        ("agent-down", host("agent-down", true, false).await),
        ("ssh-down", host("ssh-down", false, true).await),
        ("both-down", host("both-down", false, false).await),
    ];

    let mut snapshot = sentinel::inventory::InventorySnapshot::new(sentinel::entity::DiscoverySource::StaticConfig);
    for (_, fixture) in &fixtures {
        snapshot.add_entity(fixture.entity.clone());
    }
    controller.ingest_snapshot(&snapshot).await.expect("inventory");

    // Three rounds, because the network-facing probes debounce: one failure is
    // deliberately not enough to move a component (SPEC.md §90).
    for _ in 0..3 {
        let mut observations = Vec::new();
        for (_, fixture) in &fixtures {
            observations.extend(RemoteObserver::new().observe(&fixture.entity).await);
        }
        controller
            .ingest_observations(&observations)
            .await
            .expect("observations");
    }

    let state_of = |name: &str| {
        let id = EntityKey::new("lab", EntityType::Host, name).entity_id();
        controller
            .engine()
            .state(id)
            .cloned()
            .unwrap_or_else(|| panic!("no state for {name}"))
    };

    let healthy = state_of("healthy");
    assert_eq!(healthy.component(StateComponent::Ssh), Health::Healthy);
    assert_eq!(healthy.component(StateComponent::Agent), Health::Healthy);

    let agent_down = state_of("agent-down");
    assert_eq!(agent_down.component(StateComponent::Agent), Health::Unavailable);
    assert_eq!(agent_down.component(StateComponent::Ssh), Health::Healthy);

    let ssh_down = state_of("ssh-down");
    assert_eq!(ssh_down.component(StateComponent::Ssh), Health::Unavailable);
    assert_eq!(ssh_down.component(StateComponent::Agent), Health::Healthy);

    let both_down = state_of("both-down");
    assert_eq!(both_down.component(StateComponent::Ssh), Health::Unavailable);
    assert_eq!(both_down.component(StateComponent::Agent), Health::Unavailable);

    // Every one of them is a distinguishable picture, which is the claim.
    let pictures: std::collections::BTreeSet<_> = [&healthy, &agent_down, &ssh_down, &both_down]
        .iter()
        .map(|s| (s.component(StateComponent::Ssh), s.component(StateComponent::Agent)))
        .collect();
    assert_eq!(
        pictures.len(),
        4,
        "the four failures must not collapse into fewer states"
    );
}

#[tokio::test]
async fn a_single_failure_does_not_move_a_component() {
    // Debouncing, at the level it actually matters.
    let store = SqliteStore::open_in_memory().await.expect("store");
    let config = Config {
        config_version: 1,
        environment: "lab".into(),
        ..Config::default()
    };
    let mut controller = Controller::new(config, store).await.expect("controller");

    let fixture = host("flaky", false, true).await;
    let mut snapshot = sentinel::inventory::InventorySnapshot::new(sentinel::entity::DiscoverySource::StaticConfig);
    snapshot.add_entity(fixture.entity.clone());
    controller.ingest_snapshot(&snapshot).await.expect("inventory");

    let observations = RemoteObserver::new().observe(&fixture.entity).await;
    controller
        .ingest_observations(&observations)
        .await
        .expect("observations");

    let id = EntityKey::new("lab", EntityType::Host, "flaky").entity_id();
    let state = controller.engine().state(id).expect("state");
    assert_ne!(
        state.component(StateComponent::Ssh),
        Health::Unavailable,
        "one failed connection is not an outage"
    );
}

#[tokio::test]
async fn the_agent_probes_its_own_host_and_the_observations_reach_the_controller() {
    use sentinel::agent::system::FakeInspector;
    use sentinel::agent::{Agent, ControllerClient, Spool, SpoolLimits};
    use sentinel::controller::{serve, ServeOptions};
    use sentinel::protocol::ClusterCredential;

    const TOKEN: &str = "0123456789abcdef0123456789abcdef";

    let store = SqliteStore::open_in_memory().await.expect("store");
    let config = Config {
        config_version: 1,
        environment: "lab".into(),
        ..Config::default()
    };
    let controller = Controller::new(config.clone(), store.clone())
        .await
        .expect("controller");

    let handle = serve(
        controller,
        ServeOptions {
            listen: "127.0.0.1:0".into(),
            credential: ClusterCredential::new(TOKEN),
            heartbeat_interval: Duration::from_secs(5),
            discovery_interval: None,
            diagnosis_interval: None,
            retention: None,
            tls: None,
        },
    )
    .await
    .expect("serve");

    let address = handle.local_addr.to_string();
    let mut agent_config = config.clone();
    agent_config.agent.controller_address = Some(address.clone());

    let mut agent = Agent::new(
        &agent_config,
        Arc::new(FakeInspector::bare().with_hostname("node-a")),
        ControllerClient::new(&address, &ClusterCredential::new(TOKEN), Duration::from_secs(2)).expect("client"),
        Spool::open_in_memory(SpoolLimits::default()).await.expect("spool"),
    )
    .expect("agent");

    agent.register().await.expect("register");

    // An agent that is running can always read its own /proc, so host.metrics
    // is scheduled without anything needing to detect it.
    assert_eq!(agent.local_probes().len(), 1, "host.metrics is scheduled");
    // `probe_now` rather than `probe_once`: probes are jittered, so nothing is
    // due in the first fraction of an interval after startup.
    assert!(
        agent.probe_now().await.expect("probe") > 0,
        "the probe produced an observation"
    );

    let entity = EntityKey::new("lab", EntityType::Host, "node-a").entity_id();
    assert!(
        store.observation_count(entity).await.expect("count") > 0,
        "the observation reached the controller through the normal path"
    );

    // And the gate really is a gate: an operator turning the capability off
    // stops the probe being scheduled at all (SPEC.md §19).
    let mut disabled_config = agent_config.clone();
    disabled_config
        .capabilities
        .insert("host.metrics".into(), sentinel::capability::CapabilityOverride::Disable);

    let disabled = Agent::new(
        &disabled_config,
        Arc::new(FakeInspector::bare().with_hostname("node-b")),
        ControllerClient::new(&address, &ClusterCredential::new(TOKEN), Duration::from_secs(2)).expect("client"),
        Spool::open_in_memory(SpoolLimits::default()).await.expect("spool"),
    )
    .expect("agent");

    assert!(
        disabled.local_probes().is_empty(),
        "disabling the capability must stop the probe, not just hide its results"
    );

    handle.shutdown().await;
}

#[tokio::test]
async fn a_host_with_a_non_standard_ssh_port_is_probed_on_the_right_port() {
    // A cluster that moved SSH off 22 must not have every host reported as
    // SSH-down. This exercises the whole path: configured port -> entity
    // metadata -> endpoint resolution -> the probe's actual connection.
    use sentinel::controller::endpoint_for;
    use sentinel::entity::DiscoverySource;

    let (ssh_port, _ssh) = ssh_server().await;

    // Declared in configuration, as a host with no agent would be.
    let config = sentinel::config::Config::from_toml(
        &format!(
            r#"
config_version = 1
environment = "lab"

[[entities]]
type = "host"
name = "node-a"
capabilities = ["network.tcp", "ssh.server"]
addresses = ["127.0.0.1"]
ports = {{ ssh = {ssh_port} }}
"#
        ),
        std::path::Path::new("test.toml"),
    )
    .expect("parse config");

    let snapshot = sentinel::inventory::static_config::StaticConfigProvider::new(config).snapshot();
    let entity = &snapshot.entities[0];
    assert_eq!(entity.discovery_sources, vec![DiscoverySource::StaticConfig]);

    // The endpoint carries the configured port through to the probe.
    let endpoint = endpoint_for(entity).expect("endpoint");
    assert_eq!(endpoint.parameters()["ssh_port"], ssh_port);

    // And the probe finds SSH there.
    let observations = RemoteObserver::new().observe(entity).await;
    let ssh = observations
        .iter()
        .find(|o| o.probe_id.as_str() == ssh::PROBE_ID)
        .expect("the SSH probe ran");

    assert_eq!(ssh.status, ProbeStatus::Ok, "SSH must be found on its real port");
    assert_eq!(ssh.payload["port"], ssh_port);
}

#[tokio::test]
async fn without_the_configured_port_the_probe_goes_to_the_default() {
    // The counter-example that shows the previous test is testing something:
    // with no port declared, the probe asks the default port, which is not
    // where this host's SSH actually is.
    //
    // What is asserted is *which port the probe chose*, not whether anything
    // happens to answer on 22 on the machine running the tests. A CI runner
    // has its own sshd; a test that fails there would be testing the runner.
    let (ssh_port, _ssh) = ssh_server().await;

    let mut entity = ManagedEntity::new("lab", EntityType::Host, "node-a")
        .with_capabilities(CapabilitySet::from_iter(["network.tcp", "ssh.server"]));
    entity.metadata = serde_json::json!({ "host": { "addresses": ["127.0.0.1"] } });

    let observations = RemoteObserver::new().observe(&entity).await;
    let ssh = observations
        .iter()
        .find(|o| o.probe_id.as_str() == ssh::PROBE_ID)
        .expect("the SSH probe ran");

    assert_eq!(ssh.payload["port"], sentinel::probes::ssh::DEFAULT_PORT);
    assert_ne!(
        ssh.payload["port"], ssh_port,
        "the probe must not have found the real port by accident"
    );
}

#[tokio::test]
async fn a_host_whose_ssh_port_answers_nothing_is_not_reported_healthy() {
    // The consequence the previous test implies, on a port this test owns, so
    // that it holds on any machine: probe where nothing listens and the SSH
    // component is not Ok.
    let closed = dead_port().await;

    let mut entity = ManagedEntity::new("lab", EntityType::Host, "node-a")
        .with_capabilities(CapabilitySet::from_iter(["network.tcp", "ssh.server"]));
    entity.metadata = serde_json::json!({
        "host": { "addresses": ["127.0.0.1"] },
        "ports": { "ssh": closed },
    });

    let observations = RemoteObserver::new().observe(&entity).await;
    let ssh = observations
        .iter()
        .find(|o| o.probe_id.as_str() == ssh::PROBE_ID)
        .expect("the SSH probe ran");

    assert_eq!(ssh.payload["port"], closed);
    assert_ne!(ssh.status, ProbeStatus::Ok);
}
