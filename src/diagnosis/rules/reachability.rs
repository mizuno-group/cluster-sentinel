//! Reachability rules: what several viewpoints together can justify.
//!
//! This is the file the whole distributed design exists for. With one observer,
//! "I cannot reach it" is ambiguous between a dead host and a broken path, and
//! the two demand completely different responses. With several *independent*
//! observers the ambiguity resolves:
//!
//! | Evidence | Conclusion |
//! | --- | --- |
//! | Every independent observer fails | `HOST_UNREACHABLE`, confidence high |
//! | Some fail, others succeed | `PATH_SPECIFIC_NETWORK_FAILURE` |
//! | One observer fails, no second opinion | **nothing** — say so, do not guess |
//!
//! Two limits are enforced here rather than left to good intentions:
//!
//! * **Never `POWER_OFF`.** Network silence is not evidence about power. The
//!   strongest conclusion available from network probes is that nothing
//!   answers, and out-of-band evidence is not something v1 collects
//!   (SPEC.md §52).
//! * **Never `Confirmed`.** Peer reachability tops out at `High`
//!   (IMPLEMENTATION.md §73). A host that is off, a host whose NIC has died and
//!   a host behind a failed switch look identical from here.

use std::collections::{BTreeMap, BTreeSet};

use crate::diagnosis::{kind, Confidence, Diagnosis, DiagnosisContext, DiagnosisRule, RuleId};
use crate::entity::{EntityId, EntityType};
use crate::observation::Observation;
use crate::probes::network::PROBE_ID as NETWORK_PROBE;

/// What the observers of one target saw.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Verdicts {
    /// Observers that reached the target.
    pub reached: BTreeSet<EntityId>,
    /// Observers whose connection was actually completed, as opposed to
    /// refused.
    ///
    /// A refusal proves the host is alive, which is all `reached` claims. It
    /// does not prove the path carries a working connection, and the
    /// difference matters when nothing is listening on the port being probed:
    /// then a refusal and a timeout are the same non-event, told apart only by
    /// whether the firewall in between rejects or drops.
    pub connected: BTreeSet<EntityId>,
    /// Observers that did not.
    pub failed: BTreeSet<EntityId>,
}

impl Verdicts {
    /// How many observers expressed a view.
    pub fn total(&self) -> usize {
        self.reached.len() + self.failed.len()
    }

    /// Whether every observer failed, and there was more than one.
    ///
    /// More than one is the whole point: a single failing observer is exactly
    /// the ambiguous case this rule refuses to resolve.
    pub fn unanimously_unreachable(&self) -> bool {
        self.failed.len() >= 2 && self.reached.is_empty()
    }

    /// Whether observers disagree, which localises the fault to a path.
    pub fn disagree(&self) -> bool {
        !self.failed.is_empty() && !self.reached.is_empty()
    }

    /// Whether the disagreement is about a working connection.
    ///
    /// Without at least one completed connection there is nothing listening on
    /// the probed port anywhere, and the split between observers says only
    /// that some firewalls answer a closed port and others drop it. That is
    /// configuration, not a fault, and a cluster was told one of its head
    /// nodes had a broken network path because of it.
    pub fn disagree_about_a_working_path(&self) -> bool {
        self.disagree() && !self.connected.is_empty()
    }
}

/// Collect the observers' latest verdicts on each target.
///
/// Only *remote* observations count. A host's own report that it can reach
/// itself is not a second opinion.
pub fn verdicts_by_target(context: &DiagnosisContext) -> BTreeMap<EntityId, Verdicts> {
    let mut by_target: BTreeMap<EntityId, Verdicts> = BTreeMap::new();

    for host in context.entities_of_type(EntityType::Host) {
        let mut verdicts = Verdicts::default();

        for observation in context.observations.for_entity(host.id) {
            if observation.probe_id.as_str() != NETWORK_PROBE {
                continue;
            }
            let Some(observer) = observation.observer_entity else {
                continue;
            };
            if observer == host.id {
                continue;
            }

            if reached(observation) {
                verdicts.reached.insert(observer);
                if connected(observation) {
                    verdicts.connected.insert(observer);
                }
            } else {
                verdicts.failed.insert(observer);
            }
        }

        if verdicts.total() > 0 {
            by_target.insert(host.id, verdicts);
        }
    }

    by_target
}

/// Whether an observation says the target answered.
///
/// A refusal counts as reached: something at that address replied (ADR 0004).
fn reached(observation: &Observation) -> bool {
    if let Some(responded) = observation.payload.get("host_responded").and_then(|v| v.as_bool()) {
        return responded;
    }
    !observation.status.is_bad()
}

