//! The controller as an observer.
//!
//! The controller runs the remote probes — reachability, SSH, agent health —
//! against every host it knows an address for. In M4 it is the *only*
//! observer, which is precisely why its conclusions are limited: one viewpoint
//! cannot tell a dead host from a broken path to it. Peer monitoring adds the
//! other viewpoints, and the diagnosis engine is what combines them
//! (SPEC.md §50, §51).

use std::sync::Arc;

use crate::capability::CapabilitySet;
use crate::config::ProbeSchedules;
use crate::entity::{EntityId, EntityType, ManagedEntity};
use crate::observation::Observation;
use crate::probes::{ExecutionMode, HasDefinition, Probe, ProbeContext, ProbeRunner, Skipped};

/// Where to reach an entity, and on which ports.
#[derive(Debug, Clone, PartialEq)]
pub struct Endpoint {
    /// The address to connect to.
    pub address: String,
    /// SSH port, if it differs from the default.
    pub ssh_port: Option<u16>,
    /// Sentinel agent port, if it differs from the default.
    pub agent_port: Option<u16>,
}

impl Endpoint {
    /// The probe parameters this endpoint implies.
    pub fn parameters(&self) -> serde_json::Value {
        let mut parameters = serde_json::json!({ "address": self.address });
        if let Some(port) = self.ssh_port {
            parameters["ssh_port"] = port.into();
        }
        if let Some(port) = self.agent_port {
            parameters["agent_port"] = port.into();
        }
        parameters
    }
}

/// Work out where to reach an entity.
///
/// Explicit addresses win; otherwise the canonical name is tried, because in
/// practice a cluster's hosts resolve by name. Returning `None` rather than
/// guessing matters: a probe with no address reports `NotApplicable`, and "we
/// do not know where this is" must never read as "this is broken".
pub fn endpoint_for(entity: &ManagedEntity) -> Option<Endpoint> {
    let addresses = entity
        .metadata
        .get("host")
        .and_then(|host| host.get("addresses"))
        .or_else(|| entity.metadata.get("addresses"))
        .and_then(|value| value.as_array());

    let address = addresses
        .and_then(|list| list.iter().find_map(|value| value.as_str()))
        .map(str::to_string)
        // A host that has never registered still has a name, and a name is
        // usually resolvable. This is a fallback, not an assumption: if it does
        // not resolve, the probe says so.
        .unwrap_or_else(|| entity.canonical_name.clone());

    if address.is_empty() {
        return None;
    }

    let port = |key: &str| {
        entity
            .metadata
            .get("ports")
            .and_then(|p| p.get(key))
            .and_then(|v| v.as_u64())
            .map(|v| v as u16)
    };

    Some(Endpoint {
        address,
        ssh_port: port("ssh"),
        agent_port: port("agent"),
    })
}

/// Add a probe unless the operator has switched it off, applying their schedule.
fn add<P>(probes: &mut Vec<Arc<dyn Probe>>, schedules: &ProbeSchedules, mut probe: P)
where
    P: Probe + HasDefinition + 'static,
{
    if !schedules.is_enabled(probe.definition().id.as_str()) {
        return;
    }
    schedules.apply(probe.definition_mut());
    probes.push(Arc::new(probe));
}

/// Runs remote probes against the entities the controller knows about.
pub struct RemoteObserver {
    runner: ProbeRunner,
    probes: Vec<Arc<dyn Probe>>,
    observer_entity: Option<EntityId>,
}

impl RemoteObserver {
    /// An observer with the built-in remote probes.
    pub fn new() -> Self {
        Self::with_schedules(&ProbeSchedules::default())
    }

    /// An observer with the built-in remote probes, retuned by the operator.
    ///
    /// The definitions are changed rather than wrapped, so that a probe which
    /// bounds its own work by its timeout sees the same value the runner does.
    pub fn with_schedules(schedules: &ProbeSchedules) -> Self {
        let mut probes: Vec<Arc<dyn Probe>> = Vec::new();
        add(&mut probes, schedules, crate::probes::network::TcpProbe::reachability());
        add(&mut probes, schedules, crate::probes::ssh::SshProbe::new());
        add(
            &mut probes,
            schedules,
            crate::probes::sentinel_rpc::SentinelAgentProbe::new(),
        );
        add(&mut probes, schedules, crate::probes::nfs::NfsPortProbe::new());

        Self {
            runner: ProbeRunner::new(),
            probes,
            observer_entity: None,
        }
    }

