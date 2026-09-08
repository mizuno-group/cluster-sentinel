//! Inventory from the configuration file (SPEC.md §33).
//!
//! For everything no integration will find on its own: a fileserver with no
//! agent yet, a storage domain that exists only as a concept, a switch.

use async_trait::async_trait;

use crate::capability::CapabilitySet;
use crate::config::Config;
use crate::dependency::{Criticality, DependencyEdge, DependencyType};
use crate::entity::{DiscoverySource, EntityKey, EntityType, ManagedEntity};

use super::{InventoryError, InventoryProvider, InventorySnapshot};

/// Reads entities and dependencies out of the configuration.
#[derive(Debug, Clone)]
pub struct StaticConfigProvider {
    environment: String,
    config: Config,
}

impl StaticConfigProvider {
    /// Build a provider over a configuration.
    pub fn new(config: Config) -> Self {
        Self {
            environment: config.environment.clone(),
            config,
        }
    }

    /// Build the snapshot. Separate from [`InventoryProvider::discover`] so it
    /// can be used synchronously in tests and in `sentinel config check`.
    pub fn snapshot(&self) -> InventorySnapshot {
        let mut snapshot = InventorySnapshot::new(DiscoverySource::StaticConfig);

        for declared in &self.config.entities {
            let Some(entity_type) = EntityType::parse(&declared.entity_type) else {
                // Validation reports this; discovery skips it rather than
                // failing the whole cycle.
                continue;
            };

            let mut entity = ManagedEntity::new(&self.environment, entity_type, &declared.name).with_capabilities(
                declared
                    .capabilities
                    .iter()
                    .map(String::as_str)
                    .collect::<CapabilitySet>(),
            );

            if let Some(display_name) = &declared.display_name {
                entity = entity.with_display_name(display_name);
            }
            if let Some(cluster) = &declared.cluster {
                entity = entity.with_cluster(cluster);
            }
            for (key, value) in &declared.labels {
                entity = entity.with_label(key, value);
            }
            // Addresses and ports are reachability data hung off the entity,
            // never part of its identity (SPEC.md §37).
            let mut metadata = serde_json::Map::new();
            if !declared.addresses.is_empty() {
                metadata.insert("addresses".into(), serde_json::json!(declared.addresses));
            }
            if !declared.ports.is_empty() {
                metadata.insert("ports".into(), serde_json::json!(declared.ports));
            }
            if !metadata.is_empty() {
                entity.metadata = serde_json::Value::Object(metadata);
            }

            snapshot.add_entity(entity);
        }

        for declared in &self.config.dependencies {
            let (Some(source), Some(target)) = (self.resolve(&declared.from), self.resolve(&declared.to)) else {
                continue;
            };
            let edge = DependencyEdge::new(source, target, DependencyType::parse(&declared.dependency_type))
                .with_criticality(Criticality::parse(&declared.criticality).unwrap_or(Criticality::Critical));
            snapshot.add_dependency(edge);
        }

        snapshot
    }

    fn resolve(&self, reference: &str) -> Option<crate::entity::EntityId> {
        let (entity_type, name) = reference.split_once('/')?;
        let entity_type = EntityType::parse(entity_type)?;
        Some(EntityKey::new(&self.environment, entity_type, name).entity_id())
    }
}

#[async_trait]
impl InventoryProvider for StaticConfigProvider {
    fn name(&self) -> &str {
        "static_config"
    }

