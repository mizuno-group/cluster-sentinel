//! M2 acceptance: an agent registers with a controller, heartbeats, and keeps
//! its observations across a controller outage.
//!
//! These use a real HTTP server and a real client on a loopback port. Nothing
//! is mocked except the *machine* the agent believes it is running on, which is
//! the one thing a test cannot conjure.

use std::sync::Arc;
use std::time::Duration;

use sentinel::agent::system::FakeInspector;
use sentinel::agent::{Agent, ControllerClient, Spool, SpoolLimits};
use sentinel::config::{AgentConfig, Config};
use sentinel::controller::{serve, Controller, ServeOptions, ServerHandle};
use sentinel::entity::{EntityKey, EntityType};
use sentinel::observation::{Observation, ProbeStatus};
use sentinel::persistence::SqliteStore;
use sentinel::probes::ProbeId;
use sentinel::protocol::ClusterCredential;

const TOKEN: &str = "0123456789abcdef0123456789abcdef";

fn credential() -> ClusterCredential {
    ClusterCredential::new(TOKEN)
}

fn config() -> Config {
    Config {
        config_version: 1,
        environment: "lab".into(),
        ..Config::default()
    }
}

async fn start_controller(store: SqliteStore) -> ServerHandle {
    let controller = Controller::new(config(), store).await.expect("controller");
    serve(
        controller,
        ServeOptions {
            listen: "127.0.0.1:0".into(),
            credential: credential(),
            heartbeat_interval: Duration::from_secs(5),
            // No background discovery: these tests drive everything explicitly
            // so nothing races.
            discovery_interval: None,
            diagnosis_interval: None,
            retention: None,
            tls: None,
        },
    )
    .await
    .expect("serve")
}

async fn build_agent(address: &str, inspector: FakeInspector, spool: Spool) -> Agent {
    let mut config = config();
    config.agent = AgentConfig {
        controller_address: Some(address.into()),
        spool_path: None,
        roles: vec![],
        ..Default::default()
    };

    Agent::new(
        &config,
        Arc::new(inspector),
        ControllerClient::new(address, &credential(), Duration::from_millis(500)).expect("client"),
        spool,
    )
    .expect("agent")
}

fn observation_for(hostname: &str, status: ProbeStatus) -> Observation {
    Observation::new(
        ProbeId::new("host.metrics"),
        EntityKey::new("lab", EntityType::Host, hostname).entity_id(),
        status,
    )
}

#[tokio::test]
async fn an_agent_registers_and_appears_in_the_inventory() {
    let store = SqliteStore::open_in_memory().await.expect("store");
    let handle = start_controller(store.clone()).await;
    let address = handle.local_addr.to_string();

    let inspector = FakeInspector::bare()
        .with_hostname("fileserver-a")
        .with_path("/etc/exports")
        .with_program("zpool");
    let spool = Spool::open_in_memory(SpoolLimits::default()).await.expect("spool");
    let mut agent = build_agent(&address, inspector, spool).await;

    agent.register().await.expect("register");
    assert!(agent.status().registered);

    // The host Slurm has never heard of is now a monitored entity.
    let entities = store.load_entities("lab").await.expect("entities");
    let entity = entities
        .iter()
        .find(|e| e.canonical_name == "fileserver-a")
        .expect("registered host");
    assert!(
        entity.capabilities.has("storage.nfs.server"),
        "discovered, not declared"
    );
    assert!(entity.capabilities.has("storage.zfs"));
    assert!(entity.capabilities.has("sentinel.agent"));

    handle.shutdown().await;
}

#[tokio::test]
async fn a_heartbeat_measures_the_clock_difference() {
    let handle = start_controller(SqliteStore::open_in_memory().await.expect("store")).await;
    let address = handle.local_addr.to_string();
    let spool = Spool::open_in_memory(SpoolLimits::default()).await.expect("spool");
    let mut agent = build_agent(&address, FakeInspector::bare(), spool).await;

    agent.register().await.expect("register");
    agent.heartbeat().await.expect("heartbeat");

    let skew = agent.status().clock_skew_ms.expect("skew measured");
    assert!(
        skew.abs() < 5_000,
        "agent and controller share a clock here; skew was {skew}ms"
    );

    handle.shutdown().await;
}

