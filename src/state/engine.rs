//! Folding observations into entity state.
//!
//! The engine is deliberately ignorant of what any probe measures. An
//! integration *declares* which state component its probe informs, and the
//! engine does the debouncing and roll-up. Adding a Ceph probe is a
//! registration, not a change here (SPEC.md §57).

use std::collections::{BTreeMap, HashMap};

use crate::entity::EntityId;
use crate::observation::{Observation, ProbeStatus};
use crate::probes::ProbeId;

use super::{ComponentState, DebouncePolicy, Debouncer, EntityState, Health, StateComponent, StateTransition};

/// How one probe's results feed into state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProbeMapping {
    /// Which component this probe informs.
    pub component: StateComponent,
    /// How much evidence is needed before health changes.
    pub policy: DebouncePolicy,
}

impl ProbeMapping {
    /// A mapping with the default debounce policy.
    pub fn new(component: StateComponent) -> Self {
        Self {
            component,
            policy: DebouncePolicy::default(),
        }
    }

    /// A mapping that reacts to the first observation.
    ///
    /// For probes whose result is an authoritative statement rather than a
    /// measurement: a scheduler reporting DRAIN is not a flaky ping
    /// (IMPLEMENTATION.md §70).
    pub fn immediate(component: StateComponent) -> Self {
        Self {
            component,
            policy: DebouncePolicy::immediate(),
        }
    }

    /// Builder: use a specific policy.
    pub fn with_policy(mut self, policy: DebouncePolicy) -> Self {
        self.policy = policy;
        self
    }
}

/// Derives [`EntityState`] from a stream of observations.
#[derive(Debug, Default)]
pub struct StateEngine {
    mappings: BTreeMap<ProbeId, ProbeMapping>,
    debouncers: HashMap<(EntityId, StateComponent), Debouncer>,
    states: BTreeMap<EntityId, EntityState>,
}

impl StateEngine {
    /// An engine that knows about no probes yet.
    pub fn new() -> Self {
        Self::default()
    }

    /// Declare how a probe's results map onto a state component.
    pub fn register(&mut self, probe_id: impl Into<ProbeId>, mapping: ProbeMapping) -> &mut Self {
        self.mappings.insert(probe_id.into(), mapping);
        self
    }

    /// Whether a probe has been registered.
    pub fn knows(&self, probe_id: &ProbeId) -> bool {
        self.mappings.contains_key(probe_id)
    }

    /// Fold one observation in, returning any state change it caused.
    ///
    /// An observation from an unregistered probe is stored as history by the
    /// caller but changes no state: an integration that has not said what its
    /// probe means must not be guessed at.
    pub fn ingest(&mut self, observation: &Observation) -> Option<StateTransition> {
        let mapping = self.mappings.get(&observation.probe_id)?.clone();
        let entity = observation.target_entity;
        let component = mapping.component;

        let state = self
            .states
            .entry(entity)
            .or_insert_with(|| EntityState::unknown(entity));
        let previous = state.component(component);

        if !observation.status.is_conclusive() {
            // "This entity has no GPUs" is a fact about applicability, not a
            // measurement, so it bypasses the debouncer entirely.
            if previous == Health::NotApplicable {
                return None;
            }
            state.set_component(component, ComponentState::new(Health::NotApplicable));
            return Some(StateTransition::new(
                entity,
                Some(component),
                previous,
                Health::NotApplicable,
            ));
        }

        let debouncer = self
            .debouncers
            .entry((entity, component))
            .or_insert_with(|| Debouncer::starting_at(mapping.policy, previous));

        debouncer.observe(is_bad(observation.status));

        // The debouncer knows *how persistent* the problem is; the status knows
        // *how severe* it can be. A slow filesystem reported three times is
        // still degraded, not unavailable.
        let health = cap(debouncer.health(), observation.status);

        if health == previous {
            return None;
        }

        let mut component_state = ComponentState::new(health).with_evidence([observation.id]);
        component_state.consecutive_failures = debouncer.consecutive_failures();
        component_state.consecutive_successes = debouncer.consecutive_successes();
        state.set_component(component, component_state);

        Some(StateTransition::new(entity, Some(component), previous, health).with_evidence([observation.id]))
    }

    /// Fold in many observations, in the order given.
    pub fn ingest_all<'a>(&mut self, observations: impl IntoIterator<Item = &'a Observation>) -> Vec<StateTransition> {
        observations.into_iter().filter_map(|o| self.ingest(o)).collect()
    }

    /// The state of one entity, if any observation has ever mentioned it.
    pub fn state(&self, entity: EntityId) -> Option<&EntityState> {
        self.states.get(&entity)
    }

    /// Every derived state, in entity order.
    pub fn states(&self) -> impl Iterator<Item = &EntityState> {
        self.states.values()
    }

    /// One entity's state, mutably, so a diagnosis can attach a classification.
    ///
    /// Classifications come from rules rather than probes, because only a rule
    /// has weighed enough evidence to justify one.
    pub fn state_mut(&mut self, entity: EntityId) -> Option<&mut EntityState> {
        self.states.get_mut(&entity)
    }

    /// Seed an entity's state, for rehydrating from the database.
    pub fn seed(&mut self, state: EntityState) {
        for (component, component_state) in &state.components {
            let policy = self
                .mappings
                .values()
                .find(|m| m.component == *component)
                .map(|m| m.policy)
                .unwrap_or_default();
            self.debouncers.insert(
                (state.entity, *component),
                Debouncer::starting_at(policy, component_state.health),
            );
        }
        self.states.insert(state.entity, state);
    }
}

