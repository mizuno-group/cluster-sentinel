//! The probe abstraction.
//!
//! A probe is a small, pre-defined measurement gated on a capability. Probes
//! are statically compiled into the binary (IMPLEMENTATION.md §33) and the
//! Sentinel RPC never accepts an arbitrary command to run (SPEC.md §116).
//!
//! Concrete probes live in submodules; the core only knows this interface.

pub mod catalog;
pub mod gpu;
pub mod host;
pub mod journal;
pub mod network;
pub mod nfs;
mod runner;
pub mod sentinel_rpc;
pub mod ssh;
pub mod systemd;

pub use runner::{ProbeRunner, Skipped};

use std::fmt;
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::capability::{Capability, CapabilitySet};
use crate::entity::{EntityId, EntityType};
use crate::observation::Observation;

/// Identifier of a probe kind, e.g. `network.tcp` or `slurm.node`.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ProbeId(String);

impl ProbeId {
    /// Build a probe id.
    pub fn new(id: impl Into<String>) -> Self {
        Self(id.into())
    }

    /// The probe id as a string.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl From<&str> for ProbeId {
    fn from(s: &str) -> Self {
        ProbeId::new(s)
    }
}

impl fmt::Display for ProbeId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// Where a probe runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExecutionMode {
    /// Runs on the entity it measures.
    Local,
    /// Runs on an observer, measuring a different entity.
    Remote,
    /// Runs either way.
    Either,
}

/// Static description of a probe: what it needs, how often it runs and how long
/// it may take. Held separately from the probe implementation so the scheduler
/// and the CLI can reason about probes without running them.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ProbeDefinition {
    /// Probe identifier.
    pub id: ProbeId,
    /// Capabilities the entity must have for this probe to apply.
    ///
    /// Empty means the probe applies to every entity of the right type. Note
    /// this is a *capability* gate, never a role gate (SPEC.md §15).
    pub required_capabilities: Vec<Capability>,
    /// Entity types this probe can measure. Empty means any type.
    pub target_entity_types: Vec<EntityType>,
    /// How often the probe should run.
    #[serde(with = "humantime_serde")]
    pub interval: Duration,
    /// How long one execution may take before it is a [`crate::observation::ProbeStatus::Timeout`].
    #[serde(with = "humantime_serde")]
    pub timeout: Duration,
    /// Where the probe runs.
    pub execution_mode: ExecutionMode,
    /// How many executions of this probe may be in flight per target.
    ///
    /// NFS filesystem probes set this to 1: a blocked syscall must never be
    /// joined by a second one (SPEC.md §76, IMPLEMENTATION.md §53).
    pub max_outstanding: u32,
}

impl ProbeDefinition {
    /// A definition with sane defaults: no capability gate, 30s interval,
    /// 5s timeout, local execution, unbounded concurrency.
    pub fn new(id: impl Into<ProbeId>) -> Self {
        Self {
            id: id.into(),
            required_capabilities: Vec::new(),
            target_entity_types: Vec::new(),
            interval: Duration::from_secs(30),
            timeout: Duration::from_secs(5),
            execution_mode: ExecutionMode::Local,
            max_outstanding: u32::MAX,
        }
    }

    /// Builder: require capabilities.
    pub fn requiring(mut self, capabilities: impl IntoIterator<Item = &'static str>) -> Self {
        self.required_capabilities = capabilities.into_iter().map(Capability::new).collect();
        self
    }

    /// Builder: restrict target entity types.
    pub fn targeting(mut self, entity_types: impl IntoIterator<Item = EntityType>) -> Self {
        self.target_entity_types = entity_types.into_iter().collect();
        self
    }

    /// Builder: set the interval.
    pub fn every(mut self, interval: Duration) -> Self {
        self.interval = interval;
        self
    }

    /// Builder: set the timeout.
    pub fn within(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    /// Builder: set the execution mode.
    pub fn mode(mut self, execution_mode: ExecutionMode) -> Self {
        self.execution_mode = execution_mode;
        self
    }

    /// Builder: bound the number of in-flight executions per target.
    pub fn max_outstanding(mut self, max_outstanding: u32) -> Self {
        self.max_outstanding = max_outstanding;
        self
    }

    /// Whether this probe applies to an entity with these capabilities.
    pub fn applies_to(&self, entity_type: EntityType, capabilities: &CapabilitySet) -> bool {
        if !self.target_entity_types.is_empty() && !self.target_entity_types.contains(&entity_type) {
            return false;
        }
        capabilities.has_all(&self.required_capabilities)
    }
}

/// A probe whose schedule an operator may change.
///
/// Separate from [`Probe`] because it needs `&mut self`, which is gone by the
/// time a probe has been put behind an `Arc`. Overrides are therefore applied
/// at construction, before the probe is shared.
pub trait HasDefinition {
    /// The definition, mutably, so a schedule override can be applied to it.
    fn definition_mut(&mut self) -> &mut ProbeDefinition;
}

/// Implement [`HasDefinition`] for a probe holding a `definition` field.
macro_rules! configurable_probe {
    ($($probe:ty),+ $(,)?) => {
        $(
            impl $crate::probes::HasDefinition for $probe {
                fn definition_mut(&mut self) -> &mut $crate::probes::ProbeDefinition {
                    &mut self.definition
                }
            }
        )+
    };
}

pub(crate) use configurable_probe;

/// What a probe is told about the entity it is measuring.
#[derive(Debug, Clone)]
pub struct ProbeContext {
    /// The entity being measured.
    pub target_entity: EntityId,
    /// The observer, when this is a remote probe.
    pub observer_entity: Option<EntityId>,
    /// The target's capabilities.
    pub capabilities: CapabilitySet,
    /// Probe-specific parameters resolved from configuration and discovery
    /// (addresses, mount points, unit names). Deployment data reaches probes
    /// as data, never as hard-coded constants (IMPLEMENTATION.md §101).
    pub parameters: serde_json::Value,
    /// Effective timeout for this execution.
    pub timeout: Duration,
}

impl ProbeContext {
    /// A context for a locally executed probe.
    pub fn local(target_entity: EntityId, capabilities: CapabilitySet) -> Self {
        Self {
            target_entity,
            observer_entity: None,
            capabilities,
            parameters: serde_json::Value::Null,
            timeout: Duration::from_secs(5),
        }
    }

