//! Slurm diagnosis rules.
//!
//! Three situations look identical to a monitor that only asks "is the node
//! usable", and an operator's response to each is completely different:
//!
//! | Situation | What is actually wrong | What to do |
//! | --- | --- | --- |
//! | Node DRAIN, everything else fine | Nothing on the machine | `scontrol resume`, once you know why |
//! | `slurmd` dead, host fine | One daemon | Restart the daemon |
//! | Host unreachable | The machine | Go and look at it |
//!
//! Every rule here demands **positive evidence that the host is fine** before
//! blaming a service. Without that, a dead host would be reported as a dead
//! daemon, and someone would spend an hour restarting a service on a machine
//! that is not there.

use crate::diagnosis::rules::reachability::verdicts_by_target;
use crate::diagnosis::{kind, Confidence, Diagnosis, DiagnosisContext, DiagnosisRule, RuleId};
use crate::entity::{EntityKey, EntityType};
use crate::integrations::slurm::observe::{PROBE_CONTROLLER, PROBE_NODE};
use crate::state::{classification, Health, StateComponent};

/// Whether there is positive evidence that this host is reachable and running.
///
/// Deliberately requires two independent signals. A single healthy component
/// could be stale, and concluding "the host is fine" from one reading is how a
/// dead machine gets reported as a dead daemon.
fn host_looks_healthy(context: &DiagnosisContext, host: crate::entity::EntityId) -> bool {
    // Network reachability alone is not enough; something on the host has to
    // be answering as well. SSH counts if the agent is not deployed there.
    let reachable = context.is_healthy(host, StateComponent::Network);
    let answering = context.is_healthy(host, StateComponent::Agent) || context.is_healthy(host, StateComponent::Ssh);
    reachable && answering
}

/// Evidence ids for a host, so a diagnosis can be traced back.
fn evidence_for(context: &DiagnosisContext, host: crate::entity::EntityId) -> Vec<crate::observation::ObservationId> {
    context
        .observations
        .for_entity(host)
        .into_iter()
        .map(|o| o.id)
        .collect()
}

/// The host is entirely healthy; only Slurm will not use it.
///
/// This is SPEC.md §169 and §67: a DRAIN is an administrative state, not a
/// fault, and reporting it as an outage trains operators to ignore outages.
pub struct SlurmOnlyDegradation;

impl DiagnosisRule for SlurmOnlyDegradation {
    fn id(&self) -> RuleId {
        RuleId::new("slurm.only_degradation")
    }

    fn description(&self) -> &str {
        "the host is healthy but the scheduler will not schedule work on it"
    }

    fn evaluate(&self, context: &DiagnosisContext) -> Vec<Diagnosis> {
        let mut diagnoses = Vec::new();

        for host in context.entities_of_type(EntityType::Host) {
            // Drained, and specifically *not* unresponsive: an unresponsive
            // node is a different rule's business.
            let drained = context.payload_bool(host.id, PROBE_NODE, "drained").unwrap_or(false);
            let not_responding = context
                .payload_bool(host.id, PROBE_NODE, "not_responding")
                .unwrap_or(false);
            if !drained || not_responding {
                continue;
            }

            if !host_looks_healthy(context, host.id) {
                continue;
            }

            let reason = context
                .payload_str(host.id, PROBE_NODE, "reason")
                .unwrap_or("no reason recorded");
            let state = context.payload_str(host.id, PROBE_NODE, "state").unwrap_or("unknown");

            diagnoses.push(
                Diagnosis::new(kind::SLURM_ONLY_DEGRADATION, self.id(), Confidence::High)
                    .affecting([host.id])
                    .rooted_at([host.id])
                    .with_evidence(evidence_for(context, host.id))
                    .with_summary(format!(
                        "{} is healthy but Slurm has it in {state}: {reason}",
                        host.canonical_name
                    ))
                    .recommending(vec![
                        format!("scontrol show node {}", host.canonical_name),
                        format!("sentinel entity show {}", host.canonical_name),
                    ]),
            );
        }

        diagnoses
    }
}

/// The host is fine but `slurmd` has stopped talking to the controller.
///
/// SPEC.md §170 and §68.
pub struct SlurmdServiceFailure;

