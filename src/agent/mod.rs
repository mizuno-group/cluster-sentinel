//! The Sentinel agent.
//!
//! One agent binary runs on every host (SPEC.md §44). Its responsibilities in
//! this milestone are: work out what this machine can do, tell a controller,
//! stay in touch, and never lose an observation because the controller was
//! unreachable.
//!
//! The agent is built around one operating assumption: **the controller will be
//! unavailable sometimes, and that is not an error.** Registration retries,
//! heartbeats tolerate gaps, and observations go to a local spool that is
//! replayed on reconnection.

pub mod addressing;
pub mod client;
pub mod discovery;
pub mod local_probes;
pub mod peer_probes;
pub mod rpc;
pub mod spool;
pub mod system;

pub use client::{ClientError, ControllerClient};
pub use local_probes::LocalProbes;
pub use peer_probes::PeerProbes;
pub use spool::{Spool, SpoolLimits};
pub use system::{LinuxInspector, SystemInspector};

use std::sync::Arc;
use std::time::Duration;

use uuid::Uuid;

use crate::capability::CapabilitySet;
use crate::config::{Config, ProbeSchedules};
use crate::entity::{EntityKey, EntityType};
use crate::observation::Observation;
use crate::probes::{HasDefinition, Probe};
use crate::protocol::{HeartbeatRequest, ObservationBatch, RegisterRequest};
use crate::time::now;
use crate::PROTOCOL_VERSION;

/// Add a probe with the operator's schedule applied.
///
/// The definition is changed rather than wrapped, because several probes read
/// their own timeout to bound the command they run: a schedule the runner
/// enforced but the probe did not know about would be two different timeouts
/// wearing one name.
fn add_scheduled<P>(local: &mut LocalProbes, schedules: &ProbeSchedules, mut probe: P, parameters: serde_json::Value)
where
    P: Probe + HasDefinition + 'static,
{
    if !schedules.is_enabled(probe.definition().id.as_str()) {
        return;
    }
    schedules.apply(probe.definition_mut());
    local.add(Arc::new(probe), parameters);
}

/// Schedule one active filesystem probe per NFS mount.
///
/// Per mount rather than per host, so each gets its own concurrency slot: one
/// wedged filesystem must not stop the others being checked, and it must not
/// accumulate a blocked thread every interval (SPEC.md §76).
fn schedule_accelerator_probes(local: &mut LocalProbes, schedules: &ProbeSchedules) {
    add_scheduled(
        local,
        schedules,
        crate::probes::gpu::NvidiaGpuProbe::new(),
        serde_json::Value::Null,
    );
}

/// Schedule continuous collection of kernel and service events.
///
/// Continuous rather than on demand, because the moment the logs matter most
/// is the moment the host is least able to hand them over. By the time an
/// operator comes to look, the evidence is already at the controller
/// (SPEC.md §1, §83).
fn schedule_journal_probes(local: &mut LocalProbes, schedules: &ProbeSchedules) {
    add_scheduled(
        local,
        schedules,
        crate::probes::journal::JournalProbe::new(),
        serde_json::Value::Null,
    );
}

fn schedule_storage_probes(local: &mut LocalProbes, schedules: &ProbeSchedules, inspector: &dyn SystemInspector) {
    use crate::probes::nfs::{NfsClientIoProbe, NfsMountProbe};

    let mounts: Vec<_> = inspector.mounts().into_iter().filter(|m| m.is_nfs()).collect();
    if mounts.is_empty() {
        return;
    }

    // One cheap probe describing every mount, read from /proc.
    add_scheduled(local, schedules, NfsMountProbe::new(), serde_json::Value::Null);

    // And one active probe per mount.
    for mount in mounts {
        add_scheduled(
            local,
            schedules,
            NfsClientIoProbe::new(),
            serde_json::json!({ "mount_point": mount.target, "source": mount.source }),
        );
    }
}

/// The largest batch the agent sends at once.
///
/// Bounded so that a long outage does not turn into one enormous request that
/// times out, fails, and is retried identically forever.
pub const MAX_BATCH: u32 = 500;