    /// Builder: mark this as a remote observation.
    pub fn observed_by(mut self, observer: EntityId) -> Self {
        self.observer_entity = Some(observer);
        self
    }

    /// Builder: attach parameters.
    pub fn with_parameters(mut self, parameters: serde_json::Value) -> Self {
        self.parameters = parameters;
        self
    }

    /// Builder: set the timeout.
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    /// Read a string parameter.
    pub fn parameter_str(&self, key: &str) -> Option<&str> {
        self.parameters.get(key)?.as_str()
    }

    /// Read an integer parameter.
    pub fn parameter_u64(&self, key: &str) -> Option<u64> {
        self.parameters.get(key)?.as_u64()
    }
}

/// A probe implementation.
///
/// Implementations return raw facts. Drawing conclusions from them is the
/// diagnosis engine's job (IMPLEMENTATION.md §68).
#[async_trait::async_trait]
pub trait Probe: Send + Sync {
    /// Static description of this probe.
    fn definition(&self) -> &ProbeDefinition;

    /// The probe id, from the definition.
    fn id(&self) -> &ProbeId {
        &self.definition().id
    }

    /// Run one measurement.
    ///
    /// A probe must never panic the daemon; the runner isolates failures, but
    /// implementations should return an error observation instead
    /// (SPEC.md §119).
    async fn collect(&self, context: &ProbeContext) -> Observation;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_probe_without_a_capability_gate_applies_everywhere() {
        let definition = ProbeDefinition::new("network.tcp");
        assert!(definition.applies_to(EntityType::Host, &CapabilitySet::new()));
        assert!(definition.applies_to(EntityType::Storage, &CapabilitySet::new()));
    }

    #[test]
    fn capability_gate_decides_applicability() {
        let definition = ProbeDefinition::new("nfs.server").requiring(["storage.nfs.server"]);
        let without: CapabilitySet = ["host.metrics"].into_iter().collect();
        let with: CapabilitySet = ["host.metrics", "storage.nfs.server"].into_iter().collect();
        assert!(!definition.applies_to(EntityType::Host, &without));
        assert!(definition.applies_to(EntityType::Host, &with));
    }

    #[test]
    fn a_role_label_cannot_substitute_for_a_capability() {
        // The entity is labelled a fileserver but lacks the capability: the
        // probe must stay off (SPEC.md §15, §179).
        let definition = ProbeDefinition::new("nfs.server").requiring(["storage.nfs.server"]);
        let capabilities: CapabilitySet = ["host.metrics", "ssh.server"].into_iter().collect();
        assert!(!definition.applies_to(EntityType::Host, &capabilities));
    }

    #[test]
    fn entity_type_restriction_is_honoured() {
        let definition = ProbeDefinition::new("host.metrics").targeting([EntityType::Host]);
        assert!(definition.applies_to(EntityType::Host, &CapabilitySet::new()));
        assert!(!definition.applies_to(EntityType::Scheduler, &CapabilitySet::new()));
    }

    #[test]
    fn all_required_capabilities_must_be_present() {
        let definition = ProbeDefinition::new("gpu.slurm.match").requiring(["gpu.nvidia", "slurm.compute"]);
        let partial: CapabilitySet = ["gpu.nvidia"].into_iter().collect();
        let complete: CapabilitySet = ["gpu.nvidia", "slurm.compute"].into_iter().collect();
        assert!(!definition.applies_to(EntityType::Host, &partial));
        assert!(definition.applies_to(EntityType::Host, &complete));
    }

    #[test]
    fn definition_serialization_uses_human_durations() {
        let definition = ProbeDefinition::new("test")
            .every(Duration::from_secs(15))
            .within(Duration::from_secs(3));
        let text = serde_json::to_string(&definition).expect("serialize");
        assert!(text.contains("\"15s\""), "{text}");
        let decoded: ProbeDefinition = serde_json::from_str(&text).expect("deserialize");
        assert_eq!(decoded, definition);
    }
}
