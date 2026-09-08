//! Turning an agent registration into inventory.
//!
//! Agent registration is an inventory provider like any other (SPEC.md §32): it
//! is how hosts outside Slurm — fileservers, login nodes, anything — become
//! monitored entities without an operator listing them by hand.

use crate::capability::{well_known, Capability};
use crate::entity::{DiscoverySource, EntityId, EntityKey, EntityType, ManagedEntity};
use crate::inventory::InventorySnapshot;
use crate::observation::{Observation, ProbeStatus};
use crate::persistence::StoreError;
use crate::probes::ProbeId;
use crate::protocol::{ObservationBatch, ObservationBatchResponse, RegisterRequest};
use crate::PROTOCOL_VERSION;

use super::Controller;

/// Probe id under which a reboot is recorded.
pub const PROBE_BOOT: &str = "host.boot";

/// What a registration resulted in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Registration {
    /// The host entity the agent reports for.
    pub entity_id: EntityId,
    /// Whether the host had rebooted since the previous registration.
    pub rebooted: bool,
}

/// Build the inventory snapshot a registration implies.
///
/// Pure, so the mapping is testable without a controller or a database.
pub fn snapshot_from_registration(request: &RegisterRequest) -> InventorySnapshot {
    let mut snapshot = InventorySnapshot::new(DiscoverySource::AgentRegistration);

    let mut entity = ManagedEntity::new(&request.environment, EntityType::Host, &request.hostname);
    for capability in request.capabilities.iter() {
        entity.capabilities.insert(capability.clone());
    }
    // An agent that answered is, by definition, an agent that is running.
    entity.capabilities.insert(Capability::new(well_known::SENTINEL_AGENT));

    // Roles are recorded as labels, never as capabilities: they must not be
    // able to switch a probe on (SPEC.md §15).
    for role in &request.roles {
        entity.labels.insert(format!("role.{role}"), "true".to_string());
    }

    entity.metadata = serde_json::json!({
        "agent": {
            "version": request.agent_version,
            "protocol_version": request.protocol_version,
        },
        "host": {
            "fqdn": request.fqdn,
            "boot_id": request.boot_id,
            "addresses": request.addresses,
        },
        // Where to reach this host's services. Reported rather than assumed:
        // probing the wrong port reports a service down that is running fine.
        "ports": request.ports,
        "hardware": request.hardware,
    });

    snapshot.add_entity(entity);
    snapshot
}

impl Controller {
    /// Record an agent registration.
    pub async fn register_agent(&mut self, request: &RegisterRequest) -> Result<Registration, StoreError> {
        let entity_id = EntityKey::new(&request.environment, EntityType::Host, &request.hostname).entity_id();
        let previous_boot_id = self.stored_boot_id(entity_id).await?;

        let snapshot = snapshot_from_registration(request);
        self.ingest_snapshot(&snapshot).await?;

        // A changed boot id is the only reliable evidence of a reboot, and it
        // is recorded as an observation so the incident timeline can show it.
        let rebooted = match (&previous_boot_id, &request.boot_id) {
            (Some(before), Some(after)) => before != after,
            _ => false,
        };

        if rebooted {
            let observation = Observation::new(ProbeId::new(PROBE_BOOT), entity_id, ProbeStatus::Ok).with_payload(
                serde_json::json!({
                    "event": "host_rebooted",
                    "previous_boot_id": previous_boot_id,
                    "boot_id": request.boot_id,
                }),
            );
            self.ingest_observations(&[observation]).await?;
        }

        Ok(Registration { entity_id, rebooted })
    }

    /// The boot id recorded for an entity, if any.
    async fn stored_boot_id(&self, entity_id: EntityId) -> Result<Option<String>, StoreError> {
        let environment = self.config().environment.clone();
        Ok(self
            .store()
            .load_entities(&environment)
            .await?
            .into_iter()
            .find(|e| e.id == entity_id)
            .and_then(|e| e.metadata.get("host")?.get("boot_id")?.as_str().map(str::to_string)))
    }

