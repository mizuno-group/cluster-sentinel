//! Inventory from Slurm (SPEC.md §31).
//!
//! Slurm contributes compute nodes, a scheduler entity, and the services that
//! provide it. It does **not** define the inventory: a host Slurm has never
//! heard of is monitored exactly as well, and a Slurm outage removes nothing.
//!
//! The Slurm `NodeName` and the host name are kept as separate facts
//! (SPEC.md §38). The host entity is keyed on the host name; the Slurm identity
//! is stored as metadata, so a deployment where the two differ works without
//! any core change.

use async_trait::async_trait;

use crate::capability::{well_known, CapabilitySet};
use crate::command::Allowlist;
use crate::dependency::{Criticality, DependencyEdge, DependencyType};
use crate::entity::{DiscoverySource, EntityKey, EntityType, ManagedEntity};
use crate::integrations::slurm::parser::{self, ControllerPing, SlurmNode, SlurmPartition};
use crate::integrations::slurm::ScontrolClient;

use super::{InventoryError, InventoryProvider, InventorySnapshot};

/// Name of this provider, as it appears in `discovery_source`.
pub const PROVIDER: &str = "slurm";

/// What one Slurm discovery cycle saw, before it is turned into entities.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct SlurmView {
    /// Nodes from `scontrol show nodes -o`.
    pub nodes: Vec<SlurmNode>,
    /// Partitions from `scontrol show partitions -o`.
    pub partitions: Vec<SlurmPartition>,
    /// Controllers from `scontrol ping`.
    pub controllers: Vec<ControllerPing>,
}

/// Turn a Slurm view into entities and dependency edges.
///
/// Pure, so the whole mapping is testable from a fixture without a cluster.
pub fn snapshot_from_view(environment: &str, scheduler_name: &str, view: &SlurmView) -> InventorySnapshot {
    let mut snapshot = InventorySnapshot::new(DiscoverySource::Integration(PROVIDER.to_string()));

    let scheduler_id = EntityKey::new(environment, EntityType::Scheduler, scheduler_name).entity_id();
    let mut scheduler = ManagedEntity::new(environment, EntityType::Scheduler, scheduler_name);
    scheduler.metadata = serde_json::json!({
        "slurm": {
            "partitions": view.partitions.iter().map(|p| p.name.clone()).collect::<Vec<_>>(),
            "node_count": view.nodes.len(),
        }
    });
    snapshot.add_entity(scheduler);

    for node in &view.nodes {
        let host_name = node.host_name();
        let host_id = EntityKey::new(environment, EntityType::Host, host_name).entity_id();

        let mut host = ManagedEntity::new(environment, EntityType::Host, host_name)
            .with_capabilities(CapabilitySet::from_iter([well_known::SLURM_COMPUTE]));
        host.metadata = serde_json::json!({ "slurm": slurm_node_metadata(node) });
        snapshot.add_entity(host);

        // The daemon is its own entity, so "the host is fine but slurmd is
        // dead" is representable rather than a special case (SPEC.md §10).
        let service_name = format!("slurmd@{host_name}");
        let service_id = EntityKey::new(environment, EntityType::Service, &service_name).entity_id();
        let mut service = ManagedEntity::new(environment, EntityType::Service, &service_name);
        service.metadata = serde_json::json!({
            "unit": "slurmd.service",
            "slurm": { "node_name": node.node_name },
        });
        snapshot.add_entity(service);

        snapshot.add_dependency(DependencyEdge::new(service_id, host_id, DependencyType::HostedOn));
        snapshot.add_dependency(
            DependencyEdge::new(host_id, scheduler_id, DependencyType::UsesScheduler)
                // Losing the scheduler does not make the machine unhealthy; it
                // makes it unusable for work. That is a degradation, not an
                // outage of the host.
                .with_criticality(Criticality::Important),
        );
    }

    for controller in &view.controllers {
        let host_id = EntityKey::new(environment, EntityType::Host, &controller.host).entity_id();
        let host = ManagedEntity::new(environment, EntityType::Host, &controller.host)
            .with_capabilities(CapabilitySet::from_iter([well_known::SLURM_CONTROLLER]));
        snapshot.add_entity(host);

        let service_name = format!("slurmctld@{}", controller.host);
        let service_id = EntityKey::new(environment, EntityType::Service, &service_name).entity_id();
        let mut service = ManagedEntity::new(environment, EntityType::Service, &service_name);
        service.metadata = serde_json::json!({
            "unit": "slurmctld.service",
            "slurm": { "role": controller.role, "reachable": controller.up },
        });
        snapshot.add_entity(service);

        snapshot.add_dependency(DependencyEdge::new(service_id, host_id, DependencyType::HostedOn));
        // The scheduler is provided by its controller daemon: scheduler
        // depends on the service.
        snapshot.add_dependency(DependencyEdge::new(scheduler_id, service_id, DependencyType::Provides));
    }

    snapshot
}