    /// Builder: record which entity is doing the observing.
    ///
    /// Quorum logic needs to know who saw what, so an observation made from
    /// here is attributed just like one made from a peer.
    pub fn observed_by(mut self, observer: EntityId) -> Self {
        self.observer_entity = Some(observer);
        self
    }

    /// Builder: replace the probe set.
    pub fn with_probes(mut self, probes: Vec<Arc<dyn Probe>>) -> Self {
        self.probes = probes;
        self
    }

    /// The probes this observer runs.
    pub fn probes(&self) -> &[Arc<dyn Probe>] {
        &self.probes
    }

    /// Probe one entity with every applicable probe.
    pub async fn observe(&self, entity: &ManagedEntity) -> Vec<Observation> {
        let Some(endpoint) = endpoint_for(entity) else {
            return Vec::new();
        };

        let mut observations = Vec::new();
        for probe in &self.probes {
            let definition = probe.definition();

            // Capability decides what runs here, never the entity's role or
            // name (SPEC.md §15).
            if !definition.applies_to(entity.entity_type, &entity.capabilities) {
                continue;
            }
            if definition.execution_mode == ExecutionMode::Local {
                continue;
            }

            let mut context = ProbeContext::local(entity.id, entity.capabilities.clone())
                .with_parameters(endpoint.parameters())
                .with_timeout(definition.timeout);
            if let Some(observer) = self.observer_entity {
                context = context.observed_by(observer);
            }

            match self.runner.run(Arc::clone(probe), context).await {
                Ok(observation) => observations.push(observation),
                Err(Skipped::AlreadyRunning) => {
                    tracing::debug!(probe = %definition.id, entity = %entity.canonical_name, "probe still outstanding");
                }
            }
        }

        observations
    }

    /// Probe every host in a collection, concurrently.
    pub async fn observe_all<'a>(&self, entities: impl IntoIterator<Item = &'a ManagedEntity>) -> Vec<Observation> {
        let hosts: Vec<&ManagedEntity> = entities
            .into_iter()
            .filter(|e| e.entity_type == EntityType::Host)
            .filter(|e| e.lifecycle_state == crate::entity::LifecycleState::Active)
            .collect();

        // Concurrently, so one unreachable host does not delay the rest by its
        // whole timeout: with a serial loop, a dozen dead hosts at a three
        // second timeout would push the cycle past its own interval.
        //
        // Bounded batches rather than one giant join, so a large cluster does
        // not open thousands of sockets at once.
        let mut observations = Vec::new();
        for chunk in hosts.chunks(32) {
            let results = futures::future::join_all(chunk.iter().map(|entity| self.observe(entity))).await;
            observations.extend(results.into_iter().flatten());
        }
        observations
    }
}

impl Default for RemoteObserver {
    fn default() -> Self {
        Self::new()
    }
}