/// Whether a status should count against the entity in the debouncer.
fn is_bad(status: ProbeStatus) -> bool {
    !matches!(status, ProbeStatus::Ok)
}

/// The worst health a given status can justify.
fn cap(health: Health, status: ProbeStatus) -> Health {
    let ceiling = match status {
        ProbeStatus::Ok => return health,
        ProbeStatus::Degraded => Health::Degraded,
        _ => Health::Unavailable,
    };
    if health.severity_rank() > ceiling.severity_rank() {
        ceiling
    } else {
        health
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::entity::{EntityKey, EntityType};

    fn entity(name: &str) -> EntityId {
        EntityKey::new("lab", EntityType::Host, name).entity_id()
    }

    fn observation(probe: &str, target: EntityId, status: ProbeStatus) -> Observation {
        Observation::new(ProbeId::new(probe), target, status)
    }

    fn engine_with(probe: &str, mapping: ProbeMapping) -> StateEngine {
        let mut engine = StateEngine::new();
        engine.register(probe, mapping);
        engine
    }

    #[test]
    fn an_unregistered_probe_changes_no_state() {
        // Guessing what an undeclared probe means would be worse than ignoring
        // it: the guess would be invisible and wrong.
        let mut engine = StateEngine::new();
        assert!(engine
            .ingest(&observation("mystery", entity("a"), ProbeStatus::Failed))
            .is_none());
        assert!(engine.state(entity("a")).is_none());
    }

    #[test]
    fn a_registered_probe_drives_its_declared_component() {
        let mut engine = engine_with("ssh.tcp", ProbeMapping::immediate(StateComponent::Ssh));
        let transition = engine
            .ingest(&observation("ssh.tcp", entity("a"), ProbeStatus::Ok))
            .expect("transition");

        assert_eq!(transition.component, Some(StateComponent::Ssh));
        assert_eq!(transition.from, Health::NotApplicable);
        assert_eq!(transition.to, Health::Healthy);
        assert_eq!(
            engine.state(entity("a")).unwrap().component(StateComponent::Ssh),
            Health::Healthy
        );
    }

    #[test]
    fn debouncing_holds_state_until_the_threshold_is_met() {
        let mut engine = engine_with("net.tcp", ProbeMapping::new(StateComponent::Network));
        for _ in 0..2 {
            engine.ingest(&observation("net.tcp", entity("a"), ProbeStatus::Ok));
        }
        assert_eq!(
            engine.state(entity("a")).unwrap().component(StateComponent::Network),
            Health::Healthy
        );

        assert!(
            engine
                .ingest(&observation("net.tcp", entity("a"), ProbeStatus::Failed))
                .is_none(),
            "one failure is not an outage"
        );
        let transition = engine
            .ingest(&observation("net.tcp", entity("a"), ProbeStatus::Failed))
            .expect("second failure warns");
        assert_eq!(transition.to, Health::Degraded);

        let transition = engine
            .ingest(&observation("net.tcp", entity("a"), ProbeStatus::Failed))
            .expect("third failure escalates");
        assert_eq!(transition.to, Health::Unavailable);
    }

    #[test]
    fn a_persistently_degraded_result_never_escalates_to_unavailable() {
        // Slow storage stays slow storage, however long it lasts.
        let mut engine = engine_with("nfs.latency", ProbeMapping::new(StateComponent::Storage));
        for _ in 0..10 {
            engine.ingest(&observation("nfs.latency", entity("a"), ProbeStatus::Degraded));
        }
        assert_eq!(
            engine.state(entity("a")).unwrap().component(StateComponent::Storage),
            Health::Degraded
        );
    }

    #[test]
    fn an_immediate_mapping_reacts_to_a_single_authoritative_report() {
        // A scheduler saying DRAIN is a statement, not a flaky measurement.
        let mut engine = engine_with("slurm.node", ProbeMapping::immediate(StateComponent::Scheduler));
        let transition = engine
            .ingest(&observation("slurm.node", entity("a"), ProbeStatus::Degraded))
            .expect("transition");
        assert_eq!(transition.to, Health::Degraded);
    }

    #[test]
    fn a_not_applicable_result_leaves_the_component_inapplicable_and_reports_no_change() {
        // An unrecorded component already reads as inapplicable, so learning
        // that a node has no GPUs is not a state change to report.
        let mut engine = engine_with("gpu.count", ProbeMapping::new(StateComponent::Accelerator));
        assert!(engine
            .ingest(&observation("gpu.count", entity("a"), ProbeStatus::NotApplicable))
            .is_none());

        let state = engine.state(entity("a")).expect("entity is known");
        assert_eq!(state.component(StateComponent::Accelerator), Health::NotApplicable);
        assert_eq!(
            state.overall,
            Health::Unknown,
            "an inapplicable component must not make the entity look healthy"
        );
    }

    #[test]
    fn a_component_that_stops_applying_transitions_out_of_its_old_health() {
        // GPUs removed from a node: the accelerator component must stop
        // reporting the stale verdict rather than freezing on it.
        let mut engine = engine_with("gpu.count", ProbeMapping::immediate(StateComponent::Accelerator));
        engine.ingest(&observation("gpu.count", entity("a"), ProbeStatus::Failed));
        assert_eq!(
            engine
                .state(entity("a"))
                .unwrap()
                .component(StateComponent::Accelerator),
            Health::Unavailable
        );

        let transition = engine
            .ingest(&observation("gpu.count", entity("a"), ProbeStatus::NotApplicable))
            .expect("transition out of unavailable");
        assert_eq!(transition.from, Health::Unavailable);
        assert_eq!(transition.to, Health::NotApplicable);
    }

    #[test]
    fn components_are_tracked_independently_per_entity() {
        let mut engine = StateEngine::new();
        engine.register("ssh.tcp", ProbeMapping::immediate(StateComponent::Ssh));
        engine.register("net.tcp", ProbeMapping::immediate(StateComponent::Network));

        engine.ingest(&observation("ssh.tcp", entity("a"), ProbeStatus::Failed));
        engine.ingest(&observation("net.tcp", entity("a"), ProbeStatus::Ok));
        engine.ingest(&observation("ssh.tcp", entity("b"), ProbeStatus::Ok));

        let a = engine.state(entity("a")).expect("a");
        assert_eq!(a.component(StateComponent::Ssh), Health::Unavailable);
        assert_eq!(a.component(StateComponent::Network), Health::Healthy);
        assert_eq!(a.overall, Health::Unavailable);

        assert_eq!(engine.state(entity("b")).unwrap().overall, Health::Healthy);
    }

    #[test]
    fn recovery_requires_the_configured_number_of_successes() {
        let mut engine = engine_with("net.tcp", ProbeMapping::new(StateComponent::Network));
        for _ in 0..3 {
            engine.ingest(&observation("net.tcp", entity("a"), ProbeStatus::Failed));
        }
        assert_eq!(
            engine.state(entity("a")).unwrap().component(StateComponent::Network),
            Health::Unavailable
        );

        assert!(engine
            .ingest(&observation("net.tcp", entity("a"), ProbeStatus::Ok))
            .is_none());
        let transition = engine
            .ingest(&observation("net.tcp", entity("a"), ProbeStatus::Ok))
            .expect("recovered");
        assert_eq!(transition.to, Health::Healthy);
    }

    #[test]
    fn transitions_carry_the_observation_that_caused_them() {
        let mut engine = engine_with("ssh.tcp", ProbeMapping::immediate(StateComponent::Ssh));
        let observation = observation("ssh.tcp", entity("a"), ProbeStatus::Failed);
        let transition = engine.ingest(&observation).expect("transition");
        assert_eq!(transition.evidence, vec![observation.id]);
    }

    #[test]
    fn seeded_state_is_resumed_rather_than_relearned() {
        // After a controller restart, a host that was already down must not
        // need three fresh failures to be considered down again.
        let mut engine = engine_with("net.tcp", ProbeMapping::new(StateComponent::Network));
        let mut state = EntityState::unknown(entity("a"));
        state.set_component(StateComponent::Network, ComponentState::new(Health::Unavailable));
        engine.seed(state);

        assert_eq!(
            engine.state(entity("a")).unwrap().component(StateComponent::Network),
            Health::Unavailable
        );
        assert!(engine
            .ingest(&observation("net.tcp", entity("a"), ProbeStatus::Ok))
            .is_none());
        let transition = engine
            .ingest(&observation("net.tcp", entity("a"), ProbeStatus::Ok))
            .expect("recovered");
        assert_eq!(transition.from, Health::Unavailable);
    }

    #[test]
    fn ingesting_a_batch_returns_only_the_actual_changes() {
        let mut engine = engine_with("ssh.tcp", ProbeMapping::immediate(StateComponent::Ssh));
        let observations = vec![
            observation("ssh.tcp", entity("a"), ProbeStatus::Ok),
            observation("ssh.tcp", entity("a"), ProbeStatus::Ok),
            observation("ssh.tcp", entity("a"), ProbeStatus::Failed),
        ];
        let transitions = engine.ingest_all(&observations);
        assert_eq!(transitions.len(), 2, "healthy -> healthy is not a change");
    }
}
