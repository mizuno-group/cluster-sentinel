//! Inventory: what exists, merged from several sources.
//!
//! Sentinel's inventory is **not** the Slurm node list (SPEC.md §30, §180). It
//! is the union of what several providers report — Slurm, agent registration,
//! static configuration, and whatever comes later — with each entity's origins
//! preserved.
//!
//! An entity that stops being discovered is marked [`LifecycleState::Stale`],
//! never deleted (SPEC.md §35). Losing history the moment a controller has a
//! bad minute would destroy exactly the evidence an incident needs.

mod merge;
pub mod slurm;
pub mod static_config;

pub use merge::{Inventory, MergeOutcome};

use async_trait::async_trait;
use thiserror::Error;

use crate::dependency::DependencyEdge;
use crate::entity::{DiscoverySource, ManagedEntity};

/// Why a provider could not produce a snapshot.
#[derive(Debug, Error)]
pub enum InventoryError {
    /// The provider's data source was unreachable or unusable.
    #[error("{provider} discovery failed: {detail}")]
    Unavailable {
        /// Which provider failed.
        provider: String,
        /// What went wrong.
        detail: String,
    },
    /// The provider's input was present but could not be understood.
    #[error("{provider} returned unusable data: {detail}")]
    Malformed {
        /// Which provider failed.
        provider: String,
        /// What went wrong.
        detail: String,
    },
}

/// One provider's view of the world at one moment.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct InventorySnapshot {
    /// Entities this provider saw.
    pub entities: Vec<ManagedEntity>,
    /// Dependency edges this provider saw.
    pub dependencies: Vec<DependencyEdge>,
    /// Which provider produced it.
    pub source: Option<DiscoverySource>,
}

impl InventorySnapshot {
    /// An empty snapshot attributed to `source`.
    pub fn new(source: DiscoverySource) -> Self {
        Self {
            entities: Vec::new(),
            dependencies: Vec::new(),
            source: Some(source),
        }
    }

    /// Add an entity, stamping it with this snapshot's source.
    pub fn add_entity(&mut self, mut entity: ManagedEntity) -> &mut Self {
        if let Some(source) = &self.source {
            entity = entity.with_discovery_source(source.clone());
        }
        self.entities.push(entity);
        self
    }

    /// Add a dependency edge, stamping it with this snapshot's source.
    pub fn add_dependency(&mut self, mut edge: DependencyEdge) -> &mut Self {
        if let Some(source) = &self.source {
            edge = edge.with_discovery_source(source.clone());
        }
        self.dependencies.push(edge);
        self
    }

    /// Whether the snapshot found nothing at all.
    pub fn is_empty(&self) -> bool {
        self.entities.is_empty() && self.dependencies.is_empty()
    }
}

/// A source of inventory.
///
/// Deliberately small. Adding a provider must not require touching the merge
/// logic, the state engine or the schema (SPEC.md §30, §100 of
/// IMPLEMENTATION.md).
#[async_trait]
pub trait InventoryProvider: Send + Sync {
    /// Provider name, used in logs and in `discovery_source`.
    fn name(&self) -> &str;

    /// Look at the world and report what is there.
    async fn discover(&self) -> Result<InventorySnapshot, InventoryError>;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::entity::EntityType;

    #[test]
    fn a_snapshot_stamps_everything_it_carries_with_its_source() {
        let mut snapshot = InventorySnapshot::new(DiscoverySource::Integration("slurm".into()));
        snapshot.add_entity(ManagedEntity::new("lab", EntityType::Host, "node-a"));

        assert_eq!(
            snapshot.entities[0].discovery_sources,
            vec![DiscoverySource::Integration("slurm".into())]
        );
    }

    #[test]
    fn an_empty_snapshot_is_recognisable() {
        let snapshot = InventorySnapshot::new(DiscoverySource::StaticConfig);
        assert!(snapshot.is_empty());
    }

    #[test]
    fn errors_name_the_provider_that_failed() {
        let error = InventoryError::Unavailable {
            provider: "slurm".into(),
            detail: "scontrol timed out".into(),
        };
        assert!(error.to_string().contains("slurm"));
        assert!(error.to_string().contains("scontrol timed out"));
    }
}
