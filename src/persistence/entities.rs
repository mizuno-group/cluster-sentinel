//! Reading and writing inventory.
//!
//! Writes are upserts keyed on the entity id, which is derived from the natural
//! key, so re-running discovery converges instead of accumulating duplicates.
//!
//! Entities are never deleted here. Lifecycle state changes; rows stay
//! (SPEC.md §35).

use std::collections::BTreeMap;

use sqlx::Row;

use crate::capability::{Capability, CapabilitySet};
use crate::dependency::{Criticality, DependencyEdge, DependencyType};
use crate::entity::{DiscoverySource, EntityId, EntityType, LifecycleState, ManagedEntity};
use crate::inventory::Inventory;
use crate::time::{now, parse_rfc3339, to_rfc3339, Timestamp};

use super::{SqliteStore, StoreError};

impl SqliteStore {
    /// Write one entity and its labels, capabilities and discovery sources.
    pub async fn save_entity(&self, entity: &ManagedEntity) -> Result<(), StoreError> {
        let mut tx = self.pool().begin().await?;
        save_entity_tx(&mut tx, entity).await?;
        tx.commit().await?;
        Ok(())
    }

    /// Write a whole inventory in one transaction.
    ///
    /// All or nothing: a discovery cycle interrupted halfway must not leave the
    /// graph referring to entities that were never written.
    pub async fn save_inventory(&self, inventory: &Inventory) -> Result<(), StoreError> {
        let mut tx = self.pool().begin().await?;
        for entity in inventory.entities() {
            save_entity_tx(&mut tx, entity).await?;
        }
        for edge in inventory.graph().edges() {
            save_dependency_tx(&mut tx, edge).await?;
        }
        tx.commit().await?;
        Ok(())
    }

    /// Bring one provider's capability claims for an entity up to date.
    ///
    /// Capabilities are unioned across providers, because each knows different
    /// things about a host. But a provider that *stops* claiming a capability
    /// it used to claim has said something, and without this the claim would
    /// live forever: a node whose GPUs were removed would keep `gpu.nvidia`,
    /// and GPU probes would keep failing against hardware that is not there.
    ///
    /// Only rows attributed to `source` are removed, so one provider going
    /// quiet cannot delete another's findings.
    pub async fn reconcile_capabilities(
        &self,
        entity: EntityId,
        source: &DiscoverySource,
        capabilities: &CapabilitySet,
    ) -> Result<u64, StoreError> {
        let id = entity.to_string();
        let source = source.as_str();
        let timestamp = to_rfc3339(now());
        let mut tx = self.pool().begin().await?;

        let kept: Vec<String> = capabilities.iter().map(|c| c.as_str().to_string()).collect();
        let placeholders = if kept.is_empty() {
            String::new()
        } else {
            format!(" AND capability NOT IN ({})", vec!["?"; kept.len()].join(", "))
        };

        let delete_sql =
            format!("DELETE FROM entity_capabilities WHERE entity_id = ? AND discovery_source = ?{placeholders}");
        let mut delete = sqlx::query(&delete_sql).bind(&id).bind(&source);
        for capability in &kept {
            delete = delete.bind(capability);
        }
        let removed = delete.execute(&mut *tx).await?.rows_affected();

        for capability in capabilities.iter() {
            sqlx::query(
                "INSERT INTO entity_capabilities (entity_id, capability, resolution_reason, discovery_source,
                                                  first_seen_at, last_seen_at)
                 VALUES (?, ?, 'discovered', ?, ?, ?)
                 ON CONFLICT(entity_id, capability) DO UPDATE SET
                     discovery_source = excluded.discovery_source,
                     last_seen_at = excluded.last_seen_at",
            )
            .bind(&id)
            .bind(capability.as_str())
            .bind(&source)
            .bind(&timestamp)
            .bind(&timestamp)
            .execute(&mut *tx)
            .await?;
        }