/// The Slurm-side identity and state of a node, preserved verbatim
/// (SPEC.md §66).
fn slurm_node_metadata(node: &SlurmNode) -> serde_json::Value {
    serde_json::json!({
        "node_name": node.node_name,
        "node_host_name": node.node_host_name,
        "node_addr": node.node_addr,
        "state": node.state.raw,
        "state_base": node.state.base,
        "state_flags": node.state.flags,
        "reason": node.reason,
        "reason_time": node.reason_time,
        "partitions": node.partitions,
        "cfg_tres": node.cfg_tres,
        "alloc_tres": node.alloc_tres,
        "cpu_total": node.cpu_total,
        "real_memory_mb": node.real_memory_mb,
        "gres": node.gres,
        "configured_gpu_count": node.configured_gpu_count(),
        "boot_time": node.boot_time,
        "schedulable": node.state.is_schedulable(),
    })
}

/// Discovers inventory by running `scontrol`.
#[derive(Debug, Clone)]
pub struct SlurmInventoryProvider {
    environment: String,
    scheduler_name: String,
    client: ScontrolClient,
    allowlist: Allowlist,
}

impl SlurmInventoryProvider {
    /// Build a provider.
    ///
    /// `scheduler_name` is deployment data supplied by the caller, never a
    /// constant in this file (IMPLEMENTATION.md §101).
    pub fn new(environment: impl Into<String>, scheduler_name: impl Into<String>, client: ScontrolClient) -> Self {
        Self {
            environment: environment.into(),
            scheduler_name: scheduler_name.into(),
            client,
            allowlist: Allowlist::builtin(),
        }
    }

    /// Run the three `scontrol` queries and parse them.
    ///
    /// A failure of the node query is fatal to the cycle; failures of the
    /// partition and ping queries are not, because partial inventory beats no
    /// inventory.
    pub async fn collect_view(&self) -> Result<SlurmView, InventoryError> {
        let nodes_output =
            self.client
                .show_nodes(&self.allowlist)
                .await
                .map_err(|error| InventoryError::Unavailable {
                    provider: PROVIDER.into(),
                    detail: error.to_string(),
                })?;

        if !nodes_output.is_success() {
            return Err(InventoryError::Unavailable {
                provider: PROVIDER.into(),
                detail: format!(
                    "{} exited with {:?}: {}",
                    nodes_output.command_line(),
                    nodes_output.exit_code,
                    nodes_output.stderr.trim()
                ),
            });
        }

        let partitions = match self.client.show_partitions(&self.allowlist).await {
            Ok(output) if output.is_success() => parser::parse_partitions(&output.stdout),
            _ => Vec::new(),
        };
        let controllers = match self.client.ping(&self.allowlist).await {
            Ok(output) if output.is_success() => parser::parse_ping(&output.stdout),
            _ => Vec::new(),
        };

        Ok(SlurmView {
            nodes: parser::parse_nodes(&nodes_output.stdout),
            partitions,
            controllers,
        })
    }
}

#[async_trait]
impl InventoryProvider for SlurmInventoryProvider {
    fn name(&self) -> &str {
        PROVIDER
    }