impl DiagnosisRule for SlurmdServiceFailure {
    fn id(&self) -> RuleId {
        RuleId::new("slurm.slurmd_failure")
    }

    fn description(&self) -> &str {
        "the scheduler has lost contact with slurmd on a host that is otherwise healthy"
    }

    fn evaluate(&self, context: &DiagnosisContext) -> Vec<Diagnosis> {
        let mut diagnoses = Vec::new();

        for host in context.entities_of_type(EntityType::Host) {
            let not_responding = context
                .payload_bool(host.id, PROBE_NODE, "not_responding")
                .unwrap_or(false);
            let schedulable = context.payload_bool(host.id, PROBE_NODE, "schedulable").unwrap_or(true);
            let drained = context.payload_bool(host.id, PROBE_NODE, "drained").unwrap_or(false);

            // Not responding is the clear case. Otherwise, an unusable node
            // counts only if something other than an administrative drain made
            // it unusable -- a drain is a decision, not a fault, and the other
            // rule owns it. Both rules firing on one node would tell an
            // operator two contradictory things at once.
            let daemon_problem = not_responding || (!schedulable && !drained);
            if !daemon_problem {
                continue;
            }

            // The crucial guard. Without positive evidence that the machine is
            // up, "slurmd is dead" is a guess, and the more likely explanation
            // is that the whole host is gone.
            if !host_looks_healthy(context, host.id) {
                continue;
            }

            // A second guard, for the case where the network between the
            // scheduler and the node is itself in question. If observers
            // disagree about reaching this host, some path is broken -- and a
            // partition that cuts the controller off from a node also cuts
            // slurmd off from slurmctld, which makes the node look
            // unresponsive for a reason that has nothing to do with the
            // daemon. Blaming slurmd here would send someone to restart a
            // service that is running perfectly.
            if verdicts_by_target(context).get(&host.id).is_some_and(|v| v.disagree()) {
                continue;
            }

            let service_name = format!("slurmd@{}", host.canonical_name);
            let service = EntityKey::new(context.environment, EntityType::Service, &service_name).entity_id();
            let mut roots = vec![service];
            if context.entity(service).is_none() {
                // The service entity has not been discovered; blame the host,
                // but say so rather than inventing an entity.
                roots = vec![host.id];
            }

            let state = context.payload_str(host.id, PROBE_NODE, "state").unwrap_or("unknown");

            diagnoses.push(
                Diagnosis::new(kind::SLURMD_SERVICE_FAILURE, self.id(), Confidence::High)
                    .affecting([host.id])
                    .rooted_at(roots)
                    .with_evidence(evidence_for(context, host.id))
                    .with_summary(format!(
                        "{} answers on the network but Slurm reports {state}; slurmd is not registering",
                        host.canonical_name
                    ))
                    .recommending(vec![
                        format!("systemctl status slurmd  # on {}", host.canonical_name),
                        format!("journalctl -u slurmd -n 100  # on {}", host.canonical_name),
                        format!("scontrol show node {}", host.canonical_name),
                    ]),
            );
        }

        diagnoses
    }
}

/// The scheduler's control plane is impaired.
///
/// SPEC.md §97: several nodes losing their scheduler view at once, while the
/// hosts themselves are fine, points at the controller rather than at the nodes.
pub struct SlurmControlPlaneFailure;

impl DiagnosisRule for SlurmControlPlaneFailure {
    fn id(&self) -> RuleId {
        RuleId::new("slurm.control_plane_failure")
    }

    fn description(&self) -> &str {
        "the Slurm control plane is unreachable or its daemon has failed"
    }