        tx.commit().await?;
        Ok(removed)
    }

    /// Load every entity and dependency edge in an environment.
    pub async fn load_inventory(&self, environment: &str) -> Result<Inventory, StoreError> {
        let mut inventory = Inventory::new();
        for entity in self.load_entities(environment).await? {
            inventory.insert_entity(entity);
        }
        for edge in self.load_dependencies(environment).await? {
            inventory.insert_dependency(edge);
        }
        Ok(inventory)
    }

    /// Load every entity in an environment, whatever its lifecycle state.
    pub async fn load_entities(&self, environment: &str) -> Result<Vec<ManagedEntity>, StoreError> {
        let rows = sqlx::query(
            "SELECT id, environment, cluster, entity_type, canonical_name, display_name,
                    metadata, lifecycle_state, created_at, updated_at
             FROM entities WHERE environment = ? ORDER BY entity_type, canonical_name",
        )
        .bind(environment)
        .fetch_all(self.pool())
        .await?;

        let mut entities = Vec::with_capacity(rows.len());
        for row in rows {
            let id: String = row.try_get("id")?;
            let entity_id = id.parse::<EntityId>().map_err(|e| StoreError::Decode {
                kind: "entity id",
                detail: e.to_string(),
            })?;

            let entity_type_text: String = row.try_get("entity_type")?;
            let entity_type = EntityType::parse(&entity_type_text).ok_or_else(|| StoreError::Decode {
                kind: "entity type",
                detail: entity_type_text.clone(),
            })?;

            let lifecycle_text: String = row.try_get("lifecycle_state")?;
            let metadata_text: String = row.try_get("metadata")?;

            entities.push(ManagedEntity {
                id: entity_id,
                environment: row.try_get("environment")?,
                cluster: row.try_get("cluster")?,
                entity_type,
                canonical_name: row.try_get("canonical_name")?,
                display_name: row.try_get("display_name")?,
                labels: BTreeMap::new(),
                metadata: serde_json::from_str(&metadata_text).unwrap_or(serde_json::Value::Null),
                capabilities: CapabilitySet::new(),
                // An unrecognised lifecycle value must not hide the entity;
                // treat it as stale so it is visible but not counted active.
                lifecycle_state: LifecycleState::parse(&lifecycle_text).unwrap_or(LifecycleState::Stale),
                discovery_sources: Vec::new(),
                created_at: decode_time(row.try_get("created_at")?)?,
                updated_at: decode_time(row.try_get("updated_at")?)?,
            });
        }

        let by_id: BTreeMap<EntityId, usize> = entities.iter().enumerate().map(|(i, e)| (e.id, i)).collect();

        for row in sqlx::query(
            "SELECT l.entity_id, l.key, l.value FROM entity_labels l
             JOIN entities e ON e.id = l.entity_id WHERE e.environment = ?",
        )
        .bind(environment)
        .fetch_all(self.pool())
        .await?
        {
            let id: String = row.try_get("entity_id")?;
            if let Some(&index) = id.parse::<EntityId>().ok().and_then(|id| by_id.get(&id)) {
                entities[index]
                    .labels
                    .insert(row.try_get("key")?, row.try_get("value")?);
            }
        }

        for row in sqlx::query(
            "SELECT c.entity_id, c.capability FROM entity_capabilities c
             JOIN entities e ON e.id = c.entity_id WHERE e.environment = ?",
        )
        .bind(environment)
        .fetch_all(self.pool())
        .await?
        {
            let id: String = row.try_get("entity_id")?;
            if let Some(&index) = id.parse::<EntityId>().ok().and_then(|id| by_id.get(&id)) {
                entities[index]
                    .capabilities
                    .insert(Capability::new(row.try_get::<String, _>("capability")?));
            }
        }

        for row in sqlx::query(
            "SELECT d.entity_id, d.source FROM entity_discovery_sources d
             JOIN entities e ON e.id = d.entity_id WHERE e.environment = ?",
        )
        .bind(environment)
        .fetch_all(self.pool())
        .await?
        {
            let id: String = row.try_get("entity_id")?;
            if let Some(&index) = id.parse::<EntityId>().ok().and_then(|id| by_id.get(&id)) {
                entities[index]
                    .discovery_sources
                    .push(DiscoverySource::parse(&row.try_get::<String, _>("source")?));
            }
        }

        Ok(entities)
    }

    /// Load every dependency edge whose endpoints are in this environment.
    pub async fn load_dependencies(&self, environment: &str) -> Result<Vec<DependencyEdge>, StoreError> {
        let rows = sqlx::query(
            "SELECT d.id, d.source_entity_id, d.target_entity_id, d.dependency_type, d.criticality,
                    d.metadata, d.discovery_source, d.first_seen_at, d.last_seen_at
             FROM dependencies d
             JOIN entities e ON e.id = d.source_entity_id
             WHERE e.environment = ?",
        )
        .bind(environment)
        .fetch_all(self.pool())
        .await?;

        let mut edges = Vec::with_capacity(rows.len());
        for row in rows {
            let decode = |column: &str, value: String| {
                value.parse::<EntityId>().map_err(|e| StoreError::Decode {
                    kind: "dependency endpoint",
                    detail: format!("{column}: {e}"),
                })
            };
            let metadata_text: String = row.try_get("metadata")?;
            let criticality_text: String = row.try_get("criticality")?;

            edges.push(DependencyEdge {
                id: row
                    .try_get::<String, _>("id")?
                    .parse()
                    .map_err(|e: uuid::Error| StoreError::Decode {
                        kind: "dependency id",
                        detail: e.to_string(),
                    })?,
                source: decode("source", row.try_get("source_entity_id")?)?,
                target: decode("target", row.try_get("target_entity_id")?)?,
                dependency_type: DependencyType::parse(&row.try_get::<String, _>("dependency_type")?),
                criticality: Criticality::parse(&criticality_text).unwrap_or(Criticality::Critical),
                metadata: serde_json::from_str(&metadata_text).unwrap_or(serde_json::Value::Null),
                discovery_source: DiscoverySource::parse(&row.try_get::<String, _>("discovery_source")?),
                first_seen_at: decode_time(row.try_get("first_seen_at")?)?,
                last_seen_at: decode_time(row.try_get("last_seen_at")?)?,
            });
        }
        Ok(edges)
    }
}

