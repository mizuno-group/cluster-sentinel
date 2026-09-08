//! Derived state.
//!
//! State sits between raw observations and diagnosis. It answers "is this
//! component healthy right now", with debouncing so that one dropped packet is
//! not an outage (IMPLEMENTATION.md §70).
//!
//! It deliberately does **not** answer "why" — that is the diagnosis engine.

mod debounce;
mod engine;

pub use debounce::{DebouncePolicy, Debouncer};
pub use engine::{ProbeMapping, StateEngine};

use std::collections::BTreeMap;
use std::fmt;

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::entity::EntityId;
use crate::observation::ObservationId;
use crate::time::{now, Timestamp};

/// The health facets tracked per entity (IMPLEMENTATION.md §69).
///
/// Every component supports [`Health::NotApplicable`]: a fileserver has no
/// scheduler state and a switch has no agent.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StateComponent {
    /// Can the entity be reached at all, from anywhere.
    Availability,
    /// Network path health.
    Network,
    /// Host-level health (load, memory, pressure).
    Host,
    /// The Sentinel agent itself.
    Agent,
    /// SSH service.
    Ssh,
    /// Other services on the entity.
    Service,
    /// Scheduler's view of the entity.
    Scheduler,
    /// Storage the entity provides or consumes.
    Storage,
    /// GPUs and other accelerators.
    Accelerator,
    /// Clock agreement with the controller.
    Clock,
}

impl StateComponent {
    /// Every component, in a stable order.
    pub const ALL: [StateComponent; 10] = [
        StateComponent::Availability,
        StateComponent::Network,
        StateComponent::Host,
        StateComponent::Agent,
        StateComponent::Ssh,
        StateComponent::Service,
        StateComponent::Scheduler,
        StateComponent::Storage,
        StateComponent::Accelerator,
        StateComponent::Clock,
    ];

    /// Stable string form used in the database.
    pub fn as_str(&self) -> &'static str {
        match self {
            StateComponent::Availability => "availability",
            StateComponent::Network => "network",
            StateComponent::Host => "host",
            StateComponent::Agent => "agent",
            StateComponent::Ssh => "ssh",
            StateComponent::Service => "service",
            StateComponent::Scheduler => "scheduler",
            StateComponent::Storage => "storage",
            StateComponent::Accelerator => "accelerator",
            StateComponent::Clock => "clock",
        }
    }

    /// Parse the stable string form.
    pub fn parse(s: &str) -> Option<Self> {
        StateComponent::ALL.into_iter().find(|c| c.as_str() == s)
    }
}

impl fmt::Display for StateComponent {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Health of one component, or of an entity overall (SPEC.md §87).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Health {
    /// Working as expected.
    Healthy,
    /// Working, but not correctly or not fully.
    Degraded,
    /// Not usable.
    Unavailable,
    /// Under an operator-declared maintenance window.
    Maintenance,
    /// Not enough information to say.
    Unknown,
    /// This component does not exist for this entity.
    NotApplicable,
}

impl Health {
    /// Stable string form used in the database.
    pub fn as_str(&self) -> &'static str {
        match self {
            Health::Healthy => "healthy",
            Health::Degraded => "degraded",
            Health::Unavailable => "unavailable",
            Health::Maintenance => "maintenance",
            Health::Unknown => "unknown",
            Health::NotApplicable => "not_applicable",
        }
    }

    /// Parse the stable string form.
    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "healthy" => Health::Healthy,
            "degraded" => Health::Degraded,
            "unavailable" => Health::Unavailable,
            "maintenance" => Health::Maintenance,
            "unknown" => Health::Unknown,
            "not_applicable" => Health::NotApplicable,
            _ => return None,
        })
    }

    /// How alarming this health is, for rolling components up into an overall
    /// verdict. `NotApplicable` ranks lowest so it never colours a summary.
    pub fn severity_rank(&self) -> u8 {
        match self {
            Health::NotApplicable => 0,
            Health::Healthy => 1,
            Health::Maintenance => 2,
            Health::Unknown => 3,
            Health::Degraded => 4,
            Health::Unavailable => 5,
        }
    }

    /// Whether this health represents something an operator should look at.
    pub fn is_problem(&self) -> bool {
        matches!(self, Health::Degraded | Health::Unavailable)
    }
}

