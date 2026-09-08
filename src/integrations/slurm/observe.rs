//! Turning a Slurm view into observations.
//!
//! These are *facts about what the scheduler says*, not conclusions about the
//! machines. `slurm.node` reporting `Failed` means "Slurm will not use this
//! node"; whether the host is healthy is a different question, answered by
//! different probes and reconciled by the diagnosis engine
//! (IMPLEMENTATION.md §68).

use crate::entity::{EntityKey, EntityType};
use crate::observation::{Observation, ProbeStatus};
use crate::probes::ProbeId;

use super::parser::{NodeStateFlag, SlurmNode};
use super::SlurmView;

/// Probe id for the scheduler's view of one node.
pub const PROBE_NODE: &str = "slurm.node";
/// Probe id for the scheduler control plane's reachability.
pub const PROBE_CONTROLLER: &str = "slurm.controller";

/// Build observations from one Slurm discovery cycle.
///
/// `scheduler_name` and `environment` are supplied by the caller so no
/// deployment identifier is baked in here.
pub fn observations_from_view(environment: &str, scheduler_name: &str, view: &SlurmView) -> Vec<Observation> {
    let mut observations = Vec::new();

    for node in &view.nodes {
        let host = EntityKey::new(environment, EntityType::Host, node.host_name()).entity_id();
        observations.push(
            Observation::new(ProbeId::new(PROBE_NODE), host, node_status(node)).with_payload(serde_json::json!({
                "node_name": node.node_name,
                "state": node.state.raw,
                "schedulable": node.state.is_schedulable(),
                "drained": node.state.is_drained(),
                "not_responding": node.state.is_not_responding(),
                "reason": node.reason,
                "partitions": node.partitions,
                "configured_gpu_count": node.configured_gpu_count(),
                "cpu_total": node.cpu_total,
                "real_memory_mb": node.real_memory_mb,
            })),
        );
    }

    let scheduler = EntityKey::new(environment, EntityType::Scheduler, scheduler_name).entity_id();
    for controller in &view.controllers {
        let service_name = format!("slurmctld@{}", controller.host);
        let service = EntityKey::new(environment, EntityType::Service, &service_name).entity_id();
        let status = if controller.up {
            ProbeStatus::Ok
        } else {
            ProbeStatus::Failed
        };

        let payload = serde_json::json!({
            "host": controller.host,
            "role": controller.role,
            "up": controller.up,
        });
        observations
            .push(Observation::new(ProbeId::new(PROBE_CONTROLLER), service, status).with_payload(payload.clone()));
        observations.push(Observation::new(ProbeId::new(PROBE_CONTROLLER), scheduler, status).with_payload(payload));
    }

    observations
}