    /// Ingest a batch of observations from an agent.
    pub async fn ingest_agent_batch(
        &mut self,
        batch: &ObservationBatch,
    ) -> Result<ObservationBatchResponse, StoreError> {
        // Observations about entities the controller has never heard of are
        // rejected rather than silently dropped: an agent reporting for a
        // phantom is a configuration error worth surfacing.
        let environment = self.config().environment.clone();
        let known: std::collections::HashSet<EntityId> = self
            .store()
            .load_entities(&environment)
            .await?
            .into_iter()
            .map(|e| e.id)
            .collect();

        let (accepted, rejected): (Vec<_>, Vec<_>) = batch
            .observations
            .iter()
            .cloned()
            .partition(|o| known.contains(&o.target_entity));

        let outcome = self.store().ingest_observations(&accepted).await?;
        let transitions = self.engine_mut().ingest_all(&accepted);
        for transition in &transitions {
            self.store().save_state_transition(transition).await?;
        }
        for state in self.engine().states() {
            self.store().save_entity_state(state).await?;
        }

        Ok(ObservationBatchResponse {
            protocol_version: PROTOCOL_VERSION,
            accepted: outcome.inserted,
            duplicates: outcome.duplicates,
            rejected: rejected
                .into_iter()
                .map(|o| crate::protocol::RejectedObservation {
                    id: o.id.to_string(),
                    reason: format!("unknown target entity {}", o.target_entity),
                })
                .collect(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::capability::CapabilitySet;
    use crate::config::Config;
    use crate::persistence::SqliteStore;

    fn registration(hostname: &str, boot_id: Option<&str>) -> RegisterRequest {
        let mut request = RegisterRequest::new(
            "lab",
            hostname,
            CapabilitySet::from_iter(["host.metrics", "ssh.server", "storage.nfs.server"]),
        );
        request.boot_id = boot_id.map(str::to_string);
        request.addresses = vec!["192.0.2.10".into(), "198.51.100.10".into()];
        request.hardware = serde_json::json!({"cpus": 64, "memory_mb": 257000, "gpus": 2});
        request
    }

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

    #[test]
    fn a_registration_becomes_a_host_entity_with_its_capabilities() {
        let snapshot = snapshot_from_registration(&registration("node-a", Some("boot-1")));
        assert_eq!(snapshot.entities.len(), 1);

        let entity = &snapshot.entities[0];
        assert_eq!(entity.entity_type, EntityType::Host);
        assert_eq!(entity.canonical_name, "node-a");
        assert!(entity.capabilities.has("storage.nfs.server"));
        assert!(
            entity.capabilities.has(well_known::SENTINEL_AGENT),
            "a registering agent is a running agent"
        );
        assert_eq!(entity.discovery_sources, vec![DiscoverySource::AgentRegistration]);
    }

    #[test]
    fn addresses_are_metadata_and_do_not_change_identity() {
        let with_addresses = snapshot_from_registration(&registration("node-a", None));
        let mut without = registration("node-a", None);
        without.addresses.clear();
        let without_addresses = snapshot_from_registration(&without);

        assert_eq!(with_addresses.entities[0].id, without_addresses.entities[0].id);
        assert_eq!(
            with_addresses.entities[0].metadata["host"]["addresses"][0],
            "192.0.2.10"
        );
    }

    #[test]
    fn reported_ports_reach_the_entity_so_peers_probe_the_right_place() {
        let mut request = registration("node-a", None);
        request.ports = std::collections::BTreeMap::from([("ssh".to_string(), 2222u16)]);

        let entity = &snapshot_from_registration(&request).entities[0];
        assert_eq!(entity.metadata["ports"]["ssh"], 2222);

        // And the endpoint resolution actually uses them.
        let endpoint = crate::controller::endpoint_for(entity).expect("endpoint");
        assert_eq!(endpoint.parameters()["ssh_port"], 2222);
    }

    #[test]
    fn ports_do_not_affect_identity() {
        let mut with_ports = registration("node-a", None);
        with_ports.ports = std::collections::BTreeMap::from([("ssh".to_string(), 2222u16)]);
        let without_ports = registration("node-a", None);

        assert_eq!(
            snapshot_from_registration(&with_ports).entities[0].id,
            snapshot_from_registration(&without_ports).entities[0].id
        );
    }

    #[test]
    fn a_role_becomes_a_label_and_never_a_capability() {
        // SPEC.md §15: a role must not be able to switch a probe on.
        let mut request = RegisterRequest::new("lab", "node-a", CapabilitySet::new());
        request.roles = vec!["fileserver".into()];

        let entity = &snapshot_from_registration(&request).entities[0];
        assert_eq!(entity.labels.get("role.fileserver").map(String::as_str), Some("true"));
        assert!(!entity.capabilities.has("fileserver"));
        assert!(!entity.capabilities.has("storage.nfs.server"));
    }

    #[test]
    fn hardware_is_preserved_for_later_comparison_against_the_scheduler() {
        let entity = &snapshot_from_registration(&registration("node-a", None)).entities[0];
        assert_eq!(entity.metadata["hardware"]["gpus"], 2);
    }

    #[tokio::test]
    async fn a_first_registration_is_not_a_reboot() {
        let mut controller = controller().await;
        let outcome = controller
            .register_agent(&registration("node-a", Some("boot-1")))
            .await
            .expect("register");
        assert!(!outcome.rebooted, "arriving for the first time is not rebooting");
    }

    #[tokio::test]
    async fn a_changed_boot_id_records_a_reboot_observation() {
        // SPEC.md §107: the reason reboots must be detected is that they erase
        // the evidence of whatever caused them.
        let mut controller = controller().await;
        controller
            .register_agent(&registration("node-a", Some("boot-1")))
            .await
            .expect("first");
        let outcome = controller
            .register_agent(&registration("node-a", Some("boot-2")))
            .await
            .expect("second");

        assert!(outcome.rebooted);
        let observations = controller
            .store()
            .recent_observations(outcome.entity_id, 10)
            .await
            .expect("observations");
        let reboot = observations
            .iter()
            .find(|o| o.probe_id.as_str() == PROBE_BOOT)
            .expect("reboot recorded");
        assert_eq!(reboot.payload["previous_boot_id"], "boot-1");
        assert_eq!(reboot.payload["boot_id"], "boot-2");
    }

    #[tokio::test]
    async fn an_agent_restart_with_the_same_boot_id_is_not_a_reboot() {
        let mut controller = controller().await;
        controller
            .register_agent(&registration("node-a", Some("boot-1")))
            .await
            .expect("first");
        let outcome = controller
            .register_agent(&registration("node-a", Some("boot-1")))
            .await
            .expect("second");
        assert!(!outcome.rebooted);
    }

    #[tokio::test]
    async fn a_missing_boot_id_never_produces_a_false_reboot() {
        let mut controller = controller().await;
        controller
            .register_agent(&registration("node-a", None))
            .await
            .expect("first");
        let outcome = controller
            .register_agent(&registration("node-a", None))
            .await
            .expect("second");
        assert!(!outcome.rebooted);
    }

    #[tokio::test]
    async fn a_batch_for_an_unknown_entity_is_rejected_rather_than_silently_dropped() {
        let mut controller = controller().await;
        let registered = controller
            .register_agent(&registration("node-a", Some("boot-1")))
            .await
            .expect("register");

        let phantom = EntityKey::new("lab", EntityType::Host, "never-registered").entity_id();
        let batch = ObservationBatch {
            protocol_version: PROTOCOL_VERSION,
            agent_id: uuid::Uuid::new_v4(),
            session_id: uuid::Uuid::new_v4(),
            observations: vec![
                Observation::new(ProbeId::new("host.metrics"), registered.entity_id, ProbeStatus::Ok),
                Observation::new(ProbeId::new("host.metrics"), phantom, ProbeStatus::Ok),
            ],
        };

        let response = controller.ingest_agent_batch(&batch).await.expect("ingest");
        assert_eq!(response.accepted, 1);
        assert_eq!(response.rejected.len(), 1);
        assert!(response.rejected[0].reason.contains("unknown target entity"));
    }

    #[tokio::test]
    async fn registering_twice_does_not_duplicate_the_host() {
        let mut controller = controller().await;
        controller
            .register_agent(&registration("node-a", Some("boot-1")))
            .await
            .expect("first");
        controller
            .register_agent(&registration("node-a", Some("boot-1")))
            .await
            .expect("second");

        let entities = controller.store().load_entities("lab").await.expect("entities");
        assert_eq!(entities.iter().filter(|e| e.canonical_name == "node-a").count(), 1);
    }
}