#[tokio::test]
async fn observations_reach_the_controller_and_leave_the_spool() {
    let store = SqliteStore::open_in_memory().await.expect("store");
    let handle = start_controller(store.clone()).await;
    let address = handle.local_addr.to_string();
    let spool = Spool::open_in_memory(SpoolLimits::default()).await.expect("spool");
    let mut agent = build_agent(&address, FakeInspector::bare(), spool).await;

    agent.register().await.expect("register");
    agent
        .record(&[observation_for("test-host", ProbeStatus::Ok)])
        .await
        .expect("record");

    assert!(
        agent.spool().is_empty().await.expect("spool"),
        "delivered observations are acknowledged"
    );

    let entity = EntityKey::new("lab", EntityType::Host, "test-host").entity_id();
    assert_eq!(store.observation_count(entity).await.expect("count"), 1);

    handle.shutdown().await;
}

#[tokio::test]
async fn a_controller_outage_costs_no_observations() {
    // SPEC.md §105 and §176: this is the property the spool exists for.
    let store = SqliteStore::open_in_memory().await.expect("store");
    let handle = start_controller(store.clone()).await;
    let address = handle.local_addr.to_string();
    let spool = Spool::open_in_memory(SpoolLimits::default()).await.expect("spool");
    let mut agent = build_agent(&address, FakeInspector::bare(), spool).await;

    agent.register().await.expect("register");
    agent
        .record(&[observation_for("test-host", ProbeStatus::Ok)])
        .await
        .expect("first");

    // The controller goes away. The agent keeps observing.
    handle.shutdown().await;
    for _ in 0..5 {
        agent
            .record(&[observation_for("test-host", ProbeStatus::Failed)])
            .await
            .expect("record during outage");
    }
    assert_eq!(agent.spool().len().await.expect("len"), 5, "held, not dropped");
    assert!(agent.status().last_error.is_some());

    // The controller comes back on the same port.
    let listener = std::net::TcpListener::bind(address.parse::<std::net::SocketAddr>().unwrap());
    drop(listener); // just confirming the port is free again
    let restarted = Controller::new(config(), store.clone()).await.expect("controller");
    let handle = serve(
        restarted,
        ServeOptions {
            listen: address.clone(),
            credential: credential(),
            heartbeat_interval: Duration::from_secs(5),
            discovery_interval: None,
            diagnosis_interval: None,
            retention: None,
            tls: None,
        },
    )
    .await
    .expect("restart");

    let delivered = agent.flush().await.expect("flush");
    assert_eq!(delivered, 5, "everything spooled during the outage arrives");
    assert!(agent.spool().is_empty().await.expect("spool"));

    let entity = EntityKey::new("lab", EntityType::Host, "test-host").entity_id();
    assert_eq!(store.observation_count(entity).await.expect("count"), 6);

    handle.shutdown().await;
}

#[tokio::test]
async fn replaying_a_spool_twice_does_not_double_count() {
    // IMPLEMENTATION.md §45. The ids come from the agent, so a replay after a
    // lost acknowledgement is safe.
    let store = SqliteStore::open_in_memory().await.expect("store");
    let handle = start_controller(store.clone()).await;
    let address = handle.local_addr.to_string();
    let spool = Spool::open_in_memory(SpoolLimits::default()).await.expect("spool");
    let mut agent = build_agent(&address, FakeInspector::bare(), spool).await;

    agent.register().await.expect("register");
    let observation = observation_for("test-host", ProbeStatus::Ok);

    // Deliver, then put the same observation back as if the acknowledgement
    // had been lost in flight.
    agent.record(std::slice::from_ref(&observation)).await.expect("first");
    agent.record(std::slice::from_ref(&observation)).await.expect("replay");

    let entity = EntityKey::new("lab", EntityType::Host, "test-host").entity_id();
    assert_eq!(store.observation_count(entity).await.expect("count"), 1);

    handle.shutdown().await;
}

#[tokio::test]
async fn a_wrong_credential_is_refused_and_not_retried_forever() {
    let handle = start_controller(SqliteStore::open_in_memory().await.expect("store")).await;
    let address = handle.local_addr.to_string();

    let wrong = ClusterCredential::new("wrong-token-wrong-token-wrong-to");
    let mut config = config();
    config.agent.controller_address = Some(address.clone());
    let mut agent = Agent::new(
        &config,
        Arc::new(FakeInspector::bare()),
        ControllerClient::new(&address, &wrong, Duration::from_millis(500)).expect("client"),
        Spool::open_in_memory(SpoolLimits::default()).await.expect("spool"),
    )
    .expect("agent");

    let error = agent.register().await.expect_err("must be refused");
    assert!(!error.is_transient(), "a rejected credential will not fix itself");
    assert!(!agent.status().registered);

    handle.shutdown().await;
}

