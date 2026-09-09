//! One discovery cycle.
//!
//! The cycle is designed around a single principle: **a failing provider must
//! degrade the picture, not erase it.** If Slurm is unreachable, the static
//! inventory still merges, the fileservers are still monitored, and the
//! previous Slurm-derived state is left standing rather than being rewritten as
//! healthy.

use crate::entity::DiscoverySource;
use crate::integrations::slurm::observe::observations_from_view;
use crate::inventory::slurm::SlurmInventoryProvider;
use crate::inventory::{Inventory, InventoryError};
use crate::observation::Observation;
use crate::persistence::StoreError;
use crate::state::StateTransition;

use super::{Controller, RemoteObserver};

/// What one provider contributed to a cycle.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderReport {
    /// Provider name.
    pub provider: String,
    /// Entities it reported.
    pub entities: usize,
    /// Dependency edges it reported.
    pub dependencies: usize,
    /// Why it failed, if it did.
    pub error: Option<String>,
}

impl ProviderReport {
    /// Whether this provider succeeded.
    pub fn is_ok(&self) -> bool {
        self.error.is_none()
    }
}

/// The outcome of one discovery cycle.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct DiscoveryReport {
    /// One entry per provider, in the order they ran.
    pub providers: Vec<ProviderReport>,
    /// Entities in the inventory afterwards.
    pub entities: usize,
    /// Dependency edges afterwards.
    pub dependencies: usize,
    /// Observations recorded this cycle.
    pub observations: usize,
    /// State transitions this cycle caused.
    pub transitions: Vec<StateTransition>,
    /// Diagnoses drawn from the resulting picture.
    ///
    /// Reported, not acted on. Incidents are opened and notified by the
    /// diagnosis loop, which is the only place that does either.
    pub diagnoses: Vec<crate::diagnosis::Diagnosis>,
}

impl DiscoveryReport {
    /// Whether every provider succeeded.
    pub fn all_providers_ok(&self) -> bool {
        self.providers.iter().all(ProviderReport::is_ok)
    }

    /// Providers that failed.
    pub fn failures(&self) -> impl Iterator<Item = &ProviderReport> {
        self.providers.iter().filter(|p| !p.is_ok())
    }
}