    fn evaluate(&self, context: &DiagnosisContext) -> Vec<Diagnosis> {
        let mut diagnoses = Vec::new();

        for scheduler in context.entities_of_type(EntityType::Scheduler) {
            let reachable = context.payload_bool(scheduler.id, PROBE_CONTROLLER, "up");
            let service_health = context.component(scheduler.id, StateComponent::Service);

            // Only conclude anything from a *reported* failure. An absent
            // observation means the controller has not looked, which is not
            // evidence that the control plane is down.
            let failed = reachable == Some(false) || service_health == Health::Unavailable;
            if !failed {
                continue;
            }

            // Which daemons provide this scheduler, and are any of their hosts
            // fine? A controller host that is itself unreachable is a host
            // problem, not a control plane problem.
            let daemons: Vec<_> = context
                .inventory
                .graph()
                .dependencies_of(scheduler.id)
                .into_iter()
                .map(|edge| edge.target)
                .collect();

            let host_is_fine = daemons.iter().any(|daemon| {
                context
                    .inventory
                    .graph()
                    .dependencies_of(*daemon)
                    .iter()
                    .any(|edge| host_looks_healthy(context, edge.target))
            });

            let confidence = if host_is_fine {
                // The machine answers, so the daemon is the problem.
                Confidence::High
            } else {
                Confidence::Medium
            };

            let mut evidence = evidence_for(context, scheduler.id);
            for daemon in &daemons {
                evidence.extend(evidence_for(context, *daemon));
            }

            diagnoses.push(
                Diagnosis::new(kind::SLURM_CONTROL_PLANE_FAILURE, self.id(), confidence)
                    .affecting([scheduler.id])
                    .rooted_at(if daemons.is_empty() {
                        vec![scheduler.id]
                    } else {
                        daemons
                    })
                    .with_evidence(evidence)
                    .with_summary(format!(
                        "the Slurm control plane for {} is not answering",
                        scheduler.canonical_name
                    ))
                    .recommending(vec![
                        "scontrol ping".to_string(),
                        "systemctl status slurmctld  # on the controller".to_string(),
                        "journalctl -u slurmctld -n 100  # on the controller".to_string(),
                    ]),
            );
        }

        diagnoses
    }
}

/// What the scheduler has been told a node has does not match what it has.
///
/// SPEC.md §69 and §82. This is a configuration fault, not a hardware fault,
/// and it is worth reporting quietly rather than as an outage: the node may be
/// working perfectly and simply be described wrongly.
pub struct ResourceConfigurationMismatch;

impl DiagnosisRule for ResourceConfigurationMismatch {
    fn id(&self) -> RuleId {
        RuleId::new("slurm.resource_mismatch")
    }

    fn description(&self) -> &str {
        "a node's real hardware disagrees with the scheduler's configuration"
    }

    fn evaluate(&self, context: &DiagnosisContext) -> Vec<Diagnosis> {
        let mut diagnoses = Vec::new();

        for host in context.entities_of_type(EntityType::Host) {
            let Some(observed) = host.metadata.get("hardware") else {
                continue;
            };

            let mut mismatches = Vec::new();

            // GPUs. `None` from either side means "not stated", which is not a
            // mismatch: a node with no Gres line is not a node with no GPUs.
            let configured_gpus = context.payload_u64(host.id, PROBE_NODE, "configured_gpu_count");
            let observed_gpus = observed.get("gpus").and_then(|v| v.as_u64());
            if let (Some(configured), Some(observed)) = (configured_gpus, observed_gpus) {
                if configured != observed {
                    mismatches.push(format!(
                        "Slurm expects {configured} GPU(s), the host reports {observed}"
                    ));
                }
            }

            // CPUs.
            let configured_cpus = context.payload_u64(host.id, PROBE_NODE, "cpu_total");
            let observed_cpus = observed.get("cpus").and_then(|v| v.as_u64());
            if let (Some(configured), Some(observed)) = (configured_cpus, observed_cpus) {
                if configured > observed {
                    // Only *over*-configuration is a fault: Slurm using fewer
                    // CPUs than the machine has is a deliberate and common
                    // choice, and flagging it would be noise.
                    mismatches.push(format!(
                        "Slurm expects {configured} CPU(s), the host reports {observed}"
                    ));
                }
            }

            if mismatches.is_empty() {
                continue;
            }

            let diagnosis_type = if configured_gpus.is_some() && observed_gpus.is_some() && mismatches.len() == 1 {
                kind::GPU_CONFIGURATION_MISMATCH
            } else {
                kind::RESOURCE_CONFIGURATION_MISMATCH
            };

            diagnoses.push(
                Diagnosis::new(diagnosis_type, self.id(), Confidence::High)
                    .affecting([host.id])
                    .rooted_at([host.id])
                    .with_evidence(evidence_for(context, host.id))
                    .with_summary(format!("{}: {}", host.canonical_name, mismatches.join("; ")))
                    .recommending(vec![
                        format!("scontrol show node {}", host.canonical_name),
                        format!("sentinel entity show {} --json", host.canonical_name),
                    ]),
            );
        }

        diagnoses
    }
}