impl fmt::Display for Health {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A short machine-readable label refining the overall health, e.g.
/// `SCHEDULER_DEGRADED` (SPEC.md §88). Kept as a string so integrations can add
/// their own without touching a core enum.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Classification(String);

impl Classification {
    /// Build a classification.
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    /// The classification as a string.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for Classification {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// Well-known classifications used by the built-in rules.
pub mod classification {
    /// Reachable, but the scheduler will not use it.
    pub const SCHEDULER_DEGRADED: &str = "SCHEDULER_DEGRADED";
    /// SSH is broken but the host is not.
    pub const SSH_DEGRADED: &str = "SSH_DEGRADED";
    /// Storage the entity provides or uses is impaired.
    pub const STORAGE_DEGRADED: &str = "STORAGE_DEGRADED";
    /// No observer can reach the entity.
    pub const HOST_UNREACHABLE: &str = "HOST_UNREACHABLE";
    /// A service on the entity has failed.
    pub const SERVICE_FAILURE: &str = "SERVICE_FAILURE";
    /// The Sentinel agent is not reporting.
    pub const AGENT_DEGRADED: &str = "AGENT_DEGRADED";
}

/// The current state of one entity.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EntityState {
    /// The entity this state describes.
    pub entity: EntityId,
    /// Per-component health.
    pub components: BTreeMap<StateComponent, ComponentState>,
    /// Rolled-up health.
    pub overall: Health,
    /// Machine-readable refinements of `overall`.
    pub classifications: Vec<Classification>,
    /// When this state was last recomputed.
    pub updated_at: Timestamp,
}

impl EntityState {
    /// An entity with no observations yet: everything unknown.
    pub fn unknown(entity: EntityId) -> Self {
        Self {
            entity,
            components: BTreeMap::new(),
            overall: Health::Unknown,
            classifications: Vec::new(),
            updated_at: now(),
        }
    }

    /// Health of one component, `NotApplicable` if never recorded.
    pub fn component(&self, component: StateComponent) -> Health {
        self.components
            .get(&component)
            .map(|c| c.health)
            .unwrap_or(Health::NotApplicable)
    }

    /// Record a component's health.
    pub fn set_component(&mut self, component: StateComponent, state: ComponentState) {
        self.components.insert(component, state);
        self.overall = self.rollup();
        self.updated_at = now();
    }

    /// Worst component health, ignoring components that do not apply.
    pub fn rollup(&self) -> Health {
        self.components
            .values()
            .map(|c| c.health)
            .filter(|h| !matches!(h, Health::NotApplicable))
            .max_by_key(|h| h.severity_rank())
            .unwrap_or(Health::Unknown)
    }

    /// Add a classification if not already present.
    pub fn classify(&mut self, classification: impl Into<Classification>) {
        let classification = classification.into();
        if !self.classifications.contains(&classification) {
            self.classifications.push(classification);
        }
    }

    /// Replace every classification with the ones currently justified.
    ///
    /// Classifications are derived from diagnosis, and diagnosis is recomputed
    /// from scratch each cycle, so they have to be *replaced* rather than
    /// accumulated. Adding without ever removing leaves an entity wearing the
    /// label of a fault that has since been repaired -- a stale label is worse
    /// than no label, because an operator cannot tell it from a live one.
    pub fn set_classifications(&mut self, classifications: impl IntoIterator<Item = Classification>) {
        let mut replacement: Vec<Classification> = Vec::new();
        for classification in classifications {
            if !replacement.contains(&classification) {
                replacement.push(classification);
            }
        }
        self.classifications = replacement;
    }

    /// Whether the entity carries a given classification.
    pub fn has_classification(&self, name: &str) -> bool {
        self.classifications.iter().any(|c| c.as_str() == name)
    }
}

impl From<&str> for Classification {
    fn from(s: &str) -> Self {
        Classification::new(s)
    }
}

/// Health of one component, with the observations that justify it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ComponentState {
    /// Current health.
    pub health: Health,
    /// Observations supporting this verdict.
    pub evidence: Vec<ObservationId>,
    /// When this component last changed health.
    pub since: Timestamp,
    /// Consecutive bad observations counted by the debouncer.
    pub consecutive_failures: u32,
    /// Consecutive good observations counted by the debouncer.
    pub consecutive_successes: u32,
}

impl ComponentState {
    /// A component state with the given health and no evidence.
    pub fn new(health: Health) -> Self {
        Self {
            health,
            evidence: Vec::new(),
            since: now(),
            consecutive_failures: 0,
            consecutive_successes: 0,
        }
    }