/// Applicable probes for an entity, for `sentinel doctor` and tests.
pub fn applicable_probes(
    probes: &[Arc<dyn Probe>],
    entity_type: EntityType,
    capabilities: &CapabilitySet,
) -> Vec<String> {
    probes
        .iter()
        .filter(|p| p.definition().applies_to(entity_type, capabilities))
        .map(|p| p.definition().id.to_string())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::entity::EntityType;

    fn host(name: &str) -> ManagedEntity {
        ManagedEntity::new("lab", EntityType::Host, name)
    }

    #[test]
    fn a_registered_address_is_preferred() {
        let mut entity = host("node-a");
        entity.metadata = serde_json::json!({"host": {"addresses": ["192.0.2.10", "198.51.100.10"]}});

        let endpoint = endpoint_for(&entity).expect("endpoint");
        assert_eq!(endpoint.address, "192.0.2.10");
    }

    #[test]
    fn a_configured_address_is_used_when_there_is_no_registration() {
        let mut entity = host("node-a");
        entity.metadata = serde_json::json!({"addresses": ["192.0.2.20"]});
        assert_eq!(endpoint_for(&entity).expect("endpoint").address, "192.0.2.20");
    }

    #[test]
    fn the_canonical_name_is_the_fallback() {
        // In practice a cluster's hosts resolve by name; if this one does not,
        // the probe will say so rather than the resolver guessing.
        assert_eq!(endpoint_for(&host("node-a")).expect("endpoint").address, "node-a");
    }

    #[test]
    fn non_default_ports_are_carried_into_the_parameters() {
        let mut entity = host("node-a");
        entity.metadata = serde_json::json!({"ports": {"ssh": 2222, "agent": 9444}});

        let parameters = endpoint_for(&entity).expect("endpoint").parameters();
        assert_eq!(parameters["ssh_port"], 2222);
        assert_eq!(parameters["agent_port"], 9444);
        assert_eq!(parameters["address"], "node-a");
    }

    #[test]
    fn default_ports_are_left_for_the_probe_to_choose() {
        let parameters = endpoint_for(&host("node-a")).expect("endpoint").parameters();
        assert!(parameters.get("ssh_port").is_none());
        assert!(parameters.get("agent_port").is_none());
    }

    /// A host at a loopback address with nothing listening.
    ///
    /// Probes then fail immediately with a refusal rather than waiting out a
    /// DNS or connect timeout, which keeps these tests fast without weakening
    /// what they assert.
    async fn unreachable_host(name: &str, capabilities: &[&'static str]) -> ManagedEntity {
        let port = {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("bind");
            listener.local_addr().expect("addr").port()
        };
        let mut entity = host(name).with_capabilities(capabilities.iter().copied().collect::<CapabilitySet>());
        entity.metadata = serde_json::json!({
            "host": { "addresses": ["127.0.0.1"] },
            "ports": { "ssh": port, "agent": port },
        });
        entity
    }

    #[tokio::test]
    async fn only_probes_the_entity_has_the_capability_for_are_run() {
        // SPEC.md §15: a host without an agent must not be probed for one.
        let observer = RemoteObserver::new();

        let bare = unreachable_host("node-a", &[]).await;
        assert!(observer.observe(&bare).await.is_empty(), "no capabilities, no probes");

        let with_ssh = unreachable_host("node-b", &["ssh.server"]).await;
        let observations = observer.observe(&with_ssh).await;
        assert_eq!(observations.len(), 1);
        assert_eq!(observations[0].probe_id.as_str(), crate::probes::ssh::PROBE_ID);
    }

    #[tokio::test]
    async fn observations_are_attributed_to_the_observer() {
        let controller = ManagedEntity::new("lab", EntityType::Host, "controller").id;
        let observer = RemoteObserver::new().observed_by(controller);

        let entity = unreachable_host("node-a", &["ssh.server"]).await;
        let observations = observer.observe(&entity).await;

        assert_eq!(observations.len(), 1);
        assert_eq!(observations[0].observer_entity, Some(controller));
        assert!(observations[0].is_remote(), "quorum logic needs to know who saw this");
    }

    #[tokio::test]
    async fn every_applicable_probe_runs_against_a_fully_capable_host() {
        let observer = RemoteObserver::new();
        let entity = unreachable_host("node-a", &["ssh.server", "sentinel.agent", "network.tcp"]).await;

        let observations = observer.observe(&entity).await;
        let probes: Vec<&str> = observations.iter().map(|o| o.probe_id.as_str()).collect();

        assert!(probes.contains(&crate::probes::network::PROBE_ID));
        assert!(probes.contains(&crate::probes::ssh::PROBE_ID));
        assert!(probes.contains(&crate::probes::sentinel_rpc::PROBE_ID));
    }

    #[tokio::test]
    async fn a_stale_entity_is_not_probed() {
        // It is no longer reported by anything; probing it would produce
        // failures that mean nothing.
        let observer = RemoteObserver::new();
        let mut entity = unreachable_host("node-a", &["ssh.server"]).await;
        entity.lifecycle_state = crate::entity::LifecycleState::Stale;

        assert!(observer.observe_all(std::iter::once(&entity)).await.is_empty());
    }

    #[tokio::test]
    async fn only_hosts_are_probed_remotely() {
        // A storage domain has no address to connect to; probing it would be
        // probing whatever its name happens to resolve to.
        let observer = RemoteObserver::new();
        let mut storage = ManagedEntity::new("lab", EntityType::Storage, "shared-a")
            .with_capabilities(CapabilitySet::from_iter(["ssh.server"]));
        storage.metadata = serde_json::json!({"host": {"addresses": ["127.0.0.1"]}});

        assert!(observer.observe_all(std::iter::once(&storage)).await.is_empty());
    }

    #[test]
    fn applicable_probes_are_reported_for_diagnosis() {
        let observer = RemoteObserver::new();
        let capabilities = CapabilitySet::from_iter(["ssh.server", "sentinel.agent", "network.tcp"]);
        let applicable = applicable_probes(observer.probes(), EntityType::Host, &capabilities);

        assert!(applicable.contains(&crate::probes::ssh::PROBE_ID.to_string()));
        assert!(applicable.contains(&crate::probes::sentinel_rpc::PROBE_ID.to_string()));
        assert!(applicable.contains(&crate::probes::network::PROBE_ID.to_string()));
    }
}