/// The classification a Slurm-only degradation implies.
pub fn classification_for(diagnosis: &Diagnosis) -> Option<&'static str> {
    match diagnosis.diagnosis_type.as_str() {
        kind::SLURM_ONLY_DEGRADATION => Some(classification::SCHEDULER_DEGRADED),
        kind::SLURMD_SERVICE_FAILURE => Some(classification::SERVICE_FAILURE),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::capability::CapabilitySet;
    use crate::diagnosis::ObservationIndex;
    use crate::entity::{EntityId, ManagedEntity};
    use crate::inventory::Inventory;
    use crate::observation::{Observation, ProbeStatus};
    use crate::probes::ProbeId;
    use crate::state::{ComponentState, EntityState};
    use std::collections::HashMap;

    /// A world a rule can be evaluated against.
    struct World {
        inventory: Inventory,
        states: HashMap<EntityId, EntityState>,
        observations: ObservationIndex,
    }

    impl World {
        fn new() -> Self {
            Self {
                inventory: Inventory::new(),
                states: HashMap::new(),
                observations: ObservationIndex::new(),
            }
        }

        fn with_host(mut self, name: &str) -> Self {
            let entity = ManagedEntity::new("lab", EntityType::Host, name)
                .with_capabilities(CapabilitySet::from_iter(["slurm.compute", "sentinel.agent"]));
            self.inventory.insert_entity(entity);
            self
        }

        fn healthy_host_evidence(mut self, name: &str) -> Self {
            let id = Self::host_id(name);
            let state = self.states.entry(id).or_insert_with(|| EntityState::unknown(id));
            state.set_component(StateComponent::Network, ComponentState::new(Health::Healthy));
            state.set_component(StateComponent::Agent, ComponentState::new(Health::Healthy));
            self
        }

        fn unreachable_host_evidence(mut self, name: &str) -> Self {
            let id = Self::host_id(name);
            let state = self.states.entry(id).or_insert_with(|| EntityState::unknown(id));
            state.set_component(StateComponent::Network, ComponentState::new(Health::Unavailable));
            state.set_component(StateComponent::Agent, ComponentState::new(Health::Unavailable));
            self
        }

        fn slurm_node(mut self, name: &str, status: ProbeStatus, payload: serde_json::Value) -> Self {
            self.observations
                .insert(Observation::new(ProbeId::new(PROBE_NODE), Self::host_id(name), status).with_payload(payload));
            self
        }

        fn with_hardware(mut self, name: &str, hardware: serde_json::Value) -> Self {
            let id = Self::host_id(name);
            if let Some(entity) = self.inventory.get(id).cloned() {
                let mut entity = entity;
                entity.metadata = serde_json::json!({ "hardware": hardware });
                self.inventory.insert_entity(entity);
            }
            self
        }

        fn host_id(name: &str) -> EntityId {
            EntityKey::new("lab", EntityType::Host, name).entity_id()
        }

        fn evaluate(&self, rule: &dyn DiagnosisRule) -> Vec<Diagnosis> {
            let context = DiagnosisContext {
                environment: "lab",
                inventory: &self.inventory,
                states: &self.states,
                observations: &self.observations,
            };
            rule.evaluate(&context)
        }
    }

    fn drained(reason: &str) -> serde_json::Value {
        serde_json::json!({
            "drained": true,
            "not_responding": false,
            "schedulable": false,
            "state": "IDLE+DRAIN",
            "reason": reason,
        })
    }

    fn not_responding() -> serde_json::Value {
        serde_json::json!({
            "drained": false,
            "not_responding": true,
            "schedulable": false,
            "state": "DOWN*",
            "reason": "Not responding",
        })
    }

    fn healthy_node() -> serde_json::Value {
        serde_json::json!({
            "drained": false,
            "not_responding": false,
            "schedulable": true,
            "state": "IDLE",
        })
    }

    // --- SLURM_ONLY_DEGRADATION ------------------------------------------

    #[test]
    fn a_drained_but_healthy_node_is_a_slurm_only_degradation() {
        let world = World::new()
            .with_host("node-a")
            .healthy_host_evidence("node-a")
            .slurm_node("node-a", ProbeStatus::Degraded, drained("scheduled maintenance"));

        let diagnoses = world.evaluate(&SlurmOnlyDegradation);
        assert_eq!(diagnoses.len(), 1);
        assert!(diagnoses[0].is(kind::SLURM_ONLY_DEGRADATION));
        assert_eq!(diagnoses[0].confidence, Confidence::High);
        assert!(
            diagnoses[0].summary.contains("scheduled maintenance"),
            "{}",
            diagnoses[0].summary
        );
        assert_eq!(
            classification_for(&diagnoses[0]),
            Some(classification::SCHEDULER_DEGRADED)
        );
    }

    #[test]
    fn a_healthy_node_produces_no_degradation_diagnosis() {
        let world = World::new()
            .with_host("node-a")
            .healthy_host_evidence("node-a")
            .slurm_node("node-a", ProbeStatus::Ok, healthy_node());
        assert!(world.evaluate(&SlurmOnlyDegradation).is_empty());
    }

    #[test]
    fn a_drained_node_on_an_unreachable_host_is_not_a_slurm_only_problem() {
        // Without evidence that the machine is fine, "only Slurm is unhappy"
        // is a claim we cannot support.
        let world = World::new()
            .with_host("node-a")
            .unreachable_host_evidence("node-a")
            .slurm_node("node-a", ProbeStatus::Degraded, drained("unknown"));
        assert!(world.evaluate(&SlurmOnlyDegradation).is_empty());
    }

    #[test]
    fn a_drained_node_with_no_host_evidence_at_all_is_not_diagnosed() {
        // Silence is not evidence of health.
        let world = World::new()
            .with_host("node-a")
            .slurm_node("node-a", ProbeStatus::Degraded, drained("unknown"));
        assert!(world.evaluate(&SlurmOnlyDegradation).is_empty());
    }

    #[test]
    fn a_drained_and_unresponsive_node_is_not_a_slurm_only_degradation() {
        // DRAIN plus NOT_RESPONDING is a daemon problem wearing a DRAIN label.
        let payload = serde_json::json!({
            "drained": true,
            "not_responding": true,
            "schedulable": false,
            "state": "DOWN+DRAIN*",
        });
        let world = World::new()
            .with_host("node-a")
            .healthy_host_evidence("node-a")
            .slurm_node("node-a", ProbeStatus::Failed, payload);
        assert!(world.evaluate(&SlurmOnlyDegradation).is_empty());
    }

    #[test]
    fn a_degradation_diagnosis_suggests_only_read_only_commands() {
        // SPEC.md §113/§114: Sentinel suggests investigation, never action.
        let world = World::new()
            .with_host("node-a")
            .healthy_host_evidence("node-a")
            .slurm_node("node-a", ProbeStatus::Degraded, drained("x"));

        for action in &world.evaluate(&SlurmOnlyDegradation)[0].recommended_actions {
            for mutating in ["resume", "update", "restart", "reboot", "drain "] {
                assert!(!action.contains(mutating), "recommended a mutating command: {action}");
            }
        }
    }

    // --- SLURMD_SERVICE_FAILURE ------------------------------------------

    #[test]
    fn an_unresponsive_node_on_a_healthy_host_is_a_slurmd_failure() {
        let world = World::new()
            .with_host("node-a")
            .healthy_host_evidence("node-a")
            .slurm_node("node-a", ProbeStatus::Failed, not_responding());

        let diagnoses = world.evaluate(&SlurmdServiceFailure);
        assert_eq!(diagnoses.len(), 1);
        assert!(diagnoses[0].is(kind::SLURMD_SERVICE_FAILURE));
        assert!(
            diagnoses[0].summary.contains("answers on the network"),
            "{}",
            diagnoses[0].summary
        );
    }

    #[test]
    fn an_unresponsive_node_on_an_unreachable_host_is_not_blamed_on_slurmd() {
        // The single most important guard in this file: without it, a dead
        // machine is reported as a dead daemon and someone spends an hour
        // restarting a service on a host that is not there.
        let world = World::new()
            .with_host("node-a")
            .unreachable_host_evidence("node-a")
            .slurm_node("node-a", ProbeStatus::Failed, not_responding());
        assert!(world.evaluate(&SlurmdServiceFailure).is_empty());
    }

    #[test]
    fn a_slurmd_failure_names_the_service_when_it_is_known() {
        let mut world = World::new()
            .with_host("node-a")
            .healthy_host_evidence("node-a")
            .slurm_node("node-a", ProbeStatus::Failed, not_responding());
        let service = ManagedEntity::new("lab", EntityType::Service, "slurmd@node-a");
        let service_id = service.id;
        world.inventory.insert_entity(service);

        let diagnoses = world.evaluate(&SlurmdServiceFailure);
        assert_eq!(diagnoses[0].suspected_root_entities, vec![service_id]);
    }

    #[test]
    fn a_slurmd_failure_blames_the_host_when_the_service_is_unknown() {
        let world = World::new()
            .with_host("node-a")
            .healthy_host_evidence("node-a")
            .slurm_node("node-a", ProbeStatus::Failed, not_responding());

        let diagnoses = world.evaluate(&SlurmdServiceFailure);
        assert_eq!(diagnoses[0].suspected_root_entities, vec![World::host_id("node-a")]);
    }

    #[test]
    fn a_contested_network_defers_to_the_path_diagnosis() {
        // A partition between the controller and a node also cuts slurmd off
        // from slurmctld, so the node looks unresponsive for a reason that has
        // nothing to do with the daemon.
        let mut world = World::new()
            .with_host("node-a")
            .healthy_host_evidence("node-a")
            .slurm_node("node-a", ProbeStatus::Failed, not_responding());

        // Two observers disagree about reaching the node.
        for (observer, responded) in [("peer-a", true), ("peer-b", false)] {
            world
                .inventory
                .insert_entity(ManagedEntity::new("lab", EntityType::Host, observer));
            world.observations.insert(
                Observation::new(
                    ProbeId::new(crate::probes::network::PROBE_ID),
                    World::host_id("node-a"),
                    if responded {
                        ProbeStatus::Ok
                    } else {
                        ProbeStatus::Timeout
                    },
                )
                .with_observer(World::host_id(observer))
                .with_payload(serde_json::json!({"host_responded": responded})),
            );
        }

        assert!(
            world.evaluate(&SlurmdServiceFailure).is_empty(),
            "with the path in question, the daemon must not be blamed"
        );
    }

    #[test]
    fn a_drained_node_is_not_reported_as_a_slurmd_failure() {
        // The two rules must not both fire: an operator told both "the daemon
        // is dead" and "only Slurm is unhappy" learns nothing.
        let world = World::new()
            .with_host("node-a")
            .healthy_host_evidence("node-a")
            .slurm_node("node-a", ProbeStatus::Degraded, drained("maintenance"));

        assert_eq!(world.evaluate(&SlurmOnlyDegradation).len(), 1);
        assert!(
            world.evaluate(&SlurmdServiceFailure).is_empty(),
            "a drained node is administratively unusable, not broken"
        );
    }

    #[test]
    fn a_slurmd_failure_suggests_where_to_look() {
        let world = World::new()
            .with_host("node-a")
            .healthy_host_evidence("node-a")
            .slurm_node("node-a", ProbeStatus::Failed, not_responding());

        let actions = &world.evaluate(&SlurmdServiceFailure)[0].recommended_actions;
        assert!(actions.iter().any(|a| a.contains("systemctl status slurmd")));
        assert!(actions.iter().any(|a| a.contains("journalctl")));
        for action in actions {
            assert!(!action.contains("restart"), "recommended a mutating command: {action}");
        }
    }

    // --- RESOURCE_CONFIGURATION_MISMATCH ---------------------------------

    #[test]
    fn a_gpu_count_disagreement_is_reported() {
        let world = World::new()
            .with_host("node-a")
            .with_hardware("node-a", serde_json::json!({"gpus": 2, "cpus": 64}))
            .slurm_node(
                "node-a",
                ProbeStatus::Ok,
                serde_json::json!({"configured_gpu_count": 4, "cpu_total": 64, "schedulable": true}),
            );

        let diagnoses = world.evaluate(&ResourceConfigurationMismatch);
        assert_eq!(diagnoses.len(), 1);
        assert!(diagnoses[0].is(kind::GPU_CONFIGURATION_MISMATCH));
        assert!(diagnoses[0].summary.contains("4 GPU"), "{}", diagnoses[0].summary);
    }

    #[test]
    fn matching_hardware_produces_no_diagnosis() {
        let world = World::new()
            .with_host("node-a")
            .with_hardware("node-a", serde_json::json!({"gpus": 4, "cpus": 64}))
            .slurm_node(
                "node-a",
                ProbeStatus::Ok,
                serde_json::json!({"configured_gpu_count": 4, "cpu_total": 64}),
            );
        assert!(world.evaluate(&ResourceConfigurationMismatch).is_empty());
    }

    #[test]
    fn a_node_slurm_says_nothing_about_is_not_a_mismatch() {
        // No Gres line means "not stated", not "zero GPUs".
        let world = World::new()
            .with_host("node-a")
            .with_hardware("node-a", serde_json::json!({"gpus": 2, "cpus": 64}))
            .slurm_node("node-a", ProbeStatus::Ok, serde_json::json!({"cpu_total": 64}));
        assert!(world.evaluate(&ResourceConfigurationMismatch).is_empty());
    }

    #[test]
    fn slurm_using_fewer_cpus_than_the_machine_has_is_not_a_fault() {
        // A deliberate and common choice; flagging it would be pure noise.
        let world = World::new()
            .with_host("node-a")
            .with_hardware("node-a", serde_json::json!({"cpus": 64}))
            .slurm_node("node-a", ProbeStatus::Ok, serde_json::json!({"cpu_total": 16}));
        assert!(world.evaluate(&ResourceConfigurationMismatch).is_empty());
    }

    #[test]
    fn slurm_expecting_more_cpus_than_exist_is_a_fault() {
        let world = World::new()
            .with_host("node-a")
            .with_hardware("node-a", serde_json::json!({"cpus": 16}))
            .slurm_node("node-a", ProbeStatus::Ok, serde_json::json!({"cpu_total": 64}));

        let diagnoses = world.evaluate(&ResourceConfigurationMismatch);
        assert_eq!(diagnoses.len(), 1);
        assert!(diagnoses[0].is(kind::RESOURCE_CONFIGURATION_MISMATCH));
    }

    #[test]
    fn a_host_with_no_reported_hardware_is_not_diagnosed() {
        let world = World::new().with_host("node-a").slurm_node(
            "node-a",
            ProbeStatus::Ok,
            serde_json::json!({"configured_gpu_count": 4}),
        );
        assert!(world.evaluate(&ResourceConfigurationMismatch).is_empty());
    }

    // --- SLURM_CONTROL_PLANE_FAILURE --------------------------------------

    #[test]
    fn a_controller_reporting_itself_down_is_a_control_plane_failure() {
        let mut world = World::new();
        let scheduler = ManagedEntity::new("lab", EntityType::Scheduler, "test-cluster");
        let scheduler_id = scheduler.id;
        world.inventory.insert_entity(scheduler);
        world.observations.insert(
            Observation::new(ProbeId::new(PROBE_CONTROLLER), scheduler_id, ProbeStatus::Failed)
                .with_payload(serde_json::json!({"up": false, "host": "ctl-a"})),
        );

        let diagnoses = world.evaluate(&SlurmControlPlaneFailure);
        assert_eq!(diagnoses.len(), 1);
        assert!(diagnoses[0].is(kind::SLURM_CONTROL_PLANE_FAILURE));
    }

    #[test]
    fn a_healthy_control_plane_produces_nothing() {
        let mut world = World::new();
        let scheduler = ManagedEntity::new("lab", EntityType::Scheduler, "test-cluster");
        let scheduler_id = scheduler.id;
        world.inventory.insert_entity(scheduler);
        world.observations.insert(
            Observation::new(ProbeId::new(PROBE_CONTROLLER), scheduler_id, ProbeStatus::Ok)
                .with_payload(serde_json::json!({"up": true})),
        );

        assert!(world.evaluate(&SlurmControlPlaneFailure).is_empty());
    }

    #[test]
    fn a_scheduler_nobody_has_looked_at_is_not_declared_broken() {
        // No observation means no evidence, and no evidence means no diagnosis.
        let mut world = World::new();
        world
            .inventory
            .insert_entity(ManagedEntity::new("lab", EntityType::Scheduler, "test-cluster"));
        assert!(world.evaluate(&SlurmControlPlaneFailure).is_empty());
    }
}
