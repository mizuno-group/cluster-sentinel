//! Observing other hosts on the controller's behalf.
//!
//! An agent with `observer.peer` watches the targets the controller assigns it.
//! Those observations are what turn "I cannot reach it" from an ambiguity into
//! a diagnosis: several independent viewpoints agreeing means the host is gone,
//! and disagreeing means the path is (SPEC.md §46, §50, §51).
//!
//! The agent probes only what it is told to probe, only with compiled-in
//! probes, and only using parameters the controller resolved from inventory.
//! There is no path by which an assignment can make an agent run something
//! arbitrary (SPEC.md §116).

use std::sync::Arc;
use std::time::Duration;

use crate::capability::CapabilitySet;
use crate::config::ProbeSchedules;
use crate::entity::{EntityId, EntityType};
use crate::observation::Observation;
use crate::probes::{ExecutionMode, HasDefinition, Probe, ProbeContext, ProbeRunner, Skipped};
use crate::protocol::AssignedTarget;

/// Runs the remote probes an agent has been assigned.
pub struct PeerProbes {
    runner: ProbeRunner,
    probes: Vec<Arc<dyn Probe>>,
    observer: EntityId,
    targets: Vec<AssignedTarget>,
    revision: u64,
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

impl PeerProbes {
    /// A peer prober with the built-in remote probes.
    pub fn new(observer: EntityId) -> Self {
        Self::with_schedules(observer, &ProbeSchedules::default())
    }

    /// A peer prober with the built-in remote probes, retuned by the operator.
    ///
    /// The same overrides apply here as anywhere else: a site that slowed the
    /// reachability probe down did not mean "except when a peer runs it".
    pub fn with_schedules(observer: EntityId, schedules: &ProbeSchedules) -> Self {
        let mut probes: Vec<Arc<dyn Probe>> = Vec::new();
        add(&mut probes, schedules, crate::probes::network::TcpProbe::reachability());
        add(&mut probes, schedules, crate::probes::ssh::SshProbe::new());
        add(
            &mut probes,
            schedules,
            crate::probes::sentinel_rpc::SentinelAgentProbe::new(),
        );
        // The export port, from a peer rather than only from the controller.
        //
        // The controller ran this one alone, and it declines to probe its own
        // host -- rightly, for reachability: a host reporting that it answers
        // has established nothing. But that exclusion is total, so nothing
        // checked the NFS port on the controller's own machine, and the probe
        // audit found exactly that on a real cluster.
        //
        // A peer checking another host's 2049 is also better evidence than the
        // controller doing it alone: it is the same independent-viewpoint
        // argument the rest of peer monitoring rests on. Gated on the target's
        // capabilities like every probe here, so it runs only against hosts
        // that actually serve.
        add(&mut probes, schedules, crate::probes::nfs::NfsPortProbe::new());

        Self {
            runner: ProbeRunner::new(),
            probes,
            observer,
            targets: Vec::new(),
            revision: 0,
        }
    }

    /// Builder: replace the probe set.
    pub fn with_probes(mut self, probes: Vec<Arc<dyn Probe>>) -> Self {
        self.probes = probes;
        self
    }

    /// Accept a new assignment.
    ///
    /// Returns whether anything changed, so a caller can log a reassignment
    /// without logging every unchanged fetch.
    pub fn set_targets(&mut self, revision: u64, targets: Vec<AssignedTarget>) -> bool {
        let changed = revision != self.revision || targets != self.targets;
        self.revision = revision;
        self.targets = targets;
        changed
    }

    /// The assignment revision in force.
    pub fn revision(&self) -> u64 {
        self.revision
    }

    /// How many targets are assigned.
    /// The probes this peer runs, so a caller can inspect their schedules.
    pub fn probes(&self) -> &[Arc<dyn Probe>] {
        &self.probes
    }

    pub fn len(&self) -> usize {
        self.targets.len()
    }

    /// Whether nothing is assigned.
    pub fn is_empty(&self) -> bool {
        self.targets.is_empty()
    }

    /// The assigned target names.
    pub fn target_names(&self) -> Vec<&str> {
        self.targets.iter().map(|t| t.name.as_str()).collect()
    }

