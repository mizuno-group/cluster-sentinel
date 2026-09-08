//! The controller.
//!
//! In M1 the controller is *passive*: it discovers inventory, records what
//! integrations report, derives state, and persists all of it. It changes
//! nothing on the cluster — no `scontrol update`, no restarts, no remounts
//! (SPEC.md §113).
//!
//! The controller is not tied to any particular host. Which machine runs it is
//! deployment configuration (SPEC.md §42), and the schema does not assume there
//! is only one (SPEC.md §43).

pub mod agents;
pub mod api;
mod assignments;
mod diagnose;
mod discovery;
mod notify;
pub mod observer;
pub mod peers;
pub mod registration;
mod server;

pub use agents::{AgentRegistry, AgentSession, RegistrationKind};
pub use discovery::{DiscoveryReport, ProviderReport};
pub use notify::{min_severity, providers_from_config, NotifyOutcome};
pub use observer::{endpoint_for, Endpoint, RemoteObserver};
pub use peers::{assign, AssignmentPlan, Observer, ObserverRole, PeerAssignment};

/// Re-export for integration tests that check capability gating.
pub use peers::observer_candidates as observer_candidates_for_test;
pub use registration::{snapshot_from_registration, Registration};
pub use server::{serve, ServeOptions, ServerHandle};

use std::sync::Arc;

use crate::agent::SystemInspector;
use crate::config::Config;
use crate::diagnosis::{builtin_rules, DiagnosisEngine};
use crate::entity::{EntityId, EntityKey, EntityType};
use crate::incident::IncidentEngine;
use crate::integrations::slurm::{observe, ScontrolClient};
use crate::inventory::slurm::SlurmInventoryProvider;
use crate::inventory::static_config::StaticConfigProvider;
use crate::inventory::InventoryProvider;
use crate::persistence::{SqliteStore, StoreError};
use crate::state::{DebouncePolicy, ProbeMapping, StateComponent, StateEngine};

/// Name used for the scheduler entity when configuration does not name a
/// cluster. Deliberately generic: no deployment identifier belongs here.
pub const DEFAULT_SCHEDULER_NAME: &str = "slurm";

/// The controller's long-lived state.
pub struct Controller {
    config: Config,
    store: SqliteStore,
    providers: Vec<Arc<dyn InventoryProvider>>,
    engine: StateEngine,
    diagnosis: DiagnosisEngine,
    incidents: IncidentEngine,
}

impl Controller {
    /// Build a controller from configuration, wiring up the enabled providers.
    pub async fn new(config: Config, store: SqliteStore) -> Result<Self, StoreError> {
        store.ensure_environment(&config.environment).await?;

        let mut providers: Vec<Arc<dyn InventoryProvider>> = vec![Arc::new(StaticConfigProvider::new(config.clone()))];

        if config.discovery.slurm.enabled {
            providers.push(Arc::new(SlurmInventoryProvider::new(
                &config.environment,
                scheduler_name(&config),
                ScontrolClient::with_path(config.discovery.slurm.scontrol_path.clone()),
            )));
        }

        let mut engine = StateEngine::new();
        register_builtin_probes(&mut engine);

        // Resume from what is already known, so a restart does not re-learn
        // every host's state from scratch.
        for (_, state) in store.load_entity_states(&config.environment).await? {
            engine.seed(state);
        }

        // Resume the incidents that were already open, so a restart does not
        // re-alert on everything the operator is already dealing with.
        let mut incidents = IncidentEngine::new();
        incidents.seed(store.load_active_incidents(&config.environment).await?);

        Ok(Self {
            config,
            store,
            providers,
            engine,
            diagnosis: builtin_rules(),
            incidents,
        })
    }

    /// The incident engine.
    pub fn incident_engine(&self) -> &IncidentEngine {
        &self.incidents
    }

    /// The diagnosis engine.
    pub fn diagnosis_engine(&self) -> &DiagnosisEngine {
        &self.diagnosis
    }

    /// The configuration in force.
    pub fn config(&self) -> &Config {
        &self.config
    }

    /// The database.
    pub fn store(&self) -> &SqliteStore {
        &self.store
    }

    /// The state engine.
    pub fn engine(&self) -> &StateEngine {
        &self.engine
    }

    /// The state engine, mutably.
    pub fn engine_mut(&mut self) -> &mut StateEngine {
        &mut self.engine
    }