async fn save_entity_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    entity: &ManagedEntity,
) -> Result<(), StoreError> {
    let id = entity.id.to_string();

    sqlx::query(
        "INSERT INTO entities (id, environment, cluster, entity_type, canonical_name, display_name,
                               metadata, lifecycle_state, created_at, updated_at)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
         ON CONFLICT(id) DO UPDATE SET
             cluster = excluded.cluster,
             display_name = excluded.display_name,
             metadata = excluded.metadata,
             lifecycle_state = excluded.lifecycle_state,
             updated_at = excluded.updated_at",
    )
    .bind(&id)
    .bind(&entity.environment)
    .bind(&entity.cluster)
    .bind(entity.entity_type.as_str())
    .bind(&entity.canonical_name)
    .bind(&entity.display_name)
    .bind(serde_json::to_string(&entity.metadata).unwrap_or_else(|_| "null".into()))
    .bind(entity.lifecycle_state.as_str())
    .bind(to_rfc3339(entity.created_at))
    .bind(to_rfc3339(entity.updated_at))
    .execute(&mut **tx)
    .await?;

    // Labels are replaced wholesale: an operator removing a label must see it
    // gone. Capabilities and discovery sources accumulate instead, because a
    // provider being briefly silent is not a statement that a capability
    // vanished.
    sqlx::query("DELETE FROM entity_labels WHERE entity_id = ?")
        .bind(&id)
        .execute(&mut **tx)
        .await?;
    for (key, value) in &entity.labels {
        sqlx::query("INSERT INTO entity_labels (entity_id, key, value) VALUES (?, ?, ?)")
            .bind(&id)
            .bind(key)
            .bind(value)
            .execute(&mut **tx)
            .await?;
    }

    let timestamp = to_rfc3339(now());
    let source = entity
        .discovery_sources
        .first()
        .map(DiscoverySource::as_str)
        .unwrap_or_else(|| DiscoverySource::Manual.as_str());

    for capability in entity.capabilities.iter() {
        sqlx::query(
            "INSERT INTO entity_capabilities (entity_id, capability, resolution_reason, discovery_source,
                                              first_seen_at, last_seen_at)
             VALUES (?, ?, 'discovered', ?, ?, ?)
             ON CONFLICT(entity_id, capability) DO UPDATE SET last_seen_at = excluded.last_seen_at",
        )
        .bind(&id)
        .bind(capability.as_str())
        .bind(&source)
        .bind(&timestamp)
        .bind(&timestamp)
        .execute(&mut **tx)
        .await?;
    }

    for discovery_source in &entity.discovery_sources {
        sqlx::query(
            "INSERT INTO entity_discovery_sources (entity_id, source, first_seen_at, last_seen_at)
             VALUES (?, ?, ?, ?)
             ON CONFLICT(entity_id, source) DO UPDATE SET last_seen_at = excluded.last_seen_at",
        )
        .bind(&id)
        .bind(discovery_source.as_str())
        .bind(&timestamp)
        .bind(&timestamp)
        .execute(&mut **tx)
        .await?;
    }

    Ok(())
}

