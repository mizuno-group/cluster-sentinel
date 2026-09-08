//! [`ManagedEntity`] — the single core abstraction Sentinel keeps state for.
//!
//! v0.3 deliberately does *not* make `Host` the only entity type: a host, a
//! systemd service, a storage domain, a scheduler control plane and an
//! un-instrumentable external dependency are all the same kind of thing to the
//! core (SPEC.md §7, §186).

mod id;

pub use id::{EntityId, EntityKey};

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::capability::CapabilitySet;
use crate::time::{now, Timestamp};

/// Kinds of entity the core models. Adding a variant must never require
/// changes to the state, diagnosis or incident engines.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EntityType {
    /// A physical or virtual Linux machine.
    Host,
    /// A service running on a host (`slurmd@node`, `sshd@node`, ...).
    Service,
    /// A storage domain, independent of the host(s) backing it.
    Storage,
    /// A scheduler / control plane, distinct from the host running it.
    Scheduler,
    /// Something Sentinel depends on but cannot instrument directly.
    ExternalDependency,
}

impl EntityType {
    /// Stable string form used in the database and on the wire.
    pub fn as_str(&self) -> &'static str {
        match self {
            EntityType::Host => "host",
            EntityType::Service => "service",
            EntityType::Storage => "storage",
            EntityType::Scheduler => "scheduler",
            EntityType::ExternalDependency => "external_dependency",
        }
    }

    /// Parse the stable string form.
    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "host" => EntityType::Host,
            "service" => EntityType::Service,
            "storage" => EntityType::Storage,
            "scheduler" => EntityType::Scheduler,
            "external_dependency" => EntityType::ExternalDependency,
            _ => return None,
        })
    }
}

impl std::fmt::Display for EntityType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Lifecycle of an entity in the inventory (SPEC.md §35).
///
/// An entity that stops being discovered is **never** deleted automatically —
/// a transient infrastructure inconsistency must not look like a fault.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LifecycleState {
    /// Currently discovered by at least one provider.
    Active,
    /// Not discovered recently, but retained with its history.
    Stale,
    /// Explicitly retired by an operator.
    Removed,
}

impl LifecycleState {
    /// Stable string form used in the database.
    pub fn as_str(&self) -> &'static str {
        match self {
            LifecycleState::Active => "active",
            LifecycleState::Stale => "stale",
            LifecycleState::Removed => "removed",
        }
    }

    /// Parse the stable string form.
    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "active" => LifecycleState::Active,
            "stale" => LifecycleState::Stale,
            "removed" => LifecycleState::Removed,
            _ => return None,
        })
    }
}

/// Where an entity, capability or dependency edge came from.
///
/// Inventory is merged from several providers and the origin is always kept
/// (SPEC.md §34).
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DiscoverySource {
    /// Declared in the controller configuration file.
    StaticConfig,
    /// Reported by a Sentinel agent registering itself.
    AgentRegistration,
    /// Discovered through an integration, identified by name (`slurm`, ...).
    Integration(String),
    /// Created by an operator through the CLI/API.
    Manual,
}

impl DiscoverySource {
    /// Stable string form used in the database.
    pub fn as_str(&self) -> String {
        match self {
            DiscoverySource::StaticConfig => "static_config".to_string(),
            DiscoverySource::AgentRegistration => "agent_registration".to_string(),
            DiscoverySource::Integration(name) => format!("integration:{name}"),
            DiscoverySource::Manual => "manual".to_string(),
        }
    }

    /// Parse the stable string form.
    pub fn parse(s: &str) -> Self {
        match s {
            "static_config" => DiscoverySource::StaticConfig,
            "agent_registration" => DiscoverySource::AgentRegistration,
            "manual" => DiscoverySource::Manual,
            other => match other.strip_prefix("integration:") {
                Some(name) => DiscoverySource::Integration(name.to_string()),
                None => DiscoverySource::Manual,
            },
        }
    }
}

/// Free-form operator metadata. The diagnosis core must never branch on a
/// specific label key (SPEC.md §133).
pub type Labels = BTreeMap<String, String>;