    /// Probe every assigned target.
    pub async fn observe_all(&self) -> Vec<Observation> {
        let mut observations = Vec::new();

        // Concurrently within a bounded batch: an unreachable target must not
        // delay the rest by its whole timeout, and a large assignment must not
        // open every socket at once.
        for chunk in self.targets.chunks(16) {
            let results = futures::future::join_all(chunk.iter().map(|target| self.observe(target))).await;
            observations.extend(results.into_iter().flatten());
        }
        observations
    }

    /// Probe one assigned target.
    pub async fn observe(&self, target: &AssignedTarget) -> Vec<Observation> {
        let Ok(entity) = target.entity_id.parse::<EntityId>() else {
            tracing::warn!(target = %target.name, "assignment carried an unparseable entity id");
            return Vec::new();
        };

        let mut observations = Vec::new();
        for probe in &self.probes {
            let definition = probe.definition();

            // The target's capabilities decide what runs, exactly as they do
            // on the controller. An observer does not get to probe things the
            // target does not have.
            if !definition.applies_to(EntityType::Host, &target.capabilities) {
                continue;
            }
            if definition.execution_mode == ExecutionMode::Local {
                continue;
            }

            let context = ProbeContext::local(entity, target.capabilities.clone())
                .observed_by(self.observer)
                .with_parameters(target.parameters.clone())
                .with_timeout(definition.timeout);

            match self.runner.run(Arc::clone(probe), context).await {
                Ok(observation) => observations.push(observation),
                Err(Skipped::AlreadyRunning) => {
                    tracing::debug!(probe = %definition.id, target = %target.name, "peer probe still outstanding");
                }
            }
        }

        observations
    }
}

/// Build an assigned target directly, for tests and for local wiring.
pub fn assigned_target(
    entity: EntityId,
    name: &str,
    address: &str,
    capabilities: CapabilitySet,
    parameters: serde_json::Value,
) -> AssignedTarget {
    AssignedTarget {
        entity_id: entity.to_string(),
        name: name.to_string(),
        address: address.to_string(),
        parameters,
        capabilities,
    }
}

/// How often an agent refreshes its assignment.
pub const ASSIGNMENT_REFRESH: Duration = Duration::from_secs(60);

#[cfg(test)]
mod tests {
    use super::*;
    use crate::entity::EntityKey;
    use crate::observation::ProbeStatus;

    fn host(name: &str) -> EntityId {
        EntityKey::new("lab", EntityType::Host, name).entity_id()
    }