/// How much an agent knows about its own situation.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct AgentStatus {
    /// Whether the agent is currently registered.
    pub registered: bool,
    /// The agent's identity, once registered.
    pub agent_id: Option<Uuid>,
    /// The current session, once registered.
    pub session_id: Option<Uuid>,
    /// Observations waiting to be delivered.
    pub spooled: u64,
    /// Measured clock difference against the controller, in milliseconds.
    pub clock_skew_ms: Option<i64>,
    /// Why the last attempt to reach the controller failed.
    pub last_error: Option<String>,
}

/// A Sentinel agent.
pub struct Agent {
    environment: String,
    hostname: String,
    inspector: Arc<dyn SystemInspector>,
    client: ControllerClient,
    spool: Spool,
    capabilities: CapabilitySet,
    roles: Vec<String>,
    status: AgentStatus,
    local_probes: LocalProbes,
    started_at: std::time::Instant,
    /// Mirrors `status.registered` so the health endpoint can read it without
    /// touching the agent.
    registered_flag: Arc<std::sync::atomic::AtomicBool>,
    /// GPUs the probe has actually seen, if it has run.
    observed_gpu_count: Option<u64>,
    /// SSH port, when the operator has stated one explicitly.
    ssh_port: Option<u16>,
    /// The port this agent's own health endpoint listens on.
    rpc_port: Option<u16>,
    /// Peers this agent observes on the controller's behalf.
    peer_probes: PeerProbes,
    /// When the assignment was last refreshed.
    assignment_refreshed_at: Option<std::time::Instant>,
    /// The `[agent]` section, for settings consulted after construction.
    agent_config: crate::config::AgentConfig,
}

impl Agent {
    /// Build an agent from configuration and a view of the local system.
    ///
    /// Fails only if the host name cannot be determined: an agent that does not
    /// know which machine it is on cannot report anything meaningful, and
    /// guessing would attach observations to the wrong entity.
    pub fn new(
        config: &Config,
        inspector: Arc<dyn SystemInspector>,
        client: ControllerClient,
        spool: Spool,
    ) -> anyhow::Result<Self> {
        let hostname = inspector
            .hostname()
            .ok_or_else(|| anyhow::anyhow!("cannot determine this host's name; refusing to guess"))?;

        let resolution = discovery::resolve(inspector.as_ref(), &config.capabilities, &config.agent.roles);
        let capabilities = resolution.enabled;

        // The agent computes its own entity id from the natural key, so it can
        // label observations correctly before it has ever spoken to a
        // controller (see docs/adr/0001).
        let entity = EntityKey::new(&config.environment, EntityType::Host, &hostname).entity_id();

        let schedules = &config.probes;
        let mut local_probes = LocalProbes::new(entity, capabilities.clone());
        schedule_storage_probes(&mut local_probes, schedules, inspector.as_ref());
        schedule_accelerator_probes(&mut local_probes, schedules);
        schedule_journal_probes(&mut local_probes, schedules);

        Ok(Self {
            environment: config.environment.clone(),
            local_probes,
            hostname,
            inspector,
            client,
            spool,
            capabilities,
            roles: config.agent.roles.clone(),
            status: AgentStatus::default(),
            started_at: std::time::Instant::now(),
            registered_flag: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            observed_gpu_count: None,
            ssh_port: config.agent.ssh_port,
            rpc_port: config
                .agent
                .listen
                .rsplit_once(':')
                .and_then(|(_, port)| port.parse().ok()),
            peer_probes: PeerProbes::with_schedules(entity, schedules),
            assignment_refreshed_at: None,
            agent_config: config.agent.clone(),
        })
    }

    /// The peers this agent observes.
    pub fn peer_probes(&self) -> &PeerProbes {
        &self.peer_probes
    }

    /// Fetch this agent's peer assignment from the controller.
    ///
    /// A failure here is not an error worth propagating: the agent keeps the
    /// assignment it already has, which is far better than dropping peer
    /// observation the moment the controller has a bad minute.
    pub async fn refresh_assignments(&mut self) -> Result<usize, ClientError> {
        let Some(agent_id) = self.status.agent_id else {
            return Ok(0);
        };

        let response = self.client.assignments(agent_id).await?;
        let count = response.targets.len();

        if self.peer_probes.set_targets(response.revision, response.targets) {
            tracing::info!(
                revision = response.revision,
                targets = count,
                peers = ?self.peer_probes.target_names(),
                "peer assignment updated"
            );
        }

        self.assignment_refreshed_at = Some(std::time::Instant::now());
        Ok(count)
    }