    async fn discover(&self) -> Result<InventorySnapshot, InventoryError> {
        Ok(self.snapshot())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    fn provider(toml: &str) -> StaticConfigProvider {
        StaticConfigProvider::new(Config::from_toml(toml, Path::new("test.toml")).expect("parse"))
    }

    #[test]
    fn declared_entities_become_inventory() {
        let snapshot = provider(
            r#"
            config_version = 1
            environment = "lab"

            [[entities]]
            type = "host"
            name = "fileserver-a"
            display_name = "File server A"
            labels = { rack = "r01" }
            capabilities = ["storage.nfs.server", "observer.peer"]
            addresses = ["192.0.2.10"]
            "#,
        )
        .snapshot();

        assert_eq!(snapshot.entities.len(), 1);
        let entity = &snapshot.entities[0];
        assert_eq!(entity.canonical_name, "fileserver-a");
        assert_eq!(entity.display_name, "File server A");
        assert_eq!(entity.entity_type, EntityType::Host);
        assert!(entity.capabilities.has("storage.nfs.server"));
        assert_eq!(entity.labels.get("rack").unwrap(), "r01");
        assert_eq!(entity.metadata["addresses"][0], "192.0.2.10");
        assert_eq!(entity.discovery_sources, vec![DiscoverySource::StaticConfig]);
    }

    #[test]
    fn non_default_ports_are_carried_into_metadata() {
        // Without this a host whose SSH runs on 2222 would be probed on 22 and
        // reported down, which is a fault Sentinel would have invented.
        let snapshot = provider(
            r#"
            config_version = 1
            environment = "lab"

            [[entities]]
            type = "host"
            name = "fileserver-a"
            addresses = ["192.0.2.10"]
            ports = { ssh = 2222, nfs = 2049 }
            "#,
        )
        .snapshot();

        let entity = &snapshot.entities[0];
        assert_eq!(entity.metadata["ports"]["ssh"], 2222);
        assert_eq!(entity.metadata["ports"]["nfs"], 2049);
    }

    #[test]
    fn ports_do_not_affect_identity() {
        let with = provider(
            "config_version = 1\nenvironment = \"lab\"\n[[entities]]\ntype = \"host\"\nname = \"a\"\nports = { ssh = 2222 }\n",
        )
        .snapshot();
        let without =
            provider("config_version = 1\nenvironment = \"lab\"\n[[entities]]\ntype = \"host\"\nname = \"a\"\n")
                .snapshot();
        assert_eq!(with.entities[0].id, without.entities[0].id);
    }

    #[test]
    fn an_address_is_metadata_and_never_identity() {
        let with_address = provider(
            r#"
            config_version = 1
            environment = "lab"
            [[entities]]
            type = "host"
            name = "node-a"
            addresses = ["192.0.2.1"]
            "#,
        )
        .snapshot();
        let without_address = provider(
            r#"
            config_version = 1
            environment = "lab"
            [[entities]]
            type = "host"
            name = "node-a"
            "#,
        )
        .snapshot();

        assert_eq!(with_address.entities[0].id, without_address.entities[0].id);
    }

    #[test]
    fn declared_dependencies_resolve_to_entity_ids() {
        let snapshot = provider(
            r#"
            config_version = 1
            environment = "lab"

            [[entities]]
            type = "host"
            name = "compute-a"

            [[entities]]
            type = "storage"
            name = "shared-a"

            [[dependencies]]
            from = "host/compute-a"
            to = "storage/shared-a"
            type = "uses_storage"
            criticality = "important"
            "#,
        )
        .snapshot();

        assert_eq!(snapshot.dependencies.len(), 1);
        let edge = &snapshot.dependencies[0];
        assert_eq!(
            edge.source,
            EntityKey::new("lab", EntityType::Host, "compute-a").entity_id()
        );
        assert_eq!(
            edge.target,
            EntityKey::new("lab", EntityType::Storage, "shared-a").entity_id()
        );
        assert_eq!(edge.dependency_type, DependencyType::UsesStorage);
        assert_eq!(edge.criticality, Criticality::Important);
    }

    #[test]
    fn an_unparseable_entity_type_is_skipped_without_failing_the_cycle() {
        let config = Config {
            config_version: 1,
            environment: "lab".into(),
            entities: vec![crate::config::EntityConfig {
                entity_type: "spaceship".into(),
                name: "x".into(),
                display_name: None,
                cluster: None,
                labels: Default::default(),
                capabilities: vec![],
                addresses: vec![],
                ports: Default::default(),
            }],
            ..Config::default()
        };
        let snapshot = StaticConfigProvider::new(config).snapshot();
        assert!(snapshot.entities.is_empty());
    }

    #[test]
    fn a_malformed_dependency_reference_is_skipped() {
        let config = Config {
            config_version: 1,
            environment: "lab".into(),
            dependencies: vec![crate::config::DependencyConfig {
                from: "not-a-reference".into(),
                to: "host/b".into(),
                dependency_type: "depends_on".into(),
                criticality: "critical".into(),
            }],
            ..Config::default()
        };
        assert!(StaticConfigProvider::new(config).snapshot().dependencies.is_empty());
    }

    #[tokio::test]
    async fn the_provider_reports_its_name_and_discovers_asynchronously() {
        let provider = provider("config_version = 1\nenvironment = \"lab\"\n");
        assert_eq!(provider.name(), "static_config");
        assert!(provider.discover().await.expect("discover").is_empty());
    }
}