/// Whether an observation says the connection actually completed.
///
/// Distinct from [`reached`]: that asks whether anything is at the address,
/// this asks whether the path carries a connection.
fn connected(observation: &Observation) -> bool {
    observation
        .payload
        .get("outcome")
        .and_then(|v| v.as_str())
        .map(|outcome| outcome == "connected")
        // Older observations, recorded before the outcome was written into the
        // payload, cannot answer this. Treating them as connected keeps the
        // rule's previous behaviour for them rather than silently disabling it.
        .unwrap_or_else(|| !observation.status.is_bad())
}

/// Evidence ids for a target.
fn evidence_for(context: &DiagnosisContext, target: EntityId) -> Vec<crate::observation::ObservationId> {
    context
        .observations
        .for_entity(target)
        .into_iter()
        .map(|o| o.id)
        .collect()
}

/// Nothing can reach the host.
pub struct HostUnreachable;

impl DiagnosisRule for HostUnreachable {
    fn id(&self) -> RuleId {
        RuleId::new("reachability.host_unreachable")
    }

    fn description(&self) -> &str {
        "no independent observer can reach the host"
    }

    fn evaluate(&self, context: &DiagnosisContext) -> Vec<Diagnosis> {
        let mut diagnoses = Vec::new();

        for (target, verdicts) in verdicts_by_target(context) {
            if !verdicts.unanimously_unreachable() {
                continue;
            }

            let Some(entity) = context.entity(target) else {
                continue;
            };

            diagnoses.push(
                Diagnosis::new(kind::HOST_UNREACHABLE, self.id(), Confidence::High)
                    .affecting([target])
                    .rooted_at([target])
                    .with_evidence(evidence_for(context, target))
                    .with_summary(format!(
                        "{} is unreachable from all {} observers",
                        entity.canonical_name,
                        verdicts.failed.len()
                    ))
                    .recommending(vec![
                        format!("sentinel entity show {}", entity.canonical_name),
                        // Deliberately phrased as a question, not a conclusion.
                        // Sentinel cannot see power state and does not pretend
                        // to (SPEC.md §52).
                        format!("check console or BMC for {}", entity.canonical_name),
                    ]),
            );
        }

        diagnoses
    }
}

/// Some observers reach the host and others do not.
pub struct PathSpecificNetworkFailure;

impl DiagnosisRule for PathSpecificNetworkFailure {
    fn id(&self) -> RuleId {
        RuleId::new("reachability.path_failure")
    }

    fn description(&self) -> &str {
        "some observers reach the host and others do not, so the path is at fault"
    }