/// A monitored thing with state.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ManagedEntity {
    /// Stable internal identifier.
    pub id: EntityId,
    /// Environment this entity belongs to.
    pub environment: String,
    /// Optional logical cluster grouping.
    pub cluster: Option<String>,
    /// What kind of entity this is.
    pub entity_type: EntityType,
    /// Natural key within `(environment, entity_type)`; never an IP address.
    pub canonical_name: String,
    /// Human-facing name; defaults to `canonical_name`.
    pub display_name: String,
    /// Operator labels (grouping/UI only).
    pub labels: Labels,
    /// Integration-supplied structured metadata.
    pub metadata: serde_json::Value,
    /// Capabilities that decide which probes apply.
    pub capabilities: CapabilitySet,
    /// Inventory lifecycle.
    pub lifecycle_state: LifecycleState,
    /// Providers that reported this entity.
    pub discovery_sources: Vec<DiscoverySource>,
    /// First time this entity was seen.
    pub created_at: Timestamp,
    /// Last time this entity was updated.
    pub updated_at: Timestamp,
}

impl ManagedEntity {
    /// Create an entity from its natural key.
    pub fn new(environment: impl Into<String>, entity_type: EntityType, canonical_name: impl Into<String>) -> Self {
        let environment = environment.into();
        let canonical_name = canonical_name.into();
        let key = EntityKey::new(&environment, entity_type, &canonical_name);
        let ts = now();
        Self {
            id: key.entity_id(),
            environment,
            cluster: None,
            entity_type,
            display_name: canonical_name.clone(),
            canonical_name,
            labels: Labels::new(),
            metadata: serde_json::Value::Null,
            capabilities: CapabilitySet::new(),
            lifecycle_state: LifecycleState::Active,
            discovery_sources: Vec::new(),
            created_at: ts,
            updated_at: ts,
        }
    }

    /// Natural key used to merge discoveries from different providers.
    pub fn key(&self) -> EntityKey {
        EntityKey::new(&self.environment, self.entity_type, &self.canonical_name)
    }

    /// Builder: set the logical cluster.
    pub fn with_cluster(mut self, cluster: impl Into<String>) -> Self {
        self.cluster = Some(cluster.into());
        self
    }

    /// Builder: set the display name.
    pub fn with_display_name(mut self, name: impl Into<String>) -> Self {
        self.display_name = name.into();
        self
    }

    /// Builder: add a label.
    pub fn with_label(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.labels.insert(key.into(), value.into());
        self
    }

    /// Builder: record the provider that discovered this entity.
    pub fn with_discovery_source(mut self, source: DiscoverySource) -> Self {
        if !self.discovery_sources.contains(&source) {
            self.discovery_sources.push(source);
        }
        self
    }

    /// Builder: replace the capability set.
    pub fn with_capabilities(mut self, capabilities: CapabilitySet) -> Self {
        self.capabilities = capabilities;
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn entity_type_string_form_roundtrips() {
        for ty in [
            EntityType::Host,
            EntityType::Service,
            EntityType::Storage,
            EntityType::Scheduler,
            EntityType::ExternalDependency,
        ] {
            assert_eq!(EntityType::parse(ty.as_str()), Some(ty));
        }
        assert_eq!(EntityType::parse("nope"), None);
    }

    #[test]
    fn discovery_source_roundtrips_including_integration_name() {
        let sources = [
            DiscoverySource::StaticConfig,
            DiscoverySource::AgentRegistration,
            DiscoverySource::Manual,
            DiscoverySource::Integration("slurm".into()),
        ];
        for source in sources {
            assert_eq!(DiscoverySource::parse(&source.as_str()), source);
        }
    }

    #[test]
    fn identity_is_derived_from_the_natural_key_not_from_addresses() {
        let a = ManagedEntity::new("env", EntityType::Host, "node01");
        let b = ManagedEntity::new("env", EntityType::Host, "node01").with_label("ip", "10.0.0.9");
        assert_eq!(a.id, b.id, "labels must not affect identity");

        let other_env = ManagedEntity::new("other", EntityType::Host, "node01");
        assert_ne!(a.id, other_env.id, "identity is scoped to the environment");

        let other_type = ManagedEntity::new("env", EntityType::Service, "node01");
        assert_ne!(a.id, other_type.id, "identity is scoped to the entity type");
    }

    #[test]
    fn lifecycle_state_roundtrips() {
        for st in [LifecycleState::Active, LifecycleState::Stale, LifecycleState::Removed] {
            assert_eq!(LifecycleState::parse(st.as_str()), Some(st));
        }
    }
}