impl Controller {
    /// Run one discovery cycle and persist everything it produced.
    pub async fn discover_once(&mut self) -> Result<DiscoveryReport, StoreError> {
        let environment = self.config().environment.clone();
        let scheduler_name = self.scheduler_name();

        let mut inventory = self.store().load_inventory(&environment).await?;
        let mut report = DiscoveryReport::default();
        let mut observations: Vec<Observation> = Vec::new();
        let mut snapshots: Vec<crate::inventory::InventorySnapshot> = Vec::new();

        let providers = self.providers.clone();
        for provider in providers {
            match provider.discover().await {
                Ok(snapshot) => {
                    report.providers.push(ProviderReport {
                        provider: provider.name().to_string(),
                        entities: snapshot.entities.len(),
                        dependencies: snapshot.dependencies.len(),
                        error: None,
                    });
                    inventory.merge(&snapshot);
                    if let Some(source) = &snapshot.source {
                        inventory.mark_absent_as_stale(source, &snapshot);
                    }
                    snapshots.push(snapshot);
                }
                Err(error) => {
                    // Record the failure and move on. Marking this provider's
                    // entities stale here would turn "we could not look" into
                    // "they are gone".
                    tracing::warn!(provider = provider.name(), %error, "inventory provider failed");
                    report.providers.push(ProviderReport {
                        provider: provider.name().to_string(),
                        entities: 0,
                        dependencies: 0,
                        error: Some(error.to_string()),
                    });
                }
            }
        }

        // The controller is itself an observer: it probes reachability, SSH and
        // agent health from where it stands. One viewpoint is not enough to
        // call a host dead, which is why these observations carry an observer
        // and the diagnosis engine weighs them together (SPEC.md §50).
        if self.config().controller.observe {
            let mut observer = RemoteObserver::with_schedules(&self.config().probes);
            if let Some(entity) = self.observer_entity() {
                observer = observer.observed_by(entity);
            }
            observations.extend(observer.observe_all(inventory.entities()).await);
        }

        // Slurm contributes observations as well as inventory: what the
        // scheduler believes about each node is itself a fact worth recording.
        if let Some(slurm) = self.slurm_provider() {
            match slurm.collect_view().await {
                Ok(view) => observations.extend(observations_from_view(&environment, &scheduler_name, &view)),
                Err(error) => {
                    tracing::warn!(%error, "slurm observation collection failed");
                }
            }
        }

        report.entities = inventory.len();
        report.dependencies = inventory.graph().len();
        self.store().save_inventory(&inventory).await?;
        for snapshot in &snapshots {
            self.reconcile_snapshot_capabilities(snapshot).await?;
        }

        // Observations must be stored before the state they justify, so that a
        // crash between the two leaves evidence without conclusions rather than
        // conclusions without evidence.
        let ingested = self.store().ingest_observations(&observations).await?;
        report.observations = ingested.inserted;

        report.transitions = self.engine.ingest_all(&observations);
        for transition in &report.transitions {
            self.store().save_state_transition(transition).await?;
        }
        for state in self.engine.states() {
            self.store().save_entity_state(state).await?;
        }

        // Diagnosis runs here so a caller of `discover_once` -- the CLI's
        // `sentinel discover` -- can show what the new picture implies.
        //
        // **Correlation deliberately does not.** Opening an incident is what
        // makes it news, and news is sent from one place: the diagnosis loop,
        // which notifies. Correlating here as well meant whichever loop ran
        // first consumed the opening, so an incident that happened to be
        // opened by a discovery cycle was recorded and never announced. That
        // is a missed alert, arriving non-deterministically, which is worse
        // than no alerting at all because it looks like it works.
        report.diagnoses = self.diagnose().await?;

        Ok(report)
    }

    /// The Slurm provider, if it is enabled.
    fn slurm_provider(&self) -> Option<SlurmInventoryProvider> {
        self.config().discovery.slurm.enabled.then(|| {
            SlurmInventoryProvider::new(
                &self.config().environment,
                self.scheduler_name(),
                crate::integrations::slurm::ScontrolClient::with_path(
                    self.config().discovery.slurm.scontrol_path.clone(),
                ),
            )
        })
    }

    /// Merge a snapshot directly, bypassing the providers.
    ///
    /// This is how a fixture, an agent registration, or a test injects
    /// inventory without a live cluster.
    pub async fn ingest_snapshot(
        &mut self,
        snapshot: &crate::inventory::InventorySnapshot,
    ) -> Result<Inventory, StoreError> {
        let environment = self.config().environment.clone();
        let mut inventory = self.store().load_inventory(&environment).await?;
        inventory.merge(snapshot);
        self.store().save_inventory(&inventory).await?;
        self.reconcile_snapshot_capabilities(snapshot).await?;
        Ok(inventory)
    }

    /// Let a snapshot retract capability claims it no longer makes.
    ///
    /// The in-memory merge unions capabilities, which is right across
    /// providers but wrong within one: if this provider has stopped reporting a
    /// capability it used to report, the claim must not outlive it.
    async fn reconcile_snapshot_capabilities(
        &self,
        snapshot: &crate::inventory::InventorySnapshot,
    ) -> Result<(), StoreError> {
        let Some(source) = &snapshot.source else {
            return Ok(());
        };
        for entity in &snapshot.entities {
            self.store()
                .reconcile_capabilities(entity.id, source, &entity.capabilities)
                .await?;
        }
        Ok(())
    }

    /// Record observations and update state from them.
    ///
    /// Shared by the discovery cycle and by agent ingestion in M2.
    pub async fn ingest_observations(
        &mut self,
        observations: &[Observation],
    ) -> Result<Vec<StateTransition>, StoreError> {
        self.store().ingest_observations(observations).await?;
        let transitions = self.engine.ingest_all(observations);
        for transition in &transitions {
            self.store().save_state_transition(transition).await?;
        }
        for state in self.engine.states() {
            self.store().save_entity_state(state).await?;
        }
        Ok(transitions)
    }
}