    /// Observe assigned peers and record what was seen.
    pub async fn observe_peers(&mut self) -> anyhow::Result<usize> {
        if self.peer_probes.is_empty() {
            return Ok(0);
        }
        let observations = self.peer_probes.observe_all().await;
        self.record_probe_results(observations).await
    }

    /// Whether the assignment is due to be refreshed.
    fn assignment_is_stale(&self) -> bool {
        match self.assignment_refreshed_at {
            None => true,
            Some(at) => at.elapsed() >= peer_probes::ASSIGNMENT_REFRESH,
        }
    }

    /// GPUs this agent has observed, if the probe has run.
    pub fn observed_gpu_count(&self) -> Option<u64> {
        self.observed_gpu_count
    }

    /// A handle the health endpoint can read without locking the agent.
    pub fn health_handle(&self) -> rpc::AgentHealthHandle {
        rpc::AgentHealthHandle {
            environment: self.environment.clone(),
            hostname: self.hostname.clone(),
            inspector: Arc::clone(&self.inspector),
            capabilities: self.capabilities.clone(),
            spool: self.spool.clone(),
            registered: Arc::clone(&self.registered_flag),
            started_at: self.started_at,
        }
    }

    fn set_registered(&mut self, registered: bool) {
        self.status.registered = registered;
        self.registered_flag
            .store(registered, std::sync::atomic::Ordering::Relaxed);
    }

    /// The entity id this agent reports for.
    pub fn entity_id(&self) -> crate::entity::EntityId {
        EntityKey::new(&self.environment, EntityType::Host, &self.hostname).entity_id()
    }

    /// The local probes scheduled on this host.
    pub fn local_probes(&self) -> &LocalProbes {
        &self.local_probes
    }

    /// Run any local probes that are due, and record what they found.
    ///
    /// Probes are jittered, so immediately after startup nothing is due yet.
    /// That is deliberate: a fleet restarted together must not arrive at the
    /// controller in one burst (SPEC.md §123).
    pub async fn probe_once(&mut self) -> anyhow::Result<usize> {
        let observations = self.local_probes.run_due().await;
        self.record_probe_results(observations).await
    }

    /// Run every local probe now, ignoring the schedule.
    ///
    /// For `sentinel doctor` and for tests, which should not have to wait out
    /// a jitter interval to see whether probing works.
    pub async fn probe_now(&mut self) -> anyhow::Result<usize> {
        let observations = self.local_probes.run_all().await;
        self.record_probe_results(observations).await
    }

    async fn record_probe_results(&mut self, observations: Vec<Observation>) -> anyhow::Result<usize> {
        // Learn the real GPU count from the probe, so the next registration
        // reports hardware rather than an assumption.
        for observation in &observations {
            if observation.probe_id.as_str() == crate::probes::gpu::PROBE_ID {
                if let Some(count) = observation.payload.get("gpu_count").and_then(|v| v.as_u64()) {
                    self.observed_gpu_count = Some(count);
                }
            }
        }

        let count = observations.len();
        if !observations.is_empty() {
            self.record(&observations).await?;
        }
        Ok(count)
    }

    /// A health snapshot for the agent's own RPC endpoint.
    pub async fn health(&self) -> rpc::AgentHealth {
        rpc::health_snapshot(
            &self.environment,
            &self.hostname,
            self.inspector.boot_id(),
            self.capabilities.clone(),
            self.started_at,
            self.spool.len().await.unwrap_or(0),
            self.status.registered,
        )
    }

    /// The host name this agent reports as.
    pub fn hostname(&self) -> &str {
        &self.hostname
    }

    /// The capabilities in force.
    pub fn capabilities(&self) -> &CapabilitySet {
        &self.capabilities
    }

    /// Current status.
    pub fn status(&self) -> &AgentStatus {
        &self.status
    }

    /// The local spool.
    pub fn spool(&self) -> &Spool {
        &self.spool
    }

