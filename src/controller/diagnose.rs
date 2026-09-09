//! Running the diagnosis pass.
//!
//! Diagnosis reads what is already stored — inventory, state, observations —
//! and writes conclusions. It never probes anything itself, which is what makes
//! a diagnosis reproducible: the same stored evidence always yields the same
//! answer, and an operator can re-run it later and check.

use std::collections::{BTreeMap, HashMap};

use crate::diagnosis::{Diagnosis, DiagnosisContext, ObservationIndex};
use crate::entity::EntityId;
use crate::incident::IncidentUpdate;
use crate::persistence::StoreError;
use crate::state::{Classification, EntityState};

use super::Controller;

impl Controller {
    /// Run every diagnosis rule against the current picture.
    pub async fn diagnose(&self) -> Result<Vec<Diagnosis>, StoreError> {
        let environment = self.config().environment.clone();
        let inventory = self.store().load_inventory(&environment).await?;

        // Prefer the live state engine over the database: it is what the
        // observations in this cycle have just updated, and reloading would
        // race with the write.
        let mut states: HashMap<EntityId, EntityState> =
            self.engine().states().map(|s| (s.entity, s.clone())).collect();
        if states.is_empty() {
            states = self
                .store()
                .load_entity_states(&environment)
                .await?
                .into_iter()
                .collect();
        }

        let mut observations = ObservationIndex::new();
        for entity in inventory.entities() {
            for observation in self.store().latest_observations(entity.id).await? {
                observations.insert(observation);
            }
        }

        let context = DiagnosisContext {
            environment: &environment,
            inventory: &inventory,
            states: &states,
            observations: &observations,
        };

        Ok(self.diagnosis_engine().diagnose(&context))
    }

    /// Run diagnosis and apply the classifications it implies to entity state.
    ///
    /// A classification is the short machine-readable label an operator sees
    /// next to an entity. It comes from the diagnosis rather than from a probe,
    /// because only a rule has looked at enough evidence to justify one.
    ///
    /// Every entity is rewritten, including the ones this cycle said nothing
    /// about: a diagnosis that no longer fires must take its label with it.
    pub async fn diagnose_and_classify(&mut self) -> Result<Vec<Diagnosis>, StoreError> {
        let diagnoses = self.diagnose().await?;

        let mut implied: BTreeMap<EntityId, Vec<Classification>> = BTreeMap::new();
        for diagnosis in &diagnoses {
            let Some(classification) = crate::diagnosis::rules::slurm::classification_for(diagnosis) else {
                continue;
            };
            for entity in &diagnosis.affected_entities {
                implied
                    .entry(*entity)
                    .or_default()
                    .push(Classification::new(classification));
            }
        }

        let entities: Vec<EntityId> = self.engine().states().map(|s| s.entity).collect();
        for entity in entities {
            let classifications = implied.remove(&entity).unwrap_or_default();
            if let Some(state) = self.engine_mut().state_mut(entity) {
                state.set_classifications(classifications);
            }
        }

        for state in self.engine().states() {
            self.store().save_entity_state(state).await?;
        }

        Ok(diagnoses)
    }

