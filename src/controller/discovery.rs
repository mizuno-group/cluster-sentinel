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
    /// `uses_storage` edges derived from reported NFS mounts.
    pub storage_edges: usize,
    /// Mounts whose server could not be tied to a known host.
    pub unresolved_storage_servers: Vec<crate::inventory::nfs::UnresolvedServer>,
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

        // Storage topology, derived from the mounts the agents just reported.
        //
        // After the providers, because it resolves each mount's server against
        // the hosts they contributed; before saving, because the edges it adds
        // are part of the same picture.
        if self.config().discovery.nfs.enabled {
            let topology = self.derive_storage_topology(&inventory).await?;
            report.storage_edges = topology.edges();
            report.unresolved_storage_servers = topology.unresolved.clone();

            for unresolved in &topology.unresolved {
                tracing::info!(
                    server = %unresolved.server,
                    clients = unresolved.clients.len(),
                    "an NFS mount names an address belonging to no known host; declare it to include it in diagnosis"
                );
            }

            inventory.merge(&topology.snapshot);
            if let Some(source) = &topology.snapshot.source {
                inventory.mark_absent_as_stale(source, &topology.snapshot);
                // A node that moves to another fileserver stops mounting the
                // first. Without this the graph only ever grows and it keeps
                // voting in that fileserver's failures.
                inventory.retract_absent_dependencies(source, &topology.snapshot);
                let keep: Vec<uuid::Uuid> = topology.snapshot.dependencies.iter().map(|e| e.id).collect();
                self.store().reconcile_dependencies(source, &keep).await?;
            }
            report.providers.push(ProviderReport {
                provider: crate::inventory::nfs::SOURCE.to_string(),
                entities: topology.snapshot.entities.len(),
                dependencies: topology.snapshot.dependencies.len(),
                error: None,
            });
            snapshots.push(topology.snapshot);
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

        // Refreshed here because the graph has just been merged: a storage
        // domain discovered this cycle should be answered by this cycle's
        // observations, not the next one's.
        self.storage_providers = super::storage_providers(&inventory);

        report.transitions = self.ingest_into_engine(&observations);
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

    /// Build the storage topology from the latest reported mounts.
    async fn derive_storage_topology(
        &self,
        inventory: &crate::inventory::Inventory,
    ) -> Result<crate::inventory::nfs::StorageTopology, StoreError> {
        let environment = self.config().environment.clone();
        let mut observations = Vec::new();

        for entity in inventory.entities() {
            if entity.entity_type != crate::entity::EntityType::Host {
                continue;
            }
            // The newest of each probe, not the newest N observations. A
            // count-based window fills with whatever runs most often: peer
            // reachability every five seconds from every observer, against a
            // mount report every thirty. On a host with three observers the
            // mount report is gone from such a window inside a minute, and
            // when it is, the storage domains only that host mounts vanish
            // from the snapshot and are marked stale -- a topology that
            // flickers in step with the eviction rather than with the mounts.
            //
            // No age bound here, unlike diagnosis. The last known mount table
            // is the best available answer to "what does this host use"; an
            // agent that has stopped reporting is a question about that host's
            // health, which is answered separately and would only be answered
            // twice, worse, by letting the graph forget.
            if let Some(latest) = self
                .store()
                .latest_observations(entity.id)
                .await?
                .into_iter()
                .find(|o| o.probe_id.as_str() == crate::probes::nfs::PROBE_CLIENT_MOUNT)
            {
                observations.push(latest);
            }
        }

        Ok(crate::inventory::nfs::topology_from_mounts(
            &environment,
            inventory,
            observations.iter(),
        ))
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
    /// The server-side storage observations, re-aimed at the storage domains
    /// the host provides.
    ///
    /// A storage entity is a concept: nothing probes it directly, so its
    /// health was permanently `unknown` and it sat in `status` saying nothing
    /// for as long as it existed. What it needs is exactly what its provider's
    /// export probes already found out.
    ///
    /// Only the **server** side is copied. A host can be both a fileserver and
    /// an NFS client -- a compute node exporting a scratch tree, for instance
    /// -- and both sides feed the same `storage` component on the host, so
    /// mirroring the component wholesale would report a wedged *client* mount
    /// as a failure of what that host *serves*. That points at the wrong
    /// machine, which is the specific mistake this system exists to avoid.
    ///
    /// The copy keeps the original's id, so the storage entity's evidence
    /// leads to a real stored observation rather than to a synthetic one, and
    /// nothing extra is written to the database. Feeding these through the
    /// state engine rather than computing a health directly is what gives them
    /// the same debounce as everything else: one dropped packet is not an
    /// outage here either.
    pub fn storage_views(&self, observations: &[Observation]) -> Vec<Observation> {
        const SERVER_PROBES: [&str; 2] = [
            crate::probes::nfs::PROBE_SERVER_PORT,
            crate::probes::nfs::PROBE_SERVER_EXPORTS,
        ];

        let mut views = Vec::new();
        for observation in observations {
            if !SERVER_PROBES.contains(&observation.probe_id.as_str()) {
                continue;
            }
            let Some(storages) = self.storage_providers.get(&observation.target_entity) else {
                continue;
            };
            for storage in storages {
                let mut view = observation.clone();
                view.target_entity = *storage;
                views.push(view);
            }
        }
        views
    }

    /// Feed observations to the state engine, storage domains included.
    ///
    /// The single place that does it. Observations reach the controller by
    /// three routes -- its own probing, an agent's batch, and this method --
    /// and the storage views have to be derived on all of them. The one that
    /// matters most is the agent batch: `nfs.server.exports` is a local probe,
    /// so an agent's report is the *only* way it ever arrives.
    pub(super) fn ingest_into_engine(&mut self, observations: &[Observation]) -> Vec<StateTransition> {
        let views = self.storage_views(observations);
        let mut transitions = self.engine.ingest_all(observations);
        transitions.extend(self.engine.ingest_all(&views));
        transitions
    }

    pub async fn ingest_observations(
        &mut self,
        observations: &[Observation],
    ) -> Result<Vec<StateTransition>, StoreError> {
        self.store().ingest_observations(observations).await?;
        let transitions = self.ingest_into_engine(observations);
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