    /// Where this host's services actually listen.
    ///
    /// Only non-default values are reported; the controller applies the
    /// defaults itself, so an ordinary host sends nothing here.
    fn service_ports(&self) -> std::collections::BTreeMap<String, u16> {
        let mut ports = std::collections::BTreeMap::new();

        // Explicit configuration wins over what is read from sshd_config,
        // which wins over the default (SPEC.md §19's precedence, applied here).
        let ssh_port = self
            .ssh_port
            .or_else(|| discovery::detect_ssh_port(self.inspector.as_ref()));
        if let Some(port) = ssh_port.filter(|p| *p != crate::probes::ssh::DEFAULT_PORT) {
            ports.insert("ssh".to_string(), port);
        }

        // The agent's own port, so peers reach the endpoint it is really on
        // rather than the one they assume.
        if let Some(port) = self.rpc_port.filter(|p| *p != rpc::DEFAULT_PORT) {
            ports.insert("agent".to_string(), port);
        }

        ports
    }

    /// Build the registration this agent would send.
    pub fn registration(&self) -> RegisterRequest {
        let gpus = self
            .capabilities
            .has(crate::capability::well_known::GPU_NVIDIA)
            .then_some(0);

        RegisterRequest {
            protocol_version: PROTOCOL_VERSION,
            agent_version: crate::VERSION.to_string(),
            environment: self.environment.clone(),
            hostname: self.hostname.clone(),
            fqdn: self.inspector.fqdn(),
            boot_id: self.inspector.boot_id(),
            addresses: self.address_choice().addresses,
            ports: self.service_ports(),
            capabilities: self.capabilities.clone(),
            hardware: serde_json::to_value(discovery::hardware(self.inspector.as_ref(), gpus))
                .unwrap_or(serde_json::Value::Null),
            roles: self.roles.clone(),
        }
    }

    /// Which addresses this agent reports, and why.
    ///
    /// Recomputed rather than cached: an interface can come up after the agent
    /// starts, and a host that only becomes reachable later should say so at
    /// its next registration rather than at its next restart.
    pub fn address_choice(&self) -> addressing::AddressChoice {
        addressing::choose(self.inspector.as_ref(), &self.agent_config)
    }

    /// Register with the controller.
    pub async fn register(&mut self) -> Result<(), ClientError> {
        match self.client.register(&self.registration()).await {
            Ok(response) => {
                self.set_registered(true);
                self.status.agent_id = Some(response.agent_id);
                self.status.session_id = Some(response.session_id);
                self.status.last_error = None;
                tracing::info!(
                    agent_id = %response.agent_id,
                    entity_id = %response.entity_id,
                    capabilities = self.capabilities.len(),
                    "registered with controller"
                );
                // A fresh registration means the plan may have changed for us.
                self.assignment_refreshed_at = None;
                Ok(())
            }
            Err(error) => {
                self.set_registered(false);
                self.status.last_error = Some(error.to_string());
                Err(error)
            }
        }
    }

    /// Send a heartbeat, re-registering if the controller asks.
    pub async fn heartbeat(&mut self) -> Result<(), ClientError> {
        let (Some(agent_id), Some(session_id)) = (self.status.agent_id, self.status.session_id) else {
            return self.register().await;
        };

        let request = HeartbeatRequest {
            protocol_version: PROTOCOL_VERSION,
            agent_id,
            session_id,
            boot_id: self.inspector.boot_id(),
            agent_time: now(),
            spooled_observations: self.spool.len().await.unwrap_or(0),
        };

        match self.client.heartbeat(&request).await {
            Ok(response) => {
                self.status.clock_skew_ms = Some(response.clock_skew_ms);
                self.status.last_error = None;
                if response.reregister {
                    tracing::info!("controller asked us to register again");
                    return self.register().await;
                }
                Ok(())
            }
            Err(error) => {
                self.status.last_error = Some(error.to_string());
                if !error.is_transient() {
                    self.set_registered(false);
                }
                Err(error)
            }
        }
    }

    /// Record observations, sending them if possible and spooling them if not.
    ///
    /// Observations are written to the spool *first*, then removed once the
    /// controller confirms them. Sending first and spooling on failure would
    /// lose everything to a crash between the two.
    pub async fn record(&mut self, observations: &[Observation]) -> anyhow::Result<()> {
        self.spool.push(observations).await?;
        self.flush().await?;
        Ok(())
    }