/// The scheduler's verdict on a node, as a probe status.
///
/// Note what this does **not** do: a node Slurm calls DOWN is reported
/// `Failed` for the *scheduler* component only. Concluding that the machine is
/// down would need evidence from probes that actually talk to the machine.
fn node_status(node: &SlurmNode) -> ProbeStatus {
    // A node Slurm has deliberately powered down tells us nothing about its
    // health, so claiming it is healthy would be inventing evidence. This is
    // checked first: Slurm still calls such a node schedulable, because it
    // will power it back up on demand.
    if node.state.has(&NodeStateFlag::PowerSave)
        || node.state.has(&NodeStateFlag::PoweringUp)
        || node.state.has(&NodeStateFlag::PoweringDown)
    {
        return ProbeStatus::NotApplicable;
    }
    if node.state.is_schedulable() {
        return ProbeStatus::Ok;
    }
    // Draining while otherwise fine is a degradation: the node works, the
    // scheduler has just been told not to use it.
    if node.state.base.is_schedulable() && node.state.is_drained() && !node.state.is_not_responding() {
        return ProbeStatus::Degraded;
    }
    ProbeStatus::Failed
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::integrations::slurm::parser;

    fn view(nodes: &str, ping: &str) -> SlurmView {
        SlurmView {
            nodes: parser::parse_nodes(nodes),
            partitions: Vec::new(),
            controllers: parser::parse_ping(ping),
        }
    }

    fn status_of(state: &str) -> ProbeStatus {
        let node = parser::SlurmNode::parse_line(&format!("NodeName=n1 State={state}")).expect("parse");
        node_status(&node)
    }

    #[test]
    fn a_schedulable_node_reports_ok() {
        assert_eq!(status_of("IDLE"), ProbeStatus::Ok);
        assert_eq!(status_of("MIXED"), ProbeStatus::Ok);
        assert_eq!(status_of("ALLOCATED"), ProbeStatus::Ok);
    }

    #[test]
    fn a_drained_but_otherwise_healthy_node_is_degraded_not_failed() {
        // The machine works; the scheduler has been told not to use it.
        assert_eq!(status_of("IDLE+DRAIN"), ProbeStatus::Degraded);
        assert_eq!(status_of("MIXED+DRAIN"), ProbeStatus::Degraded);
    }

    #[test]
    fn a_down_or_unresponsive_node_reports_failed() {
        assert_eq!(status_of("DOWN"), ProbeStatus::Failed);
        assert_eq!(status_of("DOWN*"), ProbeStatus::Failed);
        assert_eq!(status_of("DOWN+DRAIN"), ProbeStatus::Failed);
        assert_eq!(
            status_of("IDLE*"),
            ProbeStatus::Failed,
            "not responding is a failure even from idle"
        );
    }

    #[test]
    fn a_deliberately_powered_down_node_is_neither_healthy_nor_a_fault() {
        // Slurm still calls these schedulable; we have observed nothing about
        // the machine, so we claim nothing.
        assert_eq!(status_of("IDLE~"), ProbeStatus::NotApplicable);
        assert_eq!(status_of("IDLE#"), ProbeStatus::NotApplicable);
        assert_eq!(status_of("IDLE%"), ProbeStatus::NotApplicable);
        assert_eq!(status_of("DOWN~"), ProbeStatus::NotApplicable);
    }

    #[test]
    fn an_unknown_future_state_is_treated_as_a_failure_not_silently_ignored() {
        // Better to raise a question than to report health we cannot justify.
        assert_eq!(status_of("SOME_NEW_STATE"), ProbeStatus::Failed);
    }

    #[test]
    fn node_observations_target_the_host_entity_and_carry_the_slurm_facts() {
        let view = view(
            "NodeName=n1 NodeHostName=physical-a State=IDLE+DRAIN Reason=maintenance\n",
            "",
        );
        let observations = observations_from_view("lab", "sched", &view);

        assert_eq!(observations.len(), 1);
        let observation = &observations[0];
        assert_eq!(
            observation.target_entity,
            EntityKey::new("lab", EntityType::Host, "physical-a").entity_id()
        );
        assert_eq!(observation.status, ProbeStatus::Degraded);
        assert_eq!(observation.payload["node_name"], "n1");
        assert_eq!(observation.payload["state"], "IDLE+DRAIN");
        assert_eq!(observation.payload["drained"], true);
        assert_eq!(observation.payload["reason"], "maintenance");
    }

    #[test]
    fn observations_state_facts_without_naming_a_cause() {
        // A probe payload must never contain a diagnosis.
        let view = view("NodeName=n1 State=DOWN*\n", "");
        let text = serde_json::to_string(&observations_from_view("lab", "sched", &view)).expect("serialize");
        for diagnosis in ["HOST_UNREACHABLE", "SLURMD_SERVICE_FAILURE", "POWER_OFF"] {
            assert!(!text.contains(diagnosis), "a probe must not conclude {diagnosis}");
        }
    }

    #[test]
    fn a_controller_ping_observes_both_the_daemon_and_the_scheduler() {
        let view = view("", "Slurmctld(primary) at ctl-a is UP");
        let observations = observations_from_view("lab", "sched", &view);

        assert_eq!(observations.len(), 2);
        let targets: Vec<_> = observations.iter().map(|o| o.target_entity).collect();
        assert!(targets.contains(&EntityKey::new("lab", EntityType::Service, "slurmctld@ctl-a").entity_id()));
        assert!(targets.contains(&EntityKey::new("lab", EntityType::Scheduler, "sched").entity_id()));
        assert!(observations.iter().all(|o| o.status == ProbeStatus::Ok));
    }

    #[test]
    fn a_down_controller_is_observed_as_failed() {
        let view = view("", "Slurmctld(primary) at ctl-a is DOWN");
        let observations = observations_from_view("lab", "sched", &view);
        assert!(observations.iter().all(|o| o.status == ProbeStatus::Failed));
    }

    #[test]
    fn an_empty_view_produces_no_observations_rather_than_false_health() {
        // Silence must never be recorded as "everything is fine".
        assert!(observations_from_view("lab", "sched", &SlurmView::default()).is_empty());
    }
}