    /// Names of the enabled inventory providers.
    pub fn provider_names(&self) -> Vec<&str> {
        self.providers.iter().map(|p| p.name()).collect()
    }

    /// The scheduler entity name in force.
    pub fn scheduler_name(&self) -> String {
        scheduler_name(&self.config)
    }

    /// The entity the controller observes *as*.
    ///
    /// The controller is one viewpoint among several, and its observations have
    /// to say so. An unsigned observation cannot take part in quorum, which
    /// would leave the controller's own blind spots invisible to exactly the
    /// reasoning designed to catch them (SPEC.md §50).
    pub fn observer_entity(&self) -> Option<EntityId> {
        let hostname = crate::agent::system::LinuxInspector::new().hostname()?;
        Some(EntityKey::new(&self.config.environment, EntityType::Host, &hostname).entity_id())
    }
}

/// The scheduler entity's name: the configured cluster, else a generic default.
fn scheduler_name(config: &Config) -> String {
    config
        .entities
        .iter()
        .find(|e| e.entity_type == "scheduler")
        .map(|e| e.name.clone())
        .unwrap_or_else(|| DEFAULT_SCHEDULER_NAME.to_string())
}

/// Declare how the built-in probes feed into state.
///
/// This is the whole coupling between an integration and the state engine: a
/// probe id and the component it informs.
pub fn register_builtin_probes(engine: &mut StateEngine) {
    // A scheduler reporting DRAIN or DOWN is an authoritative statement, not a
    // flaky measurement, so it takes effect immediately (IMPLEMENTATION.md §70).
    engine.register(observe::PROBE_NODE, ProbeMapping::immediate(StateComponent::Scheduler));
    engine.register(
        observe::PROBE_CONTROLLER,
        ProbeMapping::immediate(StateComponent::Service),
    );

    // Network-facing probes are measurements, not statements, so they debounce:
    // one dropped packet is not an outage (SPEC.md §90).
    engine.register(
        crate::probes::network::PROBE_ID,
        ProbeMapping::new(StateComponent::Network),
    );
    engine.register(crate::probes::ssh::PROBE_ID, ProbeMapping::new(StateComponent::Ssh));
    engine.register(
        crate::probes::sentinel_rpc::PROBE_ID,
        ProbeMapping::new(StateComponent::Agent),
    );
    engine.register(
        crate::probes::systemd::PROBE_ID,
        ProbeMapping::new(StateComponent::Service),
    );

    // Host metrics are sampled locally and are noisy by nature. A transient
    // load spike must not move the host's state, so this needs more agreement
    // than a connection failure does.
    engine.register(
        crate::probes::host::PROBE_ID,
        ProbeMapping::new(StateComponent::Host).with_policy(DebouncePolicy {
            warning_threshold: 3,
            critical_threshold: 5,
            recovery_threshold: 2,
        }),
    );

    // Storage probes all inform the storage component. The active filesystem
    // probe is deliberately debounced no harder than the others: a mount that
    // is slow twice running is genuinely slow.
    for probe in crate::diagnosis::rules::storage::storage_probe_ids() {
        engine.register(probe, ProbeMapping::new(StateComponent::Storage));
    }

    engine.register(
        crate::probes::gpu::PROBE_ID,
        ProbeMapping::new(StateComponent::Accelerator),
    );

    // Kernel events inform the host's health, but gently. A single logged I/O
    // error is worth preserving and worth showing; it is not by itself a
    // reason to call a machine broken, and SPEC.md §83 is explicit that a
    // kernel event is not equivalent to a diagnosis.
    engine.register(
        crate::probes::journal::PROBE_ID,
        ProbeMapping::new(StateComponent::Host).with_policy(DebouncePolicy {
            warning_threshold: 2,
            critical_threshold: 6,
            recovery_threshold: 2,
        }),
    );

    // A reboot is a recorded fact about the host, and it is not a fault.
    engine.register(
        crate::controller::registration::PROBE_BOOT,
        ProbeMapping::immediate(StateComponent::Host),
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{EntityConfig, SlurmDiscoveryConfig};
    use crate::probes::ProbeId;

    fn config() -> Config {
        Config {
            config_version: 1,
            environment: "lab".into(),
            ..Config::default()
        }
    }

    async fn store() -> SqliteStore {
        SqliteStore::open_in_memory().await.expect("open")
    }

    #[tokio::test]
    async fn static_configuration_is_always_a_provider() {
        let controller = Controller::new(config(), store().await).await.expect("controller");
        assert_eq!(controller.provider_names(), ["static_config"]);
    }

    #[tokio::test]
    async fn slurm_is_only_a_provider_when_it_is_enabled() {
        let mut config = config();
        config.discovery.slurm = SlurmDiscoveryConfig {
            enabled: true,
            scontrol_path: None,
        };

        let controller = Controller::new(config, store().await).await.expect("controller");
        assert_eq!(controller.provider_names(), ["static_config", "slurm"]);
    }

    #[tokio::test]
    async fn the_scheduler_name_comes_from_configuration_not_from_the_binary() {
        let mut config = config();
        config.entities.push(EntityConfig {
            entity_type: "scheduler".into(),
            name: "research-cluster".into(),
            display_name: None,
            cluster: None,
            labels: Default::default(),
            capabilities: vec![],
            addresses: vec![],
            ports: Default::default(),
        });

        let controller = Controller::new(config, store().await).await.expect("controller");
        assert_eq!(controller.scheduler_name(), "research-cluster");
    }

    #[tokio::test]
    async fn an_unnamed_scheduler_falls_back_to_a_generic_default() {
        let controller = Controller::new(config(), store().await).await.expect("controller");
        assert_eq!(controller.scheduler_name(), DEFAULT_SCHEDULER_NAME);
    }

    #[tokio::test]
    async fn creating_a_controller_registers_its_environment() {
        let store = store().await;
        Controller::new(config(), store.clone()).await.expect("controller");
        assert_eq!(
            store.environments().await.expect("environments"),
            vec!["lab".to_string()]
        );
    }

    #[test]
    fn every_builtin_probe_is_mapped_to_a_state_component() {
        // A probe the engine does not know about produces observations that
        // change nothing, which is a silent failure. This is the list that
        // stops one being added without a mapping.
        let mut engine = StateEngine::new();
        register_builtin_probes(&mut engine);

        for probe in [
            observe::PROBE_NODE,
            observe::PROBE_CONTROLLER,
            crate::probes::network::PROBE_ID,
            crate::probes::ssh::PROBE_ID,
            crate::probes::sentinel_rpc::PROBE_ID,
            crate::probes::systemd::PROBE_ID,
            crate::probes::host::PROBE_ID,
            crate::controller::registration::PROBE_BOOT,
            crate::probes::nfs::PROBE_CLIENT_MOUNT,
            crate::probes::nfs::PROBE_CLIENT_IO,
            crate::probes::nfs::PROBE_SERVER_PORT,
            crate::probes::nfs::PROBE_SERVER_EXPORTS,
            crate::probes::gpu::PROBE_ID,
            crate::probes::journal::PROBE_ID,
        ] {
            assert!(engine.knows(&ProbeId::new(probe)), "{probe} has no state mapping");
        }

        assert!(!engine.knows(&ProbeId::new("never.registered")));
    }

    #[test]
    fn the_agent_ssh_and_network_components_are_kept_separate() {
        // Telling these apart is the whole point of M4: an agent failure, an
        // SSH failure and an unreachable host must not collapse into one state.
        let mut engine = StateEngine::new();
        register_builtin_probes(&mut engine);

        let entity = crate::entity::EntityKey::new("lab", crate::entity::EntityType::Host, "node-a").entity_id();
        let observe_failure = |engine: &mut StateEngine, probe: &str| {
            for _ in 0..3 {
                engine.ingest(&crate::observation::Observation::new(
                    ProbeId::new(probe),
                    entity,
                    crate::observation::ProbeStatus::Failed,
                ));
            }
        };
        let observe_ok = |engine: &mut StateEngine, probe: &str| {
            for _ in 0..3 {
                engine.ingest(&crate::observation::Observation::new(
                    ProbeId::new(probe),
                    entity,
                    crate::observation::ProbeStatus::Ok,
                ));
            }
        };

        observe_failure(&mut engine, crate::probes::sentinel_rpc::PROBE_ID);
        observe_ok(&mut engine, crate::probes::ssh::PROBE_ID);
        observe_ok(&mut engine, crate::probes::network::PROBE_ID);

        let state = engine.state(entity).expect("state");
        assert_eq!(
            state.component(StateComponent::Agent),
            crate::state::Health::Unavailable
        );
        assert_eq!(state.component(StateComponent::Ssh), crate::state::Health::Healthy);
        assert_eq!(state.component(StateComponent::Network), crate::state::Health::Healthy);
    }
}
