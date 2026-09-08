//! Service-level rules: one thing on a host is broken, and the host is not.
//!
//! These two look identical to an up/down monitor and demand opposite
//! reactions:
//!
//! * **The agent is dead.** The monitoring stopped, not the machine. Nobody
//!   needs to leave their desk; someone needs to restart a daemon. Reporting
//!   this as a host failure is how a working machine gets investigated at 3am.
//! * **SSH is dead.** An operator is locked out of a machine that is otherwise
//!   working perfectly, which is urgent for a different reason and not an
//!   outage at all.
//!
//! Both rules require positive evidence that the host is answering *by some
//! other means*, so neither can fire for a host that has simply gone away.

use crate::diagnosis::{kind, Confidence, Diagnosis, DiagnosisContext, DiagnosisRule, RuleId};
use crate::entity::{EntityId, EntityType};
use crate::state::{Health, StateComponent};

/// Evidence ids for an entity.
fn evidence_for(context: &DiagnosisContext, entity: EntityId) -> Vec<crate::observation::ObservationId> {
    context
        .observations
        .for_entity(entity)
        .into_iter()
        .map(|o| o.id)
        .collect()
}

/// Whether a component is positively broken, as opposed to merely unknown.
fn is_broken(context: &DiagnosisContext, entity: EntityId, component: StateComponent) -> bool {
    matches!(
        context.component(entity, component),
        Health::Unavailable | Health::Degraded
    )
}

/// The Sentinel agent has stopped while its host keeps answering.
pub struct SentinelAgentFailure;

impl DiagnosisRule for SentinelAgentFailure {
    fn id(&self) -> RuleId {
        RuleId::new("service.sentinel_agent_failure")
    }

    fn description(&self) -> &str {
        "the Sentinel agent is not answering on a host that is otherwise healthy"
    }

    fn evaluate(&self, context: &DiagnosisContext) -> Vec<Diagnosis> {
        let mut diagnoses = Vec::new();

        for host in context.entities_of_type(EntityType::Host) {
            if !is_broken(context, host.id, StateComponent::Agent) {
                continue;
            }

            // The host has to be answering some other way. Network alone is
            // enough here: the agent's own port failing while the machine
            // still accepts connections is exactly the distinction being made.
            if !context.is_healthy(host.id, StateComponent::Network) {
                continue;
            }

            // If SSH is also down, something larger is happening and this rule
            // would be the wrong story to tell.
            if is_broken(context, host.id, StateComponent::Ssh) {
                continue;
            }

            diagnoses.push(
                Diagnosis::new(kind::SENTINEL_AGENT_FAILURE, self.id(), Confidence::High)
                    .affecting([host.id])
                    .rooted_at([host.id])
                    .with_evidence(evidence_for(context, host.id))
                    .with_summary(format!(
                        "the Sentinel agent on {} is not answering, but the host is reachable; \
                         the monitoring has stopped, not the machine",
                        host.canonical_name
                    ))
                    .recommending(vec![
                        format!("systemctl status sentinel-agent  # on {}", host.canonical_name),
                        format!("journalctl -u sentinel-agent -n 100  # on {}", host.canonical_name),
                    ]),
            );
        }

        diagnoses
    }
}

/// SSH has stopped while its host keeps answering.
pub struct SshServiceFailure;

impl DiagnosisRule for SshServiceFailure {
    fn id(&self) -> RuleId {
        RuleId::new("service.ssh_failure")
    }

    fn description(&self) -> &str {
        "SSH is not answering on a host that is otherwise healthy"
    }

