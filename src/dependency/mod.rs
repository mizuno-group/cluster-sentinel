//! The dependency graph.
//!
//! This is a **general directed graph**, not an NFS tree and not a DAG
//! (SPEC.md §24, §27, §28). Operational dependencies legitimately contain
//! cycles, so the storage layer accepts them and every traversal carries a
//! visited set.
//!
//! Edge direction is always `A depends on B` => `A -> B`.

mod graph;

pub use graph::{DependencyGraph, Reachable};

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::entity::{DiscoverySource, EntityId};
use crate::time::{now, Timestamp};

/// The nature of a dependency. Open-ended: an integration may add its own
/// without touching the core.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DependencyType {
    /// Generic dependency.
    DependsOn,
    /// A service runs on a host.
    HostedOn,
    /// An entity provides a higher-level entity (service -> scheduler).
    Provides,
    /// A client uses a storage entity.
    UsesStorage,
    /// A node uses a scheduler.
    UsesScheduler,
    /// Network reachability is required.
    NetworkReaches,
    /// An observer watches a target.
    Observes,
    /// Integration-specific relation.
    Other(String),
}

impl DependencyType {
    /// Stable string form used in the database.
    pub fn as_str(&self) -> String {
        match self {
            DependencyType::DependsOn => "depends_on".into(),
            DependencyType::HostedOn => "hosted_on".into(),
            DependencyType::Provides => "provides".into(),
            DependencyType::UsesStorage => "uses_storage".into(),
            DependencyType::UsesScheduler => "uses_scheduler".into(),
            DependencyType::NetworkReaches => "network_reaches".into(),
            DependencyType::Observes => "observes".into(),
            DependencyType::Other(name) => name.clone(),
        }
    }

    /// Parse the stable string form.
    pub fn parse(s: &str) -> Self {
        match s {
            "depends_on" => DependencyType::DependsOn,
            "hosted_on" => DependencyType::HostedOn,
            "provides" => DependencyType::Provides,
            "uses_storage" => DependencyType::UsesStorage,
            "uses_scheduler" => DependencyType::UsesScheduler,
            "network_reaches" => DependencyType::NetworkReaches,
            "observes" => DependencyType::Observes,
            other => DependencyType::Other(other.to_string()),
        }
    }
}

/// How badly the source needs the target.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Criticality {
    /// Loss of the target makes the source unusable.
    Critical,
    /// Loss of the target degrades the source.
    Important,
    /// Informational relation only.
    Optional,
}

impl Criticality {
    /// Stable string form used in the database.
    pub fn as_str(&self) -> &'static str {
        match self {
            Criticality::Critical => "critical",
            Criticality::Important => "important",
            Criticality::Optional => "optional",
        }
    }

    /// Parse the stable string form.
    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "critical" => Criticality::Critical,
            "important" => Criticality::Important,
            "optional" => Criticality::Optional,
            _ => return None,
        })
    }
}

/// One directed edge: `source` depends on `target`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DependencyEdge {
    /// Stable edge identifier.
    pub id: Uuid,
    /// The dependent entity.
    pub source: EntityId,
    /// The entity depended upon.
    pub target: EntityId,
    /// Nature of the dependency.
    pub dependency_type: DependencyType,
    /// How badly the source needs the target.
    pub criticality: Criticality,
    /// Integration-specific metadata.
    pub metadata: serde_json::Value,
    /// Which provider reported the edge.
    pub discovery_source: DiscoverySource,
    /// When the edge was first observed.
    pub first_seen_at: Timestamp,
    /// When the edge was last confirmed.
    pub last_seen_at: Timestamp,
}

impl DependencyEdge {
    /// Create an edge. The id is derived from `(source, target, type)` so that
    /// re-discovery of the same relation updates rather than duplicates.
    pub fn new(source: EntityId, target: EntityId, dependency_type: DependencyType) -> Self {
        let ts = now();
        let name = format!("{}\u{1f}{}\u{1f}{}", source, target, dependency_type.as_str());
        Self {
            id: Uuid::new_v5(
                &Uuid::from_u128(0x4c2f9ab1_1d33_5c8e_a0b7_5e2d9c47f1aa),
                name.as_bytes(),
            ),
            source,
            target,
            dependency_type,
            criticality: Criticality::Critical,
            metadata: serde_json::Value::Null,
            discovery_source: DiscoverySource::Manual,
            first_seen_at: ts,
            last_seen_at: ts,
        }
    }

    /// Builder: set criticality.
    pub fn with_criticality(mut self, criticality: Criticality) -> Self {
        self.criticality = criticality;
        self
    }

    /// Builder: set the discovery source.
    pub fn with_discovery_source(mut self, source: DiscoverySource) -> Self {
        self.discovery_source = source;
        self
    }

    /// Builder: set metadata.
    pub fn with_metadata(mut self, metadata: serde_json::Value) -> Self {
        self.metadata = metadata;
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::entity::{EntityKey, EntityType};

    fn id(name: &str) -> EntityId {
        EntityKey::new("env", EntityType::Host, name).entity_id()
    }

    #[test]
    fn dependency_type_roundtrips_including_unknown_values() {
        for ty in [
            DependencyType::DependsOn,
            DependencyType::UsesStorage,
            DependencyType::Observes,
            DependencyType::Other("uses_ceph_pool".into()),
        ] {
            assert_eq!(DependencyType::parse(&ty.as_str()), ty);
        }
    }

    #[test]
    fn edge_id_is_stable_for_the_same_relation() {
        let a = DependencyEdge::new(id("a"), id("b"), DependencyType::UsesStorage);
        let b = DependencyEdge::new(id("a"), id("b"), DependencyType::UsesStorage);
        assert_eq!(a.id, b.id);

        let reversed = DependencyEdge::new(id("b"), id("a"), DependencyType::UsesStorage);
        assert_ne!(a.id, reversed.id, "direction is part of the relation");

        let other_type = DependencyEdge::new(id("a"), id("b"), DependencyType::DependsOn);
        assert_ne!(a.id, other_type.id);
    }

    #[test]
    fn criticality_roundtrips() {
        for c in [Criticality::Critical, Criticality::Important, Criticality::Optional] {
            assert_eq!(Criticality::parse(c.as_str()), Some(c));
        }
    }
}