    /// A target at a loopback address with nothing listening, so probes fail
    /// fast with a refusal instead of waiting out a DNS timeout.
    async fn dead_target(name: &str, capabilities: &[&'static str]) -> AssignedTarget {
        let port = {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("bind");
            listener.local_addr().expect("addr").port()
        };
        assigned_target(
            host(name),
            name,
            "127.0.0.1",
            capabilities.iter().copied().collect(),
            serde_json::json!({"address": "127.0.0.1", "port": port, "ssh_port": port, "agent_port": port}),
        )
    }

    #[tokio::test]
    async fn an_agent_with_no_assignment_observes_nothing() {
        let peers = PeerProbes::new(host("watcher"));
        assert!(peers.is_empty());
        assert!(peers.observe_all().await.is_empty());
    }

    #[tokio::test]
    async fn every_observation_is_attributed_to_this_observer() {
        // Without this, quorum cannot tell one viewpoint from another.
        let mut peers = PeerProbes::new(host("watcher"));
        peers.set_targets(1, vec![dead_target("target", &["ssh.server"]).await]);

        let observations = peers.observe_all().await;
        assert!(!observations.is_empty());
        for observation in &observations {
            assert_eq!(observation.observer_entity, Some(host("watcher")));
            assert!(observation.is_remote());
            assert_eq!(observation.target_entity, host("target"));
        }
    }

    #[tokio::test]
    async fn only_probes_the_target_has_the_capability_for_are_run() {
        let mut peers = PeerProbes::new(host("watcher"));
        peers.set_targets(1, vec![dead_target("target", &["ssh.server"]).await]);

        let probes: Vec<String> = peers
            .observe_all()
            .await
            .into_iter()
            .map(|o| o.probe_id.to_string())
            .collect();
        assert!(probes.contains(&crate::probes::ssh::PROBE_ID.to_string()));
        assert!(
            !probes.contains(&crate::probes::sentinel_rpc::PROBE_ID.to_string()),
            "the target has no agent capability"
        );
    }

    #[tokio::test]
    async fn a_peer_checks_a_fileservers_export_port() {
        // Found by the probe audit on a real cluster: nothing was checking the
        // NFS port on the controller's own host, because the controller was
        // the only thing that ran that probe and it declines to probe itself.
        // A peer is both the fix and the better evidence -- the same
        // independent-viewpoint argument the rest of peer monitoring rests on.
        let target = dead_target("fs1", &["storage.nfs.server"]).await;
        let mut peers = PeerProbes::new(host("observer"));
        peers.set_targets(1, vec![target]);

        let observations = peers.observe_all().await;
        assert!(
            observations
                .iter()
                .any(|o| o.probe_id.as_str() == crate::probes::nfs::PROBE_SERVER_PORT),
            "{:?}",
            observations
                .iter()
                .map(|o| o.probe_id.as_str().to_string())
                .collect::<Vec<_>>()
        );
    }

    #[tokio::test]
    async fn a_host_that_serves_no_storage_is_not_asked_about_its_export_port() {
        // Gated on the target's capabilities like everything else here, so
        // adding it does not start knocking on 2049 across the whole cluster.
        let target = dead_target("node01", &["ssh.server"]).await;
        let mut peers = PeerProbes::new(host("observer"));
        peers.set_targets(1, vec![target]);

        let observations = peers.observe_all().await;
        assert!(
            !observations
                .iter()
                .any(|o| o.probe_id.as_str() == crate::probes::nfs::PROBE_SERVER_PORT),
            "{:?}",
            observations
                .iter()
                .map(|o| o.probe_id.as_str().to_string())
                .collect::<Vec<_>>()
        );
    }

    #[tokio::test]
    async fn a_target_with_no_capabilities_is_still_checked_for_reachability() {
        // Reachability needs nothing installed on the far end, and a peer that
        // only watched hosts already known to be watchable would be no use for
        // the hosts that most need watching.
        let mut peers = PeerProbes::new(host("watcher"));
        peers.set_targets(1, vec![dead_target("target", &[]).await]);

        let probes: Vec<String> = peers
            .observe_all()
            .await
            .iter()
            .map(|o| o.probe_id.to_string())
            .collect();

        assert_eq!(probes, vec![crate::probes::network::PROBE_ID.to_string()]);
    }

    #[tokio::test]
    async fn several_targets_are_all_observed() {
        let mut peers = PeerProbes::new(host("watcher"));
        peers.set_targets(
            1,
            vec![
                dead_target("t1", &["ssh.server"]).await,
                dead_target("t2", &["ssh.server"]).await,
            ],
        );

        let observations = peers.observe_all().await;
        let targets: std::collections::BTreeSet<_> = observations.iter().map(|o| o.target_entity).collect();
        assert_eq!(targets.len(), 2);
    }

    #[tokio::test]
    async fn a_failed_probe_is_recorded_rather_than_dropped() {
        // The observation that a peer could not be reached is the point.
        let mut peers = PeerProbes::new(host("watcher"));
        peers.set_targets(1, vec![dead_target("target", &["ssh.server"]).await]);

        let observations = peers.observe_all().await;
        assert!(observations.iter().any(|o| o.status == ProbeStatus::Failed));
    }

    #[test]
    fn a_changed_assignment_is_reported_as_changed() {
        let mut peers = PeerProbes::new(host("watcher"));
        assert!(peers.set_targets(1, vec![]), "the first assignment is a change");
        assert!(!peers.set_targets(1, vec![]), "an identical refetch is not");
        assert!(peers.set_targets(2, vec![]), "a new revision is");
        assert_eq!(peers.revision(), 2);
    }

    #[tokio::test]
    async fn an_unparseable_entity_id_is_skipped_rather_than_panicking() {
        let mut peers = PeerProbes::new(host("watcher"));
        peers.set_targets(
            1,
            vec![AssignedTarget {
                entity_id: "not-a-uuid".into(),
                name: "broken".into(),
                address: "127.0.0.1".into(),
                parameters: serde_json::json!({"address": "127.0.0.1"}),
                capabilities: ["ssh.server"].into_iter().collect(),
            }],
        );
        assert!(peers.observe_all().await.is_empty());
    }
}