    /// Diagnose, classify, then fold the result into incidents.
    ///
    /// The order matters: incidents are built from diagnoses, and diagnoses
    /// from state, so each stage sees a settled picture from the one before.
    pub async fn diagnose_and_correlate(&mut self) -> Result<(Vec<Diagnosis>, IncidentUpdate), StoreError> {
        let diagnoses = self.diagnose_and_classify().await?;

        let environment = self.config().environment.clone();
        let inventory = self.store().load_inventory(&environment).await?;
        let states: BTreeMap<EntityId, EntityState> = self.engine().states().map(|s| (s.entity, s.clone())).collect();

        let update = self.incidents.reconcile(&diagnoses, inventory.graph(), &states);

        for incident in update.all() {
            self.store().save_incident(&environment, incident).await?;
        }

        Ok((diagnoses, update))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::diagnosis::kind;
    use crate::entity::{EntityKey, EntityType};
    use crate::integrations::slurm::{parser, SlurmView};
    use crate::inventory::slurm::snapshot_from_view;
    use crate::persistence::SqliteStore;
    use crate::state::classification;

    async fn controller() -> Controller {
        let config = Config {
            config_version: 1,
            environment: "lab".into(),
            ..Config::default()
        };
        Controller::new(config, SqliteStore::open_in_memory().await.expect("store"))
            .await
            .expect("controller")
    }

    fn view(nodes: &str) -> SlurmView {
        SlurmView {
            nodes: parser::parse_nodes(nodes),
            partitions: Vec::new(),
            controllers: Vec::new(),
        }
    }

    /// Give a host positive evidence that it is reachable and answering.
    async fn make_host_look_healthy(controller: &mut Controller, name: &str) {
        use crate::observation::{Observation, ProbeStatus};
        use crate::probes::ProbeId;

        let host = EntityKey::new("lab", EntityType::Host, name).entity_id();
        let mut observations = Vec::new();
        for _ in 0..3 {
            observations.push(Observation::new(
                ProbeId::new(crate::probes::network::PROBE_ID),
                host,
                ProbeStatus::Ok,
            ));
            observations.push(Observation::new(
                ProbeId::new(crate::probes::sentinel_rpc::PROBE_ID),
                host,
                ProbeStatus::Ok,
            ));
        }
        controller
            .ingest_observations(&observations)
            .await
            .expect("host evidence");
    }

    #[tokio::test]
    async fn a_healthy_cluster_yields_no_diagnoses() {
        let mut controller = controller().await;
        let view = view("NodeName=n1 State=IDLE\n");
        controller
            .ingest_snapshot(&snapshot_from_view("lab", "sched", &view))
            .await
            .expect("inventory");
        controller
            .ingest_observations(&crate::integrations::slurm::observe::observations_from_view(
                "lab", "sched", &view,
            ))
            .await
            .expect("observations");

        assert!(controller.diagnose().await.expect("diagnose").is_empty());
    }

    #[tokio::test]
    async fn a_drained_node_on_a_healthy_host_is_diagnosed_and_classified() {
        let mut controller = controller().await;
        let view = view("NodeName=n1 State=IDLE+DRAIN Reason=maintenance\n");
        controller
            .ingest_snapshot(&snapshot_from_view("lab", "sched", &view))
            .await
            .expect("inventory");
        make_host_look_healthy(&mut controller, "n1").await;
        controller
            .ingest_observations(&crate::integrations::slurm::observe::observations_from_view(
                "lab", "sched", &view,
            ))
            .await
            .expect("observations");

        let diagnoses = controller.diagnose_and_classify().await.expect("diagnose");
        assert_eq!(diagnoses.len(), 1);
        assert!(diagnoses[0].is(kind::SLURM_ONLY_DEGRADATION));

        let host = EntityKey::new("lab", EntityType::Host, "n1").entity_id();
        let state = controller.engine().state(host).expect("state");
        assert!(state.has_classification(classification::SCHEDULER_DEGRADED));
    }

    #[tokio::test]
    async fn an_unresponsive_node_on_a_healthy_host_is_a_slurmd_failure() {
        let mut controller = controller().await;
        let view = view("NodeName=n1 State=DOWN* Reason=Not responding\n");
        controller
            .ingest_snapshot(&snapshot_from_view("lab", "sched", &view))
            .await
            .expect("inventory");
        make_host_look_healthy(&mut controller, "n1").await;
        controller
            .ingest_observations(&crate::integrations::slurm::observe::observations_from_view(
                "lab", "sched", &view,
            ))
            .await
            .expect("observations");

        let diagnoses = controller.diagnose().await.expect("diagnose");
        assert_eq!(diagnoses.len(), 1);
        assert!(diagnoses[0].is(kind::SLURMD_SERVICE_FAILURE));
    }

    #[tokio::test]
    async fn an_unresponsive_node_with_no_host_evidence_is_not_blamed_on_slurmd() {
        // The controller has not probed this host, so it has no grounds to say
        // the machine is fine and only the daemon is broken.
        let mut controller = controller().await;
        let view = view("NodeName=n1 State=DOWN* Reason=Not responding\n");
        controller
            .ingest_snapshot(&snapshot_from_view("lab", "sched", &view))
            .await
            .expect("inventory");
        controller
            .ingest_observations(&crate::integrations::slurm::observe::observations_from_view(
                "lab", "sched", &view,
            ))
            .await
            .expect("observations");

        assert!(controller.diagnose().await.expect("diagnose").is_empty());
    }

    #[tokio::test]
    async fn diagnoses_carry_evidence_that_can_be_looked_up() {
        let mut controller = controller().await;
        let view = view("NodeName=n1 State=IDLE+DRAIN Reason=maintenance\n");
        controller
            .ingest_snapshot(&snapshot_from_view("lab", "sched", &view))
            .await
            .expect("inventory");
        make_host_look_healthy(&mut controller, "n1").await;
        controller
            .ingest_observations(&crate::integrations::slurm::observe::observations_from_view(
                "lab", "sched", &view,
            ))
            .await
            .expect("observations");

        let diagnoses = controller.diagnose().await.expect("diagnose");
        let diagnosis = &diagnoses[0];

        assert!(
            !diagnosis.evidence.is_empty(),
            "a diagnosis with no evidence cannot be checked"
        );
        assert!(!diagnosis.rule_id.as_str().is_empty());

        // Every cited observation must actually be retrievable.
        let host = EntityKey::new("lab", EntityType::Host, "n1").entity_id();
        let stored: Vec<_> = controller
            .store()
            .recent_observations(host, 64)
            .await
            .expect("observations")
            .into_iter()
            .map(|o| o.id)
            .collect();
        for cited in &diagnosis.evidence {
            assert!(stored.contains(cited), "cited observation {cited} is not stored");
        }
    }

    #[tokio::test]
    async fn diagnosis_is_reproducible() {
        let mut controller = controller().await;
        let view = view("NodeName=n1 State=IDLE+DRAIN Reason=maintenance\n");
        controller
            .ingest_snapshot(&snapshot_from_view("lab", "sched", &view))
            .await
            .expect("inventory");
        make_host_look_healthy(&mut controller, "n1").await;
        controller
            .ingest_observations(&crate::integrations::slurm::observe::observations_from_view(
                "lab", "sched", &view,
            ))
            .await
            .expect("observations");

        let first = controller.diagnose().await.expect("first");
        let second = controller.diagnose().await.expect("second");

        let describe = |d: &[Diagnosis]| {
            d.iter()
                .map(|d| (d.diagnosis_type.to_string(), d.summary.clone()))
                .collect::<Vec<_>>()
        };
        assert_eq!(describe(&first), describe(&second));
    }
}