/// Attribute an error to a provider, for reporting.
#[allow(dead_code)]
fn provider_of(error: &InventoryError) -> &str {
    match error {
        InventoryError::Unavailable { provider, .. } | InventoryError::Malformed { provider, .. } => provider,
    }
}

/// The discovery source a provider's entities carry.
#[allow(dead_code)]
fn source_for(provider: &str) -> DiscoverySource {
    match provider {
        "static_config" => DiscoverySource::StaticConfig,
        other => DiscoverySource::Integration(other.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Config, EntityConfig, SlurmDiscoveryConfig};
    use crate::entity::{EntityKey, EntityType};
    use crate::integrations::slurm::{parser, SlurmView};
    use crate::inventory::slurm::snapshot_from_view;
    use crate::persistence::SqliteStore;
    use crate::state::{Health, StateComponent};

    fn host_entity(name: &str) -> EntityConfig {
        EntityConfig {
            entity_type: "host".into(),
            name: name.into(),
            display_name: None,
            cluster: None,
            labels: Default::default(),
            capabilities: vec!["storage.nfs.server".into()],
            addresses: vec![],
            ports: Default::default(),
        }
    }

    async fn controller(config: Config) -> Controller {
        Controller::new(config, SqliteStore::open_in_memory().await.expect("open"))
            .await
            .expect("controller")
    }

    /// A controller that does not probe remotely.
    ///
    /// These tests are about inventory merging and state derivation, not about
    /// observation. Leaving it on would spend a connection timeout per
    /// unresolvable fixture name and assert nothing extra; observation is
    /// covered in `controller::observer` and the M4 acceptance tests.
    fn config_with(entities: Vec<EntityConfig>) -> Config {
        let mut config = Config {
            config_version: 1,
            environment: "lab".into(),
            entities,
            ..Config::default()
        };
        config.controller.observe = false;
        config
    }

    fn view(nodes: &str, ping: &str) -> SlurmView {
        SlurmView {
            nodes: parser::parse_nodes(nodes),
            partitions: Vec::new(),
            controllers: parser::parse_ping(ping),
        }
    }

    #[tokio::test]
    async fn a_cycle_persists_the_statically_declared_inventory() {
        let mut controller = controller(config_with(vec![host_entity("fileserver-a")])).await;
        let report = controller.discover_once().await.expect("cycle");

        assert!(report.all_providers_ok());
        assert_eq!(report.entities, 1);

        let stored = controller.store().load_entities("lab").await.expect("load");
        assert_eq!(stored.len(), 1);
        assert!(stored[0].capabilities.has("storage.nfs.server"));
    }

    #[tokio::test]
    async fn repeated_cycles_converge_rather_than_accumulate() {
        let mut controller = controller(config_with(vec![host_entity("a"), host_entity("b")])).await;
        for _ in 0..3 {
            controller.discover_once().await.expect("cycle");
        }
        assert_eq!(controller.store().load_entities("lab").await.expect("load").len(), 2);
    }

    #[tokio::test]
    async fn a_failing_provider_does_not_erase_what_others_reported() {
        // Slurm is enabled but scontrol does not exist.
        let mut config = config_with(vec![host_entity("fileserver-a")]);
        config.discovery.slurm = SlurmDiscoveryConfig {
            enabled: true,
            scontrol_path: Some("/nonexistent/scontrol".into()),
        };

        let mut controller = controller(config).await;
        let report = controller.discover_once().await.expect("cycle");

        assert!(!report.all_providers_ok());
        assert_eq!(report.failures().count(), 1);
        assert_eq!(report.entities, 1, "the fileserver is still monitored");

        let stored = controller.store().load_entities("lab").await.expect("load");
        assert_eq!(stored[0].canonical_name, "fileserver-a");
        assert_eq!(stored[0].lifecycle_state, crate::entity::LifecycleState::Active);
    }

    #[tokio::test]
    async fn slurm_derived_observations_drive_the_scheduler_component() {
        let mut controller = controller(config_with(vec![])).await;
        let view = view("NodeName=n1 State=IDLE+DRAIN Reason=maintenance\n", "");

        controller
            .ingest_snapshot(&snapshot_from_view("lab", "sched", &view))
            .await
            .expect("inventory");
        let observations = observations_from_view("lab", "sched", &view);
        controller
            .ingest_observations(&observations)
            .await
            .expect("observations");

        let host = EntityKey::new("lab", EntityType::Host, "n1").entity_id();
        let state = controller.engine().state(host).expect("state");
        assert_eq!(state.component(StateComponent::Scheduler), Health::Degraded);
        assert_eq!(state.overall, Health::Degraded);
    }

    #[tokio::test]
    async fn a_healthy_node_reads_as_healthy() {
        let mut controller = controller(config_with(vec![])).await;
        let view = view("NodeName=n1 State=IDLE\n", "");
        controller
            .ingest_snapshot(&snapshot_from_view("lab", "sched", &view))
            .await
            .expect("inventory");
        controller
            .ingest_observations(&observations_from_view("lab", "sched", &view))
            .await
            .expect("observations");

        let host = EntityKey::new("lab", EntityType::Host, "n1").entity_id();
        assert_eq!(controller.engine().state(host).expect("state").overall, Health::Healthy);
    }

    #[tokio::test]
    async fn state_survives_a_controller_restart() {
        let store = SqliteStore::open_in_memory().await.expect("open");
        let config = config_with(vec![]);
        let view = view("NodeName=n1 State=DOWN\n", "");
        let host = EntityKey::new("lab", EntityType::Host, "n1").entity_id();

        {
            let mut controller = Controller::new(config.clone(), store.clone())
                .await
                .expect("controller");
            controller
                .ingest_snapshot(&snapshot_from_view("lab", "sched", &view))
                .await
                .expect("inventory");
            controller
                .ingest_observations(&observations_from_view("lab", "sched", &view))
                .await
                .expect("observations");
        }

        let restarted = Controller::new(config, store).await.expect("restarted controller");
        let state = restarted.engine().state(host).expect("state resumed");
        assert_eq!(state.component(StateComponent::Scheduler), Health::Unavailable);
    }

    #[tokio::test]
    async fn ingesting_the_same_observations_twice_stores_them_once() {
        let mut controller = controller(config_with(vec![])).await;
        let view = view("NodeName=n1 State=IDLE\n", "");
        controller
            .ingest_snapshot(&snapshot_from_view("lab", "sched", &view))
            .await
            .expect("inventory");

        let observations = observations_from_view("lab", "sched", &view);
        controller.ingest_observations(&observations).await.expect("first");
        controller.ingest_observations(&observations).await.expect("replay");

        let host = EntityKey::new("lab", EntityType::Host, "n1").entity_id();
        assert_eq!(controller.store().observation_count(host).await.expect("count"), 1);
    }

    #[tokio::test]
    async fn transitions_are_recorded_for_the_timeline() {
        let mut controller = controller(config_with(vec![])).await;
        let healthy = view("NodeName=n1 State=IDLE\n", "");
        let drained = view("NodeName=n1 State=IDLE+DRAIN\n", "");
        controller
            .ingest_snapshot(&snapshot_from_view("lab", "sched", &healthy))
            .await
            .expect("inventory");

        controller
            .ingest_observations(&observations_from_view("lab", "sched", &healthy))
            .await
            .expect("healthy");
        let transitions = controller
            .ingest_observations(&observations_from_view("lab", "sched", &drained))
            .await
            .expect("drained");

        assert_eq!(transitions.len(), 1);
        assert_eq!(transitions[0].from, Health::Healthy);
        assert_eq!(transitions[0].to, Health::Degraded);

        let host = EntityKey::new("lab", EntityType::Host, "n1").entity_id();
        let stored = controller.store().recent_transitions(host, 10).await.expect("load");
        assert_eq!(stored.len(), 2, "healthy on first sight, then degraded");
    }
}