    fn evaluate(&self, context: &DiagnosisContext) -> Vec<Diagnosis> {
        let mut diagnoses = Vec::new();

        for (target, verdicts) in verdicts_by_target(context) {
            if !verdicts.disagree_about_a_working_path() {
                continue;
            }

            let Some(entity) = context.entity(target) else {
                continue;
            };

            let blind: Vec<String> = verdicts
                .failed
                .iter()
                .filter_map(|id| context.entity(*id))
                .map(|e| e.canonical_name.clone())
                .collect();
            let seeing: Vec<String> = verdicts
                .connected
                .iter()
                .filter_map(|id| context.entity(*id))
                .map(|e| e.canonical_name.clone())
                .collect();

            // The suspected fault is the path, so both ends are implicated and
            // neither is blamed outright.
            let mut roots: Vec<EntityId> = verdicts.failed.iter().copied().collect();
            roots.push(target);

            diagnoses.push(
                Diagnosis::new(kind::PATH_SPECIFIC_NETWORK_FAILURE, self.id(), Confidence::High)
                    .affecting([target])
                    .rooted_at(roots)
                    .with_evidence(evidence_for(context, target))
                    .with_summary(format!(
                        "{} is unreachable from {} but reachable from {}; the host is up and the path is at fault",
                        entity.canonical_name,
                        blind.join(", "),
                        seeing.join(", ")
                    ))
                    .recommending(vec![
                        format!("sentinel entity show {}", entity.canonical_name),
                        "sentinel peers".to_string(),
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
    use crate::observation::ProbeStatus;
    use crate::probes::ProbeId;
    use crate::state::EntityState;
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
        fn new(hosts: &[&str]) -> Self {
            let mut inventory = Inventory::new();
            for name in hosts {
                inventory.insert_entity(
                    ManagedEntity::new("lab", EntityType::Host, *name)
                        .with_capabilities(CapabilitySet::from_iter(["network.tcp", "observer.peer"])),
                );
            }
            Self {
                inventory,
                states: HashMap::new(),
                observations: ObservationIndex::new(),
            }
        }

        /// Record what one observer saw of one target.
        fn saw(self, observer: &str, target: &str, responded: bool) -> Self {
            let outcome = if responded { "connected" } else { "timed_out" };
            self.saw_outcome(observer, target, outcome)
        }

        /// Record a specific connection outcome, which is what tells a
        /// completed connection from a refusal.
        fn saw_outcome(mut self, observer: &str, target: &str, outcome: &str) -> Self {
            let responded = matches!(outcome, "connected" | "refused");
            let status = if responded {
                ProbeStatus::Ok
            } else {
                ProbeStatus::Timeout
            };
            self.observations.insert(
                Observation::new(ProbeId::new(NETWORK_PROBE), host(target), status)
                    .with_observer(host(observer))
                    .with_payload(serde_json::json!({"host_responded": responded, "outcome": outcome})),
            );
            self
        }

        /// A local (non-remote) observation, which must not count as a view.
        fn self_reported(mut self, target: &str, responded: bool) -> Self {
            let status = if responded {
                ProbeStatus::Ok
            } else {
                ProbeStatus::Timeout
            };
            self.observations.insert(
                Observation::new(ProbeId::new(NETWORK_PROBE), host(target), status)
                    .with_payload(serde_json::json!({"host_responded": responded})),
            );
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

    #[test]
    fn all_observers_failing_justifies_host_unreachable() {
        // SPEC.md §51.
        let world = World::new(&["target", "a", "b", "c"])
            .saw("a", "target", false)
            .saw("b", "target", false)
            .saw("c", "target", false);

        let diagnoses = world.evaluate(&HostUnreachable);
        assert_eq!(diagnoses.len(), 1);
        assert!(diagnoses[0].is(kind::HOST_UNREACHABLE));
        assert_eq!(diagnoses[0].confidence, Confidence::High);
    }

    #[test]
    fn one_observer_failing_alone_justifies_nothing() {
        // SPEC.md §175 and the single most important restraint in this file.
        // With one viewpoint, a dead host and a broken path are the same
        // picture, and guessing sends someone to the wrong place.
        let world = World::new(&["target", "a"]).saw("a", "target", false);

        assert!(world.evaluate(&HostUnreachable).is_empty());
        assert!(world.evaluate(&PathSpecificNetworkFailure).is_empty());
    }

    #[test]
    fn refusals_on_a_closed_port_are_not_a_path_fault() {
        // Found on a live cluster. SSH had been moved off 22, so the probe
        // knocked on a port nothing was listening on. Two observers' kernels
        // sent RST -- "refused", which counts as reached, because a dead host
        // does not send RST. A third sat behind a firewall that drops instead,
        // and timed out. That split is firewall policy on a closed port, and
        // it was reported as a broken network path to the head node, at
        // CRITICAL, for as long as the port stayed wrong.
        let world = World::new(&["target", "a", "b", "c"])
            .saw_outcome("a", "target", "refused")
            .saw_outcome("b", "target", "refused")
            .saw_outcome("c", "target", "timed_out");

        assert!(
            world.evaluate(&PathSpecificNetworkFailure).is_empty(),
            "nothing was listening anywhere; there is no working path to be missing"
        );
        assert!(world.evaluate(&HostUnreachable).is_empty(), "the host plainly answered");
    }

    #[test]
    fn one_completed_connection_is_enough_to_localise_a_path_fault() {
        // The other side of the same line: once something has actually
        // connected, an observer that cannot is a real difference.
        let world = World::new(&["target", "a", "b", "c"])
            .saw_outcome("a", "target", "connected")
            .saw_outcome("b", "target", "refused")
            .saw_outcome("c", "target", "timed_out");

        let diagnoses = world.evaluate(&PathSpecificNetworkFailure);
        assert_eq!(diagnoses.len(), 1, "{diagnoses:#?}");
        assert!(diagnoses[0].is(kind::PATH_SPECIFIC_NETWORK_FAILURE));
    }

    #[test]
    fn observers_disagreeing_localises_the_fault_to_the_path() {
        // SPEC.md §50: the controller cannot see it, but its peers can.
        let world = World::new(&["target", "controller", "b", "c"])
            .saw("controller", "target", false)
            .saw("b", "target", true)
            .saw("c", "target", true);

        assert!(
            world.evaluate(&HostUnreachable).is_empty(),
            "the host is demonstrably up"
        );

        let diagnoses = world.evaluate(&PathSpecificNetworkFailure);
        assert_eq!(diagnoses.len(), 1);
        assert!(diagnoses[0].is(kind::PATH_SPECIFIC_NETWORK_FAILURE));
        assert!(diagnoses[0].summary.contains("controller"), "{}", diagnoses[0].summary);
        assert!(
            diagnoses[0].summary.contains("the host is up"),
            "{}",
            diagnoses[0].summary
        );
    }

    #[test]
    fn the_two_reachability_rules_never_fire_together() {
        for (name, world) in [
            (
                "unreachable",
                World::new(&["target", "a", "b"])
                    .saw("a", "target", false)
                    .saw("b", "target", false),
            ),
            (
                "path failure",
                World::new(&["target", "a", "b"])
                    .saw("a", "target", false)
                    .saw("b", "target", true),
            ),
        ] {
            let unreachable = !world.evaluate(&HostUnreachable).is_empty();
            let path = !world.evaluate(&PathSpecificNetworkFailure).is_empty();
            assert!(unreachable != path, "{name}: exactly one should fire");
        }
    }

    #[test]
    fn a_host_reaching_itself_is_not_a_second_opinion() {
        // Otherwise a host could vouch for its own reachability and defeat the
        // quorum entirely.
        let world = World::new(&["target", "a", "b"])
            .saw("a", "target", false)
            .saw("b", "target", false)
            .self_reported("target", true);

        assert_eq!(
            world.evaluate(&HostUnreachable).len(),
            1,
            "a self-report must not count"
        );
    }

    #[test]
    fn a_refusal_counts_as_reached() {
        // ADR 0004: something replied, so the path works even though the
        // service does not.
        let world = World::new(&["target", "a", "b"])
            .saw("a", "target", true)
            .saw("b", "target", true);
        assert!(world.evaluate(&HostUnreachable).is_empty());
    }

    #[test]
    fn a_healthy_host_produces_nothing() {
        let world = World::new(&["target", "a", "b", "c"])
            .saw("a", "target", true)
            .saw("b", "target", true)
            .saw("c", "target", true);

        assert!(world.evaluate(&HostUnreachable).is_empty());
        assert!(world.evaluate(&PathSpecificNetworkFailure).is_empty());
    }

    #[test]
    fn a_host_nobody_has_observed_is_not_diagnosed() {
        let world = World::new(&["target", "a"]);
        assert!(world.evaluate(&HostUnreachable).is_empty());
    }

    #[test]
    fn unreachability_never_claims_a_power_state() {
        // SPEC.md §52. A host that is off, a host with a dead NIC and a host
        // behind a failed switch are indistinguishable from here.
        let world = World::new(&["target", "a", "b"])
            .saw("a", "target", false)
            .saw("b", "target", false);

        let diagnosis = &world.evaluate(&HostUnreachable)[0];
        assert!(
            diagnosis.confidence < Confidence::Confirmed,
            "network evidence cannot confirm"
        );

        let rendered = format!(
            "{} {} {:?}",
            diagnosis.diagnosis_type, diagnosis.summary, diagnosis.recommended_actions
        );
        for claim in ["POWER_OFF", "powered off", "is off", "power is"] {
            assert!(!rendered.contains(claim), "must not claim a power state: {rendered}");
        }
    }

    #[test]
    fn the_recommended_action_asks_rather_than_asserts() {
        let world = World::new(&["target", "a", "b"])
            .saw("a", "target", false)
            .saw("b", "target", false);
        let actions = &world.evaluate(&HostUnreachable)[0].recommended_actions;

        assert!(
            actions.iter().any(|a| a.contains("check console or BMC")),
            "{actions:?}"
        );
        for action in actions {
            for mutating in ["reboot", "power on", "ipmitool power", "restart"] {
                assert!(!action.contains(mutating), "recommended a mutating command: {action}");
            }
        }
    }

    #[test]
    fn several_targets_are_diagnosed_independently() {
        let world = World::new(&["t1", "t2", "a", "b"])
            .saw("a", "t1", false)
            .saw("b", "t1", false)
            .saw("a", "t2", true)
            .saw("b", "t2", true);

        let diagnoses = world.evaluate(&HostUnreachable);
        assert_eq!(diagnoses.len(), 1);
        assert_eq!(diagnoses[0].affected_entities, vec![host("t1")]);
    }
}