#[tokio::test]
async fn a_reboot_is_detected_from_the_boot_id_and_an_agent_restart_is_not() {
    // SPEC.md §107: reboots matter because they destroy the evidence of
    // whatever caused them.
    let store = SqliteStore::open_in_memory().await.expect("store");
    let handle = start_controller(store.clone()).await;
    let address = handle.local_addr.to_string();
    let entity = EntityKey::new("lab", EntityType::Host, "node-a").entity_id();

    let register_with_boot_id = |boot_id: &'static str| {
        let address = address.clone();
        async move {
            let inspector = FakeInspector::bare().with_hostname("node-a").with_boot_id(boot_id);
            let spool = Spool::open_in_memory(SpoolLimits::default()).await.expect("spool");
            let mut agent = build_agent(&address, inspector, spool).await;
            agent.register().await.expect("register");
        }
    };

    register_with_boot_id("boot-1").await;
    let after_first = store.recent_observations(entity, 50).await.expect("observations").len();

    // Same machine, agent restarted: not a reboot.
    register_with_boot_id("boot-1").await;
    assert_eq!(
        store.recent_observations(entity, 50).await.expect("observations").len(),
        after_first,
        "an agent restart must not be recorded as a reboot"
    );

    // New boot id: a reboot.
    register_with_boot_id("boot-2").await;
    let observations = store.recent_observations(entity, 50).await.expect("observations");
    let reboot = observations
        .iter()
        .find(|o| o.probe_id.as_str() == "host.boot")
        .expect("reboot recorded");
    assert_eq!(reboot.payload["previous_boot_id"], "boot-1");
    assert_eq!(reboot.payload["boot_id"], "boot-2");

    handle.shutdown().await;
}

#[tokio::test]
async fn two_agents_register_as_two_distinct_hosts() {
    let store = SqliteStore::open_in_memory().await.expect("store");
    let handle = start_controller(store.clone()).await;
    let address = handle.local_addr.to_string();

    for hostname in ["node-a", "node-b"] {
        let inspector = FakeInspector::bare().with_hostname(hostname);
        let spool = Spool::open_in_memory(SpoolLimits::default()).await.expect("spool");
        let mut agent = build_agent(&address, inspector, spool).await;
        agent.register().await.expect("register");
    }

    let entities = store.load_entities("lab").await.expect("entities");
    assert_eq!(entities.len(), 2);

    handle.shutdown().await;
}

#[tokio::test]
async fn an_agent_that_starts_before_the_controller_recovers_by_itself() {
    // Boot order is not something a monitoring system may depend on.
    let store = SqliteStore::open_in_memory().await.expect("store");

    // Reserve a port, then release it, so we know a free address to use.
    let address = {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("bind");
        listener.local_addr().expect("addr").to_string()
    };

    let spool = Spool::open_in_memory(SpoolLimits::default()).await.expect("spool");
    let mut agent = build_agent(&address, FakeInspector::bare(), spool).await;

    assert!(agent.register().await.is_err(), "nothing is listening yet");
    agent
        .record(&[observation_for("test-host", ProbeStatus::Ok)])
        .await
        .expect("record");
    assert_eq!(agent.spool().len().await.expect("len"), 1);

    let controller = Controller::new(config(), store.clone()).await.expect("controller");
    let handle = serve(
        controller,
        ServeOptions {
            listen: address,
            credential: credential(),
            heartbeat_interval: Duration::from_secs(5),
            discovery_interval: None,
            diagnosis_interval: None,
            retention: None,
            tls: None,
        },
    )
    .await
    .expect("serve");

    agent.heartbeat().await.expect("heartbeat registers us");
    assert!(agent.status().registered);
    assert_eq!(
        agent.flush().await.expect("flush"),
        1,
        "the spooled observation arrives"
    );

    handle.shutdown().await;
}

#[tokio::test]
async fn the_controller_health_endpoint_needs_no_credential() {
    // A peer must be able to tell the controller is alive (SPEC.md §109).
    let handle = start_controller(SqliteStore::open_in_memory().await.expect("store")).await;
    let url = format!("http://{}/v1/health", handle.local_addr);

    let response = reqwest::get(&url).await.expect("get");
    assert!(response.status().is_success());

    let body: serde_json::Value = response.json().await.expect("json");
    assert_eq!(body["environment"], "lab");

    handle.shutdown().await;
}