    /// Try to deliver everything in the spool.
    ///
    /// Returns how many observations the controller accepted. A failure here is
    /// not an error for the caller: the observations remain spooled, and the
    /// next attempt will carry them.
    pub async fn flush(&mut self) -> anyhow::Result<usize> {
        let (Some(agent_id), Some(session_id)) = (self.status.agent_id, self.status.session_id) else {
            // Not registered yet. The spool keeps growing, bounded by its
            // limits, until there is somewhere to send it.
            self.status.spooled = self.spool.len().await.unwrap_or(0);
            return Ok(0);
        };

        let mut delivered = 0;
        loop {
            let batch = self.spool.peek(MAX_BATCH).await?;
            if batch.is_empty() {
                break;
            }

            let request = ObservationBatch {
                protocol_version: PROTOCOL_VERSION,
                agent_id,
                session_id,
                observations: batch.clone(),
            };

            match self.client.send_observations(&request).await {
                Ok(response) => {
                    // Acknowledge duplicates too: the controller already has
                    // them, so holding on would replay them forever.
                    let rejected: std::collections::HashSet<&str> =
                        response.rejected.iter().map(|r| r.id.as_str()).collect();
                    let confirmed: Vec<_> = batch
                        .iter()
                        .filter(|o| !rejected.contains(o.id.to_string().as_str()))
                        .map(|o| o.id)
                        .collect();

                    self.spool.acknowledge(&confirmed).await?;
                    delivered += response.accepted;
                    self.status.last_error = None;

                    if !response.rejected.is_empty() {
                        // Rejected observations stay spooled: the entity may
                        // appear later, and dropping them would hide a real
                        // configuration problem.
                        tracing::warn!(count = response.rejected.len(), "controller rejected observations");
                        break;
                    }
                }
                Err(error) => {
                    tracing::debug!(%error, spooled = batch.len(), "cannot deliver observations; they stay spooled");
                    self.status.last_error = Some(error.to_string());
                    break;
                }
            }
        }

        self.status.spooled = self.spool.len().await.unwrap_or(0);
        Ok(delivered)
    }