    async fn discover(&self) -> Result<InventorySnapshot, InventoryError> {
        let view = self.collect_view().await?;
        Ok(snapshot_from_view(&self.environment, &self.scheduler_name, &view))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn view_from(nodes: &str, partitions: &str, ping: &str) -> SlurmView {
        SlurmView {
            nodes: parser::parse_nodes(nodes),
            partitions: parser::parse_partitions(partitions),
            controllers: parser::parse_ping(ping),
        }
    }

    fn find<'a>(snapshot: &'a InventorySnapshot, entity_type: EntityType, name: &str) -> Option<&'a ManagedEntity> {
        snapshot
            .entities
            .iter()
            .find(|e| e.entity_type == entity_type && e.canonical_name == name)
    }

    #[test]
    fn nodes_become_hosts_with_the_compute_capability() {
        let view = view_from("NodeName=n1 State=IDLE\nNodeName=n2 State=IDLE\n", "", "");
        let snapshot = snapshot_from_view("lab", "sched", &view);

        let host = find(&snapshot, EntityType::Host, "n1").expect("host");
        assert!(host.capabilities.has(well_known::SLURM_COMPUTE));
        assert!(find(&snapshot, EntityType::Host, "n2").is_some());
    }

    #[test]
    fn the_slurmd_daemon_is_its_own_entity_hosted_on_the_node() {
        // So that "host healthy, slurmd dead" is expressible.
        let view = view_from("NodeName=n1 State=IDLE\n", "", "");
        let snapshot = snapshot_from_view("lab", "sched", &view);

        let service = find(&snapshot, EntityType::Service, "slurmd@n1").expect("service");
        let host_id = EntityKey::new("lab", EntityType::Host, "n1").entity_id();
        assert!(snapshot
            .dependencies
            .iter()
            .any(|e| e.source == service.id && e.target == host_id && e.dependency_type == DependencyType::HostedOn));
    }

    #[test]
    fn the_scheduler_is_an_entity_distinct_from_the_host_running_it() {
        // SPEC.md §12: Host(ctl) hosts Service(slurmctld) which provides the
        // Scheduler. Three entities, not one.
        let view = view_from("NodeName=n1 State=IDLE\n", "", "Slurmctld(primary) at ctl-a is UP");
        let snapshot = snapshot_from_view("lab", "sched", &view);

        let scheduler = find(&snapshot, EntityType::Scheduler, "sched").expect("scheduler");
        let service = find(&snapshot, EntityType::Service, "slurmctld@ctl-a").expect("slurmctld");
        let host = find(&snapshot, EntityType::Host, "ctl-a").expect("controller host");

        assert_ne!(scheduler.id, service.id);
        assert_ne!(service.id, host.id);
        assert!(snapshot
            .dependencies
            .iter()
            .any(|e| e.source == scheduler.id && e.target == service.id));
        assert!(snapshot
            .dependencies
            .iter()
            .any(|e| e.source == service.id && e.target == host.id));
    }

    #[test]
    fn the_controller_host_gains_the_controller_capability_not_a_role() {
        let view = view_from("", "", "Slurmctld(primary) at ctl-a is UP");
        let snapshot = snapshot_from_view("lab", "sched", &view);
        let host = find(&snapshot, EntityType::Host, "ctl-a").expect("host");
        assert!(host.capabilities.has(well_known::SLURM_CONTROLLER));
    }

    #[test]
    fn a_node_whose_slurm_name_differs_from_its_host_name_is_keyed_on_the_host() {
        // SPEC.md §38: the two must not be conflated.
        let view = view_from("NodeName=n1 NodeHostName=physical-a State=IDLE\n", "", "");
        let snapshot = snapshot_from_view("lab", "sched", &view);

        assert!(find(&snapshot, EntityType::Host, "physical-a").is_some());
        assert!(find(&snapshot, EntityType::Host, "n1").is_none());

        let host = find(&snapshot, EntityType::Host, "physical-a").expect("host");
        assert_eq!(host.metadata["slurm"]["node_name"], "n1");
    }

    #[test]
    fn slurm_state_is_preserved_as_metadata_not_flattened_to_a_boolean() {
        let view = view_from(
            "NodeName=n1 State=IDLE+DRAIN Partitions=compute Reason=disk full [root@2026-09-01T10:00:00]\n",
            "",
            "",
        );
        let snapshot = snapshot_from_view("lab", "sched", &view);
        let slurm = &find(&snapshot, EntityType::Host, "n1").expect("host").metadata["slurm"];

        assert_eq!(slurm["state"], "IDLE+DRAIN");
        assert_eq!(slurm["reason"], "disk full [root@2026-09-01T10:00:00]");
        assert_eq!(slurm["schedulable"], false);
        assert_eq!(slurm["partitions"][0], "compute");
    }

    #[test]
    fn a_node_depends_on_the_scheduler_only_importantly_not_critically() {
        // Losing the scheduler makes a node unusable for work; it does not make
        // the machine unhealthy, and must not cascade as though it did.
        let view = view_from("NodeName=n1 State=IDLE\n", "", "");
        let snapshot = snapshot_from_view("lab", "sched", &view);

        let edge = snapshot
            .dependencies
            .iter()
            .find(|e| e.dependency_type == DependencyType::UsesScheduler)
            .expect("scheduler edge");
        assert_eq!(edge.criticality, Criticality::Important);
    }

    #[test]
    fn everything_is_attributed_to_the_slurm_integration() {
        let view = view_from("NodeName=n1 State=IDLE\n", "", "");
        let snapshot = snapshot_from_view("lab", "sched", &view);
        let expected = DiscoverySource::Integration(PROVIDER.to_string());
        assert!(snapshot
            .entities
            .iter()
            .all(|e| e.discovery_sources.contains(&expected)));
        assert!(snapshot.dependencies.iter().all(|e| e.discovery_source == expected));
    }

    #[test]
    fn an_empty_cluster_still_yields_a_scheduler_entity() {
        // Otherwise a total control-plane outage would look like "nothing to
        // monitor" rather than "the scheduler is down".
        let snapshot = snapshot_from_view("lab", "sched", &SlurmView::default());
        assert!(find(&snapshot, EntityType::Scheduler, "sched").is_some());
    }

    #[test]
    fn the_scheduler_name_comes_from_the_caller_not_from_this_module() {
        let view = SlurmView::default();
        assert!(find(
            &snapshot_from_view("lab", "alpha", &view),
            EntityType::Scheduler,
            "alpha"
        )
        .is_some());
        assert!(find(&snapshot_from_view("lab", "beta", &view), EntityType::Scheduler, "beta").is_some());
    }

    #[tokio::test]
    async fn a_missing_scontrol_is_reported_as_unavailable_rather_than_panicking() {
        let provider = SlurmInventoryProvider::new(
            "lab",
            "sched",
            ScontrolClient::with_path(Some("/nonexistent/bin/scontrol".into())),
        );
        let error = provider.discover().await.expect_err("must fail");
        assert!(matches!(error, InventoryError::Unavailable { .. }), "{error:?}");
    }
}