async fn save_dependency_tx(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    edge: &DependencyEdge,
) -> Result<(), StoreError> {
    sqlx::query(
        "INSERT INTO dependencies (id, source_entity_id, target_entity_id, dependency_type, criticality,
                                   metadata, discovery_source, first_seen_at, last_seen_at)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)
         ON CONFLICT(id) DO UPDATE SET
             criticality = excluded.criticality,
             metadata = excluded.metadata,
             discovery_source = excluded.discovery_source,
             last_seen_at = excluded.last_seen_at",
    )
    .bind(edge.id.to_string())
    .bind(edge.source.to_string())
    .bind(edge.target.to_string())
    .bind(edge.dependency_type.as_str())
    .bind(edge.criticality.as_str())
    .bind(serde_json::to_string(&edge.metadata).unwrap_or_else(|_| "null".into()))
    .bind(edge.discovery_source.as_str())
    .bind(to_rfc3339(edge.first_seen_at))
    .bind(to_rfc3339(edge.last_seen_at))
    .execute(&mut **tx)
    .await?;
    Ok(())
}

fn decode_time(value: String) -> Result<Timestamp, StoreError> {
    parse_rfc3339(&value).map_err(|e| StoreError::Decode {
        kind: "timestamp",
        detail: format!("{value}: {e}"),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::inventory::InventorySnapshot;

    async fn store() -> SqliteStore {
        let store = SqliteStore::open_in_memory().await.expect("open");
        store.ensure_environment("lab").await.expect("environment");
        store
    }

    fn host(name: &str) -> ManagedEntity {
        ManagedEntity::new("lab", EntityType::Host, name)
    }

    #[tokio::test]
    async fn an_entity_survives_a_round_trip_with_all_its_facets() {
        let store = store().await;
        let entity = host("node-a")
            .with_display_name("Node A")
            .with_cluster("cluster-a")
            .with_label("rack", "r01")
            .with_capabilities(CapabilitySet::from_iter(["host.metrics", "slurm.compute"]))
            .with_discovery_source(DiscoverySource::Integration("slurm".into()));

        store.save_entity(&entity).await.expect("save");
        let loaded = store.load_entities("lab").await.expect("load");

        assert_eq!(loaded.len(), 1);
        let loaded = &loaded[0];
        assert_eq!(loaded.id, entity.id);
        assert_eq!(loaded.display_name, "Node A");
        assert_eq!(loaded.cluster.as_deref(), Some("cluster-a"));
        assert_eq!(loaded.labels.get("rack").unwrap(), "r01");
        assert!(loaded.capabilities.has("slurm.compute"));
        assert_eq!(
            loaded.discovery_sources,
            vec![DiscoverySource::Integration("slurm".into())]
        );
    }

    #[tokio::test]
    async fn saving_the_same_entity_twice_updates_rather_than_duplicates() {
        let store = store().await;
        store.save_entity(&host("node-a")).await.expect("first");
        store
            .save_entity(&host("node-a").with_display_name("Renamed"))
            .await
            .expect("second");

        let loaded = store.load_entities("lab").await.expect("load");
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].display_name, "Renamed");
    }

    #[tokio::test]
    async fn a_removed_label_is_gone_but_a_previously_seen_capability_is_kept() {
        let store = store().await;
        store
            .save_entity(
                &host("node-a")
                    .with_label("rack", "r01")
                    .with_capabilities(CapabilitySet::from_iter(["gpu.nvidia"])),
            )
            .await
            .expect("first");
        store.save_entity(&host("node-a")).await.expect("second");

        let loaded = &store.load_entities("lab").await.expect("load")[0];
        assert!(
            loaded.labels.is_empty(),
            "an operator removing a label must see it removed"
        );
        assert!(
            loaded.capabilities.has("gpu.nvidia"),
            "a provider being briefly silent is not proof the GPUs left"
        );
    }

    #[tokio::test]
    async fn a_capability_a_provider_stops_claiming_is_retracted() {
        // A node whose GPUs were removed must stop claiming gpu.nvidia, or GPU
        // probes keep failing against hardware that is not there.
        let store = store().await;
        let entity = host("node-a").with_discovery_source(DiscoverySource::AgentRegistration);
        store.save_entity(&entity).await.expect("save");

        let agent = DiscoverySource::AgentRegistration;
        store
            .reconcile_capabilities(
                entity.id,
                &agent,
                &CapabilitySet::from_iter(["host.metrics", "gpu.nvidia"]),
            )
            .await
            .expect("first");
        assert!(store.load_entities("lab").await.expect("load")[0]
            .capabilities
            .has("gpu.nvidia"));

        let removed = store
            .reconcile_capabilities(entity.id, &agent, &CapabilitySet::from_iter(["host.metrics"]))
            .await
            .expect("second");

        assert_eq!(removed, 1);
        let loaded = &store.load_entities("lab").await.expect("load")[0];
        assert!(
            !loaded.capabilities.has("gpu.nvidia"),
            "the retracted claim must be gone"
        );
        assert!(loaded.capabilities.has("host.metrics"), "the others stand");
    }

    #[tokio::test]
    async fn one_provider_going_quiet_does_not_delete_anothers_findings() {
        let store = store().await;
        let entity = host("node-a");
        store.save_entity(&entity).await.expect("save");

        let slurm = DiscoverySource::Integration("slurm".into());
        store
            .reconcile_capabilities(entity.id, &slurm, &CapabilitySet::from_iter(["slurm.compute"]))
            .await
            .expect("slurm");
        store
            .reconcile_capabilities(
                entity.id,
                &DiscoverySource::AgentRegistration,
                &CapabilitySet::from_iter(["gpu.nvidia"]),
            )
            .await
            .expect("agent");

        // The agent reports again with nothing; Slurm's claim must survive.
        store
            .reconcile_capabilities(entity.id, &DiscoverySource::AgentRegistration, &CapabilitySet::new())
            .await
            .expect("agent again");

        let loaded = &store.load_entities("lab").await.expect("load")[0];
        assert!(loaded.capabilities.has("slurm.compute"), "Slurm never retracted this");
        assert!(!loaded.capabilities.has("gpu.nvidia"));
    }

    #[tokio::test]
    async fn reconciling_an_empty_set_from_an_unknown_source_removes_nothing() {
        let store = store().await;
        let entity = host("node-a");
        store.save_entity(&entity).await.expect("save");
        store
            .reconcile_capabilities(
                entity.id,
                &DiscoverySource::Integration("slurm".into()),
                &CapabilitySet::from_iter(["slurm.compute"]),
            )
            .await
            .expect("slurm");

        let removed = store
            .reconcile_capabilities(entity.id, &DiscoverySource::StaticConfig, &CapabilitySet::new())
            .await
            .expect("static");
        assert_eq!(removed, 0);
        assert!(store.load_entities("lab").await.expect("load")[0]
            .capabilities
            .has("slurm.compute"));
    }

    #[tokio::test]
    async fn a_stale_entity_is_persisted_not_deleted() {
        let store = store().await;
        let mut entity = host("node-a");
        entity.lifecycle_state = LifecycleState::Stale;
        store.save_entity(&entity).await.expect("save");

        let loaded = store.load_entities("lab").await.expect("load");
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].lifecycle_state, LifecycleState::Stale);
    }

    #[tokio::test]
    async fn a_whole_inventory_round_trips_including_its_graph() {
        let store = store().await;
        let mut snapshot = InventorySnapshot::new(DiscoverySource::StaticConfig);
        let (a, s) = (host("node-a"), ManagedEntity::new("lab", EntityType::Storage, "shared"));
        let (a_id, s_id) = (a.id, s.id);
        snapshot.add_entity(a);
        snapshot.add_entity(s);
        snapshot.add_dependency(
            DependencyEdge::new(a_id, s_id, DependencyType::UsesStorage).with_criticality(Criticality::Important),
        );

        let mut inventory = Inventory::new();
        inventory.merge(&snapshot);
        store.save_inventory(&inventory).await.expect("save");

        let loaded = store.load_inventory("lab").await.expect("load");
        assert_eq!(loaded.len(), 2);
        assert_eq!(loaded.graph().len(), 1);
        let edge = &loaded.graph().edges()[0];
        assert_eq!(edge.source, a_id);
        assert_eq!(edge.target, s_id);
        assert_eq!(edge.dependency_type, DependencyType::UsesStorage);
        assert_eq!(edge.criticality, Criticality::Important);
    }

    #[tokio::test]
    async fn a_cycle_round_trips_through_the_database() {
        let store = store().await;
        let mut inventory = Inventory::new();
        let mut snapshot = InventorySnapshot::new(DiscoverySource::StaticConfig);
        let (a, b) = (host("a"), host("b"));
        let (a_id, b_id) = (a.id, b.id);
        snapshot.add_entity(a);
        snapshot.add_entity(b);
        snapshot.add_dependency(DependencyEdge::new(a_id, b_id, DependencyType::DependsOn));
        snapshot.add_dependency(DependencyEdge::new(b_id, a_id, DependencyType::DependsOn));
        inventory.merge(&snapshot);

        store.save_inventory(&inventory).await.expect("save");
        let loaded = store.load_inventory("lab").await.expect("load");
        assert_eq!(loaded.graph().len(), 2);
        assert_eq!(
            loaded.graph().upstream(a_id, None).len(),
            1,
            "traversal still terminates"
        );
    }

    #[tokio::test]
    async fn environments_are_isolated_from_one_another() {
        let store = store().await;
        store.ensure_environment("other").await.expect("environment");
        store.save_entity(&host("node-a")).await.expect("save lab");
        store
            .save_entity(&ManagedEntity::new("other", EntityType::Host, "node-a"))
            .await
            .expect("save other");

        assert_eq!(store.load_entities("lab").await.expect("load").len(), 1);
        assert_eq!(store.load_entities("other").await.expect("load").len(), 1);
    }

    #[tokio::test]
    async fn saving_an_inventory_is_all_or_nothing() {
        // An edge whose target was never written must abort the transaction
        // rather than leave a dangling reference.
        let store = store().await;
        let mut inventory = Inventory::new();
        let a = host("node-a");
        let a_id = a.id;
        inventory.insert_entity(a);
        inventory.insert_dependency(DependencyEdge::new(
            a_id,
            ManagedEntity::new("lab", EntityType::Storage, "phantom").id,
            DependencyType::UsesStorage,
        ));

        assert!(store.save_inventory(&inventory).await.is_err());
        assert!(
            store.load_entities("lab").await.expect("load").is_empty(),
            "nothing was committed"
        );
    }
}