    fn evaluate(&self, context: &DiagnosisContext) -> Vec<Diagnosis> {
        let mut diagnoses = Vec::new();

        for host in context.entities_of_type(EntityType::Host) {
            if !is_broken(context, host.id, StateComponent::Ssh) {
                continue;
            }
            if !context.is_healthy(host.id, StateComponent::Network) {
                continue;
            }

            // The agent answering is the strongest evidence the machine is
            // fine, but a host without an agent can still be diagnosed from
            // reachability alone.
            let agent_confirms = context.is_healthy(host.id, StateComponent::Agent);
            let confidence = if agent_confirms {
                Confidence::High
            } else {
                Confidence::Medium
            };

            let detail = if agent_confirms {
                "the Sentinel agent is answering, so the host is up"
            } else {
                "the host is reachable"
            };

            diagnoses.push(
                Diagnosis::new(kind::SSH_SERVICE_FAILURE, self.id(), confidence)
                    .affecting([host.id])
                    .rooted_at([host.id])
                    .with_evidence(evidence_for(context, host.id))
                    .with_summary(format!(
                        "SSH is not answering on {}; {detail}, so this is a lockout rather than an outage",
                        host.canonical_name
                    ))
                    .recommending(vec![
                        format!("systemctl status sshd  # on {}", host.canonical_name),
                        format!("journalctl -u sshd -n 100  # on {}", host.canonical_name),
                    ]),
            );
        }

        diagnoses
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::capability::CapabilitySet;
    use crate::diagnosis::ObservationIndex;
    use crate::entity::{EntityKey, ManagedEntity};
    use crate::inventory::Inventory;
    use crate::state::{ComponentState, EntityState};
    use std::collections::HashMap;

    fn host(name: &str) -> EntityId {
        EntityKey::new("lab", EntityType::Host, name).entity_id()
    }

    struct World {
        inventory: Inventory,
        states: HashMap<EntityId, EntityState>,
        observations: ObservationIndex,
    }

    impl World {
        fn new(name: &str) -> Self {
            let mut inventory = Inventory::new();
            inventory.insert_entity(ManagedEntity::new("lab", EntityType::Host, name).with_capabilities(
                CapabilitySet::from_iter(["ssh.server", "sentinel.agent", "network.tcp"]),
            ));
            Self {
                inventory,
                states: HashMap::new(),
                observations: ObservationIndex::new(),
            }
        }

        fn component(mut self, name: &str, component: StateComponent, health: Health) -> Self {
            let id = host(name);
            let state = self.states.entry(id).or_insert_with(|| EntityState::unknown(id));
            state.set_component(component, ComponentState::new(health));
            self
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

    // --- SENTINEL_AGENT_FAILURE -------------------------------------------

    #[test]
    fn a_dead_agent_on_a_reachable_host_is_a_monitoring_failure() {
        let world = World::new("node-a")
            .component("node-a", StateComponent::Agent, Health::Unavailable)
            .component("node-a", StateComponent::Network, Health::Healthy)
            .component("node-a", StateComponent::Ssh, Health::Healthy);

        let diagnoses = world.evaluate(&SentinelAgentFailure);
        assert_eq!(diagnoses.len(), 1);
        assert!(diagnoses[0].is(kind::SENTINEL_AGENT_FAILURE));
        assert!(
            diagnoses[0]
                .summary
                .contains("the monitoring has stopped, not the machine"),
            "{}",
            diagnoses[0].summary
        );
    }

    #[test]
    fn an_unreachable_host_is_not_reported_as_an_agent_failure() {
        // The machine is gone. Blaming the agent would send someone to restart
        // a daemon on a host that is not there.
        let world = World::new("node-a")
            .component("node-a", StateComponent::Agent, Health::Unavailable)
            .component("node-a", StateComponent::Network, Health::Unavailable);

        assert!(world.evaluate(&SentinelAgentFailure).is_empty());
    }

    #[test]
    fn an_agent_failure_alongside_an_ssh_failure_is_not_reported_as_a_monitoring_problem() {
        // Two services down at once is a larger story, and this rule would be
        // the wrong one to tell.
        let world = World::new("node-a")
            .component("node-a", StateComponent::Agent, Health::Unavailable)
            .component("node-a", StateComponent::Ssh, Health::Unavailable)
            .component("node-a", StateComponent::Network, Health::Healthy);

        assert!(world.evaluate(&SentinelAgentFailure).is_empty());
    }

    #[test]
    fn a_healthy_agent_produces_nothing() {
        let world = World::new("node-a")
            .component("node-a", StateComponent::Agent, Health::Healthy)
            .component("node-a", StateComponent::Network, Health::Healthy);
        assert!(world.evaluate(&SentinelAgentFailure).is_empty());
    }

    #[test]
    fn an_agent_failure_recommends_looking_at_the_agent() {
        let world = World::new("node-a")
            .component("node-a", StateComponent::Agent, Health::Unavailable)
            .component("node-a", StateComponent::Network, Health::Healthy);

        let actions = &world.evaluate(&SentinelAgentFailure)[0].recommended_actions;
        assert!(actions.iter().any(|a| a.contains("systemctl status sentinel-agent")));
        for action in actions {
            assert!(!action.contains("restart"), "recommended a mutating command: {action}");
        }
    }

    // --- SSH_SERVICE_FAILURE ----------------------------------------------

    #[test]
    fn a_dead_sshd_on_a_healthy_host_is_a_lockout_not_an_outage() {
        let world = World::new("node-a")
            .component("node-a", StateComponent::Ssh, Health::Unavailable)
            .component("node-a", StateComponent::Network, Health::Healthy)
            .component("node-a", StateComponent::Agent, Health::Healthy);

        let diagnoses = world.evaluate(&SshServiceFailure);
        assert_eq!(diagnoses.len(), 1);
        assert!(diagnoses[0].is(kind::SSH_SERVICE_FAILURE));
        assert_eq!(
            diagnoses[0].confidence,
            Confidence::High,
            "the agent confirms the host is up"
        );
        assert!(
            diagnoses[0].summary.contains("lockout rather than an outage"),
            "{}",
            diagnoses[0].summary
        );
    }

    #[test]
    fn ssh_failing_on_a_host_with_no_agent_is_diagnosed_with_less_confidence() {
        // Reachability alone is weaker evidence than a running agent.
        let world = World::new("node-a")
            .component("node-a", StateComponent::Ssh, Health::Unavailable)
            .component("node-a", StateComponent::Network, Health::Healthy);

        let diagnoses = world.evaluate(&SshServiceFailure);
        assert_eq!(diagnoses.len(), 1);
        assert_eq!(diagnoses[0].confidence, Confidence::Medium);
    }

    #[test]
    fn an_unreachable_host_is_not_reported_as_an_ssh_failure() {
        let world = World::new("node-a")
            .component("node-a", StateComponent::Ssh, Health::Unavailable)
            .component("node-a", StateComponent::Network, Health::Unavailable);

        assert!(world.evaluate(&SshServiceFailure).is_empty());
    }

    #[test]
    fn the_two_service_rules_tell_different_stories_about_different_faults() {
        // The distinction the whole milestone rests on.
        let agent_down = World::new("node-a")
            .component("node-a", StateComponent::Agent, Health::Unavailable)
            .component("node-a", StateComponent::Ssh, Health::Healthy)
            .component("node-a", StateComponent::Network, Health::Healthy);

        assert_eq!(agent_down.evaluate(&SentinelAgentFailure).len(), 1);
        assert!(agent_down.evaluate(&SshServiceFailure).is_empty());

        let ssh_down = World::new("node-a")
            .component("node-a", StateComponent::Ssh, Health::Unavailable)
            .component("node-a", StateComponent::Agent, Health::Healthy)
            .component("node-a", StateComponent::Network, Health::Healthy);

        assert_eq!(ssh_down.evaluate(&SshServiceFailure).len(), 1);
        assert!(ssh_down.evaluate(&SentinelAgentFailure).is_empty());
    }

    #[test]
    fn a_degraded_service_is_diagnosed_too() {
        // An sshd that accepts connections and then says nothing is broken in
        // a way that matters, even though something is listening.
        let world = World::new("node-a")
            .component("node-a", StateComponent::Ssh, Health::Degraded)
            .component("node-a", StateComponent::Network, Health::Healthy)
            .component("node-a", StateComponent::Agent, Health::Healthy);

        assert_eq!(world.evaluate(&SshServiceFailure).len(), 1);
    }

    #[test]
    fn a_host_nobody_has_observed_is_not_diagnosed() {
        let world = World::new("node-a");
        assert!(world.evaluate(&SentinelAgentFailure).is_empty());
        assert!(world.evaluate(&SshServiceFailure).is_empty());
    }
}