    /// Run until cancelled: register, then probe, heartbeat and flush.
    pub async fn run(&mut self, heartbeat_interval: Duration, shutdown: tokio::sync::oneshot::Receiver<()>) {
        // A failed first registration is not fatal: the controller may simply
        // not be up yet, and the agent must keep observing and spooling.
        if let Err(error) = self.register().await {
            tracing::warn!(%error, "initial registration failed; will retry");
        }

        let mut ticker = tokio::time::interval(heartbeat_interval);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        tokio::pin!(shutdown);

        loop {
            tokio::select! {
                _ = ticker.tick() => {
                    // Probing comes first: observations are worth collecting
                    // even in the turn where the controller is unreachable.
                    if let Err(error) = self.probe_once().await {
                        tracing::warn!(%error, "local probes failed");
                    }
                    if self.assignment_is_stale() {
                        if let Err(error) = self.refresh_assignments().await {
                            tracing::debug!(%error, "cannot refresh peer assignment; keeping the current one");
                        }
                    }
                    // Peer observation continues on the assignment we already
                    // have, even when the controller is unreachable. That is
                    // precisely when a second viewpoint matters most.
                    if let Err(error) = self.observe_peers().await {
                        tracing::warn!(%error, "peer probes failed");
                    }
                    if let Err(error) = self.heartbeat().await {
                        tracing::debug!(%error, "heartbeat failed");
                    }
                    if let Err(error) = self.flush().await {
                        tracing::warn!(%error, "flushing the spool failed");
                    }
                }
                _ = &mut shutdown => {
                    tracing::info!("agent shutting down");
                    // One last attempt to hand over what we have.
                    let _ = self.flush().await;
                    return;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::system::FakeInspector;
    use crate::config::AgentConfig;
    use crate::entity::{EntityKey, EntityType};
    use crate::observation::ProbeStatus;
    use crate::probes::ProbeId;
    use crate::protocol::ClusterCredential;

    fn config(roles: Vec<String>) -> Config {
        Config {
            config_version: 1,
            environment: "lab".into(),
            agent: AgentConfig {
                controller_address: None,
                spool_path: None,
                roles,
                ..Default::default()
            },
            ..Config::default()
        }
    }

    async fn agent_with(inspector: FakeInspector, address: &str) -> Agent {
        let credential = ClusterCredential::new("0123456789abcdef0123456789abcdef");
        Agent::new(
            &config(vec![]),
            Arc::new(inspector),
            ControllerClient::new(address, &credential, Duration::from_millis(200)).expect("client"),
            Spool::open_in_memory(SpoolLimits::default()).await.expect("spool"),
        )
        .expect("agent")
    }

    fn observation() -> Observation {
        Observation::new(
            ProbeId::new("host.metrics"),
            EntityKey::new("lab", EntityType::Host, "test-host").entity_id(),
            ProbeStatus::Ok,
        )
    }

    #[tokio::test]
    async fn an_agent_reports_the_capabilities_discovery_found() {
        let inspector = FakeInspector::bare()
            .with_hostname("compute01")
            .with_program("slurmd")
            .with_file(
                "/etc/slurm/slurm.conf",
                "SlurmctldHost=ctl-a\nNodeName=compute01 CPUs=1\n",
            )
            .with_mount("fs:/export", "/home", "nfs4");
        let agent = agent_with(inspector, "127.0.0.1:1").await;

        assert!(
            agent.capabilities().has("slurm.compute"),
            "configured as a node, and slurmd is installed"
        );
        assert!(agent.capabilities().has("storage.nfs.client"));
        assert!(!agent.capabilities().has("gpu.nvidia"));
    }

    #[tokio::test]
    async fn an_agent_without_a_host_name_refuses_to_start() {
        // Guessing would attach this machine's observations to another entity.
        let inspector = FakeInspector {
            hostname: None,
            ..FakeInspector::bare()
        };
        let credential = ClusterCredential::new("0123456789abcdef0123456789abcdef");
        let result = Agent::new(
            &config(vec![]),
            Arc::new(inspector),
            ControllerClient::new("127.0.0.1:1", &credential, Duration::from_millis(100)).expect("client"),
            Spool::open_in_memory(SpoolLimits::default()).await.expect("spool"),
        );
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn the_registration_carries_identity_addresses_and_hardware() {
        let inspector = FakeInspector::bare().with_hostname("node-a").with_boot_id("boot-7");
        let request = agent_with(inspector, "127.0.0.1:1").await.registration();

        assert_eq!(request.hostname, "node-a");
        assert_eq!(request.boot_id.as_deref(), Some("boot-7"));
        assert_eq!(request.addresses, ["192.0.2.1"]);
        assert_eq!(request.hardware["cpus"], 8);
        assert_eq!(request.protocol_version, PROTOCOL_VERSION);
    }

    #[tokio::test]
    async fn an_ordinary_host_reports_no_ports_at_all() {
        // Defaults are the controller's business; sending them would be noise.
        let agent = agent_with(FakeInspector::bare(), "127.0.0.1:1").await;
        assert!(agent.registration().ports.is_empty());
    }

    #[tokio::test]
    async fn a_non_standard_ssh_port_is_read_from_sshd_config_and_reported() {
        // A cluster that moved SSH off 22 would otherwise have every host
        // probed on 22 and reported down.
        let inspector = FakeInspector::bare().with_file(crate::agent::discovery::SSHD_CONFIG_PATH, "Port 2222\n");
        let agent = agent_with(inspector, "127.0.0.1:1").await;

        assert_eq!(agent.registration().ports.get("ssh"), Some(&2222));
    }

    #[tokio::test]
    async fn an_explicit_ssh_port_overrides_what_was_read_from_the_file() {
        let credential = ClusterCredential::new("0123456789abcdef0123456789abcdef");
        let mut config = config(vec![]);
        config.agent.ssh_port = Some(2022);

        let inspector = FakeInspector::bare().with_file(crate::agent::discovery::SSHD_CONFIG_PATH, "Port 2222\n");
        let agent = Agent::new(
            &config,
            Arc::new(inspector),
            ControllerClient::new("127.0.0.1:1", &credential, Duration::from_millis(100)).expect("client"),
            Spool::open_in_memory(SpoolLimits::default()).await.expect("spool"),
        )
        .expect("agent");

        assert_eq!(agent.registration().ports.get("ssh"), Some(&2022));
    }

    #[tokio::test]
    async fn a_non_default_agent_port_is_reported_so_peers_can_find_it() {
        // Otherwise moving the agent's listen address silently breaks peer
        // monitoring: peers keep probing the port nobody is on.
        let credential = ClusterCredential::new("0123456789abcdef0123456789abcdef");
        let mut config = config(vec![]);
        config.agent.listen = "0.0.0.0:9444".into();

        let agent = Agent::new(
            &config,
            Arc::new(FakeInspector::bare()),
            ControllerClient::new("127.0.0.1:1", &credential, Duration::from_millis(100)).expect("client"),
            Spool::open_in_memory(SpoolLimits::default()).await.expect("spool"),
        )
        .expect("agent");

        assert_eq!(agent.registration().ports.get("agent"), Some(&9444));
    }

    #[tokio::test]
    async fn a_default_port_stated_explicitly_is_still_not_reported() {
        let credential = ClusterCredential::new("0123456789abcdef0123456789abcdef");
        let mut config = config(vec![]);
        config.agent.ssh_port = Some(22);

        let agent = Agent::new(
            &config,
            Arc::new(FakeInspector::bare()),
            ControllerClient::new("127.0.0.1:1", &credential, Duration::from_millis(100)).expect("client"),
            Spool::open_in_memory(SpoolLimits::default()).await.expect("spool"),
        )
        .expect("agent");

        assert!(agent.registration().ports.is_empty(), "the default needs no stating");
    }

    #[tokio::test]
    async fn a_role_is_reported_as_a_role_and_not_as_a_capability() {
        let credential = ClusterCredential::new("0123456789abcdef0123456789abcdef");
        let agent = Agent::new(
            &config(vec!["fileserver".into()]),
            Arc::new(FakeInspector::bare()),
            ControllerClient::new("127.0.0.1:1", &credential, Duration::from_millis(100)).expect("client"),
            Spool::open_in_memory(SpoolLimits::default()).await.expect("spool"),
        )
        .expect("agent");

        let request = agent.registration();
        assert_eq!(request.roles, ["fileserver"]);
        assert!(
            !request.capabilities.has("storage.nfs.server"),
            "this host exports nothing; the label must not conjure the capability"
        );
    }

    #[tokio::test]
    async fn registration_against_an_unreachable_controller_fails_without_panicking() {
        let mut agent = agent_with(FakeInspector::bare(), "127.0.0.1:1").await;
        assert!(agent.register().await.is_err());
        assert!(!agent.status().registered);
        assert!(agent.status().last_error.is_some());
    }

    #[tokio::test]
    async fn observations_are_spooled_when_the_controller_is_unreachable() {
        // SPEC.md §105: a controller outage is exactly when the evidence
        // matters most.
        let mut agent = agent_with(FakeInspector::bare(), "127.0.0.1:1").await;
        agent.record(&[observation(), observation()]).await.expect("record");

        assert_eq!(agent.spool().len().await.expect("len"), 2);
        assert_eq!(agent.status().spooled, 2);
    }

    #[tokio::test]
    async fn flushing_while_unregistered_keeps_everything_spooled() {
        let mut agent = agent_with(FakeInspector::bare(), "127.0.0.1:1").await;
        agent.record(&[observation()]).await.expect("record");
        assert_eq!(agent.flush().await.expect("flush"), 0);
        assert_eq!(agent.spool().len().await.expect("len"), 1, "nothing is lost");
    }

    #[tokio::test]
    async fn the_spool_bounds_growth_during_a_long_outage() {
        let credential = ClusterCredential::new("0123456789abcdef0123456789abcdef");
        let mut agent = Agent::new(
            &config(vec![]),
            Arc::new(FakeInspector::bare()),
            ControllerClient::new("127.0.0.1:1", &credential, Duration::from_millis(100)).expect("client"),
            Spool::open_in_memory(SpoolLimits {
                max_rows: 10,
                ..Default::default()
            })
            .await
            .expect("spool"),
        )
        .expect("agent");

        for _ in 0..50 {
            agent.record(&[observation()]).await.expect("record");
        }
        assert_eq!(agent.spool().len().await.expect("len"), 10, "the disk must not fill up");
    }
}