    /// Builder: attach supporting observations.
    pub fn with_evidence(mut self, evidence: impl IntoIterator<Item = ObservationId>) -> Self {
        self.evidence = evidence.into_iter().collect();
        self
    }
}

/// A recorded change of an entity's component health (SPEC.md §89).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StateTransition {
    /// Transition identifier.
    pub id: Uuid,
    /// Entity whose state changed.
    pub entity: EntityId,
    /// Component that changed, `None` for the overall rollup.
    pub component: Option<StateComponent>,
    /// Health before.
    pub from: Health,
    /// Health after.
    pub to: Health,
    /// When the change happened.
    pub at: Timestamp,
    /// Observations that triggered the change.
    pub evidence: Vec<ObservationId>,
}

impl StateTransition {
    /// Record a transition.
    pub fn new(entity: EntityId, component: Option<StateComponent>, from: Health, to: Health) -> Self {
        Self {
            id: Uuid::new_v4(),
            entity,
            component,
            from,
            to,
            at: now(),
            evidence: Vec::new(),
        }
    }

    /// Builder: attach supporting observations.
    pub fn with_evidence(mut self, evidence: impl IntoIterator<Item = ObservationId>) -> Self {
        self.evidence = evidence.into_iter().collect();
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::entity::{EntityKey, EntityType};

    fn entity() -> EntityId {
        EntityKey::new("env", EntityType::Host, "node01").entity_id()
    }

    #[test]
    fn component_names_roundtrip() {
        for component in StateComponent::ALL {
            assert_eq!(StateComponent::parse(component.as_str()), Some(component));
        }
    }

    #[test]
    fn health_names_roundtrip() {
        for health in [
            Health::Healthy,
            Health::Degraded,
            Health::Unavailable,
            Health::Maintenance,
            Health::Unknown,
            Health::NotApplicable,
        ] {
            assert_eq!(Health::parse(health.as_str()), Some(health));
        }
    }

    #[test]
    fn rollup_takes_the_worst_applicable_component() {
        let mut state = EntityState::unknown(entity());
        state.set_component(StateComponent::Host, ComponentState::new(Health::Healthy));
        state.set_component(StateComponent::Ssh, ComponentState::new(Health::Healthy));
        assert_eq!(state.overall, Health::Healthy);

        state.set_component(StateComponent::Scheduler, ComponentState::new(Health::Degraded));
        assert_eq!(state.overall, Health::Degraded);

        state.set_component(StateComponent::Availability, ComponentState::new(Health::Unavailable));
        assert_eq!(state.overall, Health::Unavailable);
    }

    #[test]
    fn not_applicable_components_never_colour_the_rollup() {
        let mut state = EntityState::unknown(entity());
        state.set_component(StateComponent::Host, ComponentState::new(Health::Healthy));
        state.set_component(StateComponent::Accelerator, ComponentState::new(Health::NotApplicable));
        state.set_component(StateComponent::Scheduler, ComponentState::new(Health::NotApplicable));
        assert_eq!(
            state.overall,
            Health::Healthy,
            "a node without GPUs is not a degraded node"
        );
    }

    #[test]
    fn an_entity_with_only_inapplicable_components_is_unknown_not_healthy() {
        let mut state = EntityState::unknown(entity());
        state.set_component(StateComponent::Accelerator, ComponentState::new(Health::NotApplicable));
        assert_eq!(state.overall, Health::Unknown);
    }

    #[test]
    fn unknown_outranks_healthy_but_not_degraded() {
        assert!(Health::Unknown.severity_rank() > Health::Healthy.severity_rank());
        assert!(Health::Unknown.severity_rank() < Health::Degraded.severity_rank());
        assert!(!Health::Unknown.is_problem());
    }

    #[test]
    fn classifications_are_deduplicated() {
        let mut state = EntityState::unknown(entity());
        state.classify(classification::SCHEDULER_DEGRADED);
        state.classify(classification::SCHEDULER_DEGRADED);
        assert_eq!(state.classifications.len(), 1);
        assert!(state.has_classification(classification::SCHEDULER_DEGRADED));
        assert!(!state.has_classification(classification::HOST_UNREACHABLE));
    }

    #[test]
    fn unrecorded_components_read_as_not_applicable() {
        let state = EntityState::unknown(entity());
        assert_eq!(state.component(StateComponent::Storage), Health::NotApplicable);
    }
}
