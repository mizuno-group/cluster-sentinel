//! Storing observations and derived state.
//!
//! Observations are append-only and their ids come from the producing agent, so
//! ingestion is idempotent: replaying a spool inserts nothing twice
//! (IMPLEMENTATION.md §44, §45).

use std::collections::BTreeMap;

use sqlx::Row;

use crate::entity::EntityId;
use crate::observation::{Observation, ObservationId, ProbeStatus};
use crate::probes::ProbeId;
use crate::state::{Classification, ComponentState, EntityState, Health, StateComponent, StateTransition};
use crate::time::{now, parse_rfc3339, to_rfc3339};

use super::{SqliteStore, StoreError};

/// How many rows an ingestion actually inserted.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct IngestOutcome {
    /// Observations stored for the first time.
    pub inserted: usize,
    /// Observations already present, and therefore skipped.
    pub duplicates: usize,
}

impl SqliteStore {
    /// Store observations, ignoring ones already present.
    pub async fn ingest_observations(&self, observations: &[Observation]) -> Result<IngestOutcome, StoreError> {
        let mut outcome = IngestOutcome::default();
        let mut tx = self.pool().begin().await?;
        let ingested_at = to_rfc3339(now());

        for observation in observations {
            let result = sqlx::query(
                "INSERT INTO observations (id, probe_id, target_entity_id, observer_entity_id, agent_session_id,
                                           started_at, finished_at, duration_ms, status, payload, evidence,
                                           error_code, error_message, ingested_at)
                 VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
                 ON CONFLICT(id) DO NOTHING",
            )
            .bind(observation.id.to_string())
            .bind(observation.probe_id.as_str())
            .bind(observation.target_entity.to_string())
            .bind(observation.observer_entity.map(|o| o.to_string()))
            .bind(observation.agent_session.map(|s| s.to_string()))
            .bind(to_rfc3339(observation.started_at))
            .bind(to_rfc3339(observation.finished_at))
            .bind(observation.duration_ms as i64)
            .bind(observation.status.as_str())
            .bind(serde_json::to_string(&observation.payload).unwrap_or_else(|_| "null".into()))
            .bind(serde_json::to_string(&observation.evidence).unwrap_or_else(|_| "null".into()))
            .bind(&observation.error_code)
            .bind(&observation.error_message)
            .bind(&ingested_at)
            .execute(&mut *tx)
            .await?;

            if result.rows_affected() > 0 {
                outcome.inserted += 1;
            } else {
                outcome.duplicates += 1;
            }
        }

        tx.commit().await?;
        Ok(outcome)
    }

    /// The most recent observations for one entity, newest first.
    pub async fn recent_observations(&self, entity: EntityId, limit: u32) -> Result<Vec<Observation>, StoreError> {
        let rows = sqlx::query(
            "SELECT id, probe_id, target_entity_id, observer_entity_id, agent_session_id, started_at,
                    finished_at, duration_ms, status, payload, evidence, error_code, error_message
             FROM observations WHERE target_entity_id = ? ORDER BY finished_at DESC, id DESC LIMIT ?",
        )
        .bind(entity.to_string())
        .bind(limit as i64)
        .fetch_all(self.pool())
        .await?;

        rows.iter().map(decode_observation).collect()
    }

    /// The newest observation of each probe, from each observer, for one
    /// entity.
    ///
    /// This exists because "the last N observations" is the wrong window for
    /// diagnosis. Probes run at wildly different cadences -- reachability
    /// every five seconds from every observer, the Slurm node view every five
    /// minutes -- so a fixed-size recent window fills with the fast ones and
    /// evicts the slow ones. A rule comparing the two then sees the slow side
    /// only in the moments just after it lands, and its diagnosis appears and
    /// disappears in step with the eviction rather than with the cluster.
    /// That looks exactly like a flapping fault, and it was reported as one.
    pub async fn latest_observations(&self, entity: EntityId) -> Result<Vec<Observation>, StoreError> {
        let rows = sqlx::query(
            "SELECT id, probe_id, target_entity_id, observer_entity_id, agent_session_id, started_at,
                    finished_at, duration_ms, status, payload, evidence, error_code, error_message
             FROM (
                 SELECT id, probe_id, target_entity_id, observer_entity_id, agent_session_id, started_at,
                        finished_at, duration_ms, status, payload, evidence, error_code, error_message,
                        ROW_NUMBER() OVER (
                            PARTITION BY probe_id, COALESCE(observer_entity_id, '')
                            ORDER BY finished_at DESC, id DESC
                        ) AS rank
                 FROM observations
                 WHERE target_entity_id = ?
             )
             WHERE rank = 1",
        )
        .bind(entity.to_string())
        .fetch_all(self.pool())
        .await?;

        rows.iter().map(decode_observation).collect()
    }

    /// How many observations are stored for one entity.
    pub async fn observation_count(&self, entity: EntityId) -> Result<i64, StoreError> {
        let row = sqlx::query("SELECT COUNT(*) AS n FROM observations WHERE target_entity_id = ?")
            .bind(entity.to_string())
            .fetch_one(self.pool())
            .await?;
        Ok(row.try_get("n")?)
    }

    /// Write an entity's derived state and its component breakdown.
    pub async fn save_entity_state(&self, state: &EntityState) -> Result<(), StoreError> {
        let id = state.entity.to_string();
        let updated_at = to_rfc3339(state.updated_at);
        let mut tx = self.pool().begin().await?;

        for (component, component_state) in &state.components {
            sqlx::query(
                "INSERT INTO entity_states (entity_id, component, health, since, consecutive_failures,
                                            consecutive_successes, evidence, updated_at)
                 VALUES (?, ?, ?, ?, ?, ?, ?, ?)
                 ON CONFLICT(entity_id, component) DO UPDATE SET
                     health = excluded.health,
                     since = excluded.since,
                     consecutive_failures = excluded.consecutive_failures,
                     consecutive_successes = excluded.consecutive_successes,
                     evidence = excluded.evidence,
                     updated_at = excluded.updated_at",
            )
            .bind(&id)
            .bind(component.as_str())
            .bind(component_state.health.as_str())
            .bind(to_rfc3339(component_state.since))
            .bind(component_state.consecutive_failures as i64)
            .bind(component_state.consecutive_successes as i64)
            .bind(serde_json::to_string(&component_state.evidence).unwrap_or_else(|_| "[]".into()))
            .bind(&updated_at)
            .execute(&mut *tx)
            .await?;
        }

        sqlx::query(
            "INSERT INTO entity_overall_states (entity_id, health, classifications, since, updated_at)
             VALUES (?, ?, ?, ?, ?)
             ON CONFLICT(entity_id) DO UPDATE SET
                 health = excluded.health,
                 classifications = excluded.classifications,
                 -- Keep the original `since` while the health is unchanged, so
                 -- an operator can see how long this has been going on.
                 since = CASE WHEN entity_overall_states.health = excluded.health
                              THEN entity_overall_states.since ELSE excluded.since END,
                 updated_at = excluded.updated_at",
        )
        .bind(&id)
        .bind(state.overall.as_str())
        .bind(serde_json::to_string(&state.classifications).unwrap_or_else(|_| "[]".into()))
        .bind(&updated_at)
        .bind(&updated_at)
        .execute(&mut *tx)
        .await?;

        tx.commit().await?;
        Ok(())
    }

    /// Load every derived state in an environment.
    pub async fn load_entity_states(&self, environment: &str) -> Result<BTreeMap<EntityId, EntityState>, StoreError> {
        let mut states: BTreeMap<EntityId, EntityState> = BTreeMap::new();

        for row in sqlx::query(
            "SELECT o.entity_id, o.health, o.classifications, o.updated_at FROM entity_overall_states o
             JOIN entities e ON e.id = o.entity_id WHERE e.environment = ?",
        )
        .bind(environment)
        .fetch_all(self.pool())
        .await?
        {
            let entity = decode_entity_id(row.try_get("entity_id")?)?;
            let classifications_text: String = row.try_get("classifications")?;
            let health_text: String = row.try_get("health")?;

            let mut state = EntityState::unknown(entity);
            state.overall = Health::parse(&health_text).unwrap_or(Health::Unknown);
            state.classifications =
                serde_json::from_str::<Vec<Classification>>(&classifications_text).unwrap_or_default();
            state.updated_at =
                parse_rfc3339(&row.try_get::<String, _>("updated_at")?).map_err(|e| StoreError::Decode {
                    kind: "state timestamp",
                    detail: e.to_string(),
                })?;
            states.insert(entity, state);
        }

        for row in sqlx::query(
            "SELECT s.entity_id, s.component, s.health, s.since, s.consecutive_failures, s.consecutive_successes,
                    s.evidence
             FROM entity_states s JOIN entities e ON e.id = s.entity_id WHERE e.environment = ?",
        )
        .bind(environment)
        .fetch_all(self.pool())
        .await?
        {
            let entity = decode_entity_id(row.try_get("entity_id")?)?;
            let component_text: String = row.try_get("component")?;
            let Some(component) = StateComponent::parse(&component_text) else {
                continue;
            };
            let health_text: String = row.try_get("health")?;
            let evidence_text: String = row.try_get("evidence")?;

            let mut component_state = ComponentState::new(Health::parse(&health_text).unwrap_or(Health::Unknown));
            component_state.since =
                parse_rfc3339(&row.try_get::<String, _>("since")?).map_err(|e| StoreError::Decode {
                    kind: "state timestamp",
                    detail: e.to_string(),
                })?;
            component_state.consecutive_failures = row.try_get::<i64, _>("consecutive_failures")? as u32;
            component_state.consecutive_successes = row.try_get::<i64, _>("consecutive_successes")? as u32;
            component_state.evidence = serde_json::from_str(&evidence_text).unwrap_or_default();

            let state = states.entry(entity).or_insert_with(|| EntityState::unknown(entity));
            // Insert directly rather than through `set_component`, which would
            // recompute the roll-up and overwrite the stored overall health.
            state.components.insert(component, component_state);
        }

        Ok(states)
    }

    /// Record a state transition.
    pub async fn save_state_transition(&self, transition: &StateTransition) -> Result<(), StoreError> {
        sqlx::query(
            "INSERT INTO state_transitions (id, entity_id, component, from_health, to_health, occurred_at, evidence)
             VALUES (?, ?, ?, ?, ?, ?, ?)
             ON CONFLICT(id) DO NOTHING",
        )
        .bind(transition.id.to_string())
        .bind(transition.entity.to_string())
        .bind(transition.component.map(|c| c.as_str()))
        .bind(transition.from.as_str())
        .bind(transition.to.as_str())
        .bind(to_rfc3339(transition.at))
        .bind(serde_json::to_string(&transition.evidence).unwrap_or_else(|_| "[]".into()))
        .execute(self.pool())
        .await?;
        Ok(())
    }

    /// The most recent transitions for one entity, newest first.
    pub async fn recent_transitions(&self, entity: EntityId, limit: u32) -> Result<Vec<StateTransition>, StoreError> {
        let rows = sqlx::query(
            "SELECT id, entity_id, component, from_health, to_health, occurred_at, evidence
             FROM state_transitions WHERE entity_id = ? ORDER BY occurred_at DESC, id DESC LIMIT ?",
        )
        .bind(entity.to_string())
        .bind(limit as i64)
        .fetch_all(self.pool())
        .await?;

        let mut transitions = Vec::with_capacity(rows.len());
        for row in rows {
            let component_text: Option<String> = row.try_get("component")?;
            let evidence_text: String = row.try_get("evidence")?;
            let from_text: String = row.try_get("from_health")?;
            let to_text: String = row.try_get("to_health")?;

            transitions.push(StateTransition {
                id: row
                    .try_get::<String, _>("id")?
                    .parse()
                    .map_err(|e: uuid::Error| StoreError::Decode {
                        kind: "transition id",
                        detail: e.to_string(),
                    })?,
                entity: decode_entity_id(row.try_get("entity_id")?)?,
                component: component_text.as_deref().and_then(StateComponent::parse),
                from: Health::parse(&from_text).unwrap_or(Health::Unknown),
                to: Health::parse(&to_text).unwrap_or(Health::Unknown),
                at: parse_rfc3339(&row.try_get::<String, _>("occurred_at")?).map_err(|e| StoreError::Decode {
                    kind: "transition timestamp",
                    detail: e.to_string(),
                })?,
                evidence: serde_json::from_str(&evidence_text).unwrap_or_default(),
            });
        }
        Ok(transitions)
    }
}

fn decode_entity_id(value: String) -> Result<EntityId, StoreError> {
    value.parse().map_err(|e: uuid::Error| StoreError::Decode {
        kind: "entity id",
        detail: e.to_string(),
    })
}

fn decode_observation(row: &sqlx::sqlite::SqliteRow) -> Result<Observation, StoreError> {
    let status_text: String = row.try_get("status")?;
    let payload_text: String = row.try_get("payload")?;
    let evidence_text: String = row.try_get("evidence")?;
    let observer: Option<String> = row.try_get("observer_entity_id")?;
    let session: Option<String> = row.try_get("agent_session_id")?;

    Ok(Observation {
        id: row
            .try_get::<String, _>("id")?
            .parse::<ObservationId>()
            .map_err(|e| StoreError::Decode {
                kind: "observation id",
                detail: e.to_string(),
            })?,
        probe_id: ProbeId::new(row.try_get::<String, _>("probe_id")?),
        target_entity: decode_entity_id(row.try_get("target_entity_id")?)?,
        observer_entity: observer.map(decode_entity_id).transpose()?,
        agent_session: session.and_then(|s| s.parse().ok()),
        started_at: parse_rfc3339(&row.try_get::<String, _>("started_at")?).map_err(|e| StoreError::Decode {
            kind: "observation timestamp",
            detail: e.to_string(),
        })?,
        finished_at: parse_rfc3339(&row.try_get::<String, _>("finished_at")?).map_err(|e| StoreError::Decode {
            kind: "observation timestamp",
            detail: e.to_string(),
        })?,
        duration_ms: row.try_get::<i64, _>("duration_ms")? as u64,
        status: ProbeStatus::parse(&status_text).unwrap_or(ProbeStatus::Unsupported),
        payload: serde_json::from_str(&payload_text).unwrap_or(serde_json::Value::Null),
        evidence: serde_json::from_str(&evidence_text).unwrap_or(serde_json::Value::Null),
        error_code: row.try_get("error_code")?,
        error_message: row.try_get("error_message")?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::entity::{EntityKey, EntityType, ManagedEntity};

    async fn store_with_entity(name: &str) -> (SqliteStore, EntityId) {
        let store = SqliteStore::open_in_memory().await.expect("open");
        store.ensure_environment("lab").await.expect("environment");
        let entity = ManagedEntity::new("lab", EntityType::Host, name);
        let id = entity.id;
        store.save_entity(&entity).await.expect("save entity");
        (store, id)
    }

    fn observation(target: EntityId, status: ProbeStatus) -> Observation {
        Observation::new(ProbeId::new("test.probe"), target, status)
    }

    #[tokio::test]
    async fn an_observation_round_trips_with_every_field() {
        let (store, entity) = store_with_entity("node-a").await;
        let observer = EntityKey::new("lab", EntityType::Host, "peer-a").entity_id();
        store
            .save_entity(&ManagedEntity::new("lab", EntityType::Host, "peer-a"))
            .await
            .expect("peer");

        let original = observation(entity, ProbeStatus::Failed)
            .with_observer(observer)
            .with_payload(serde_json::json!({"port": 22}))
            .with_error("refused", "connection refused")
            .with_duration_ms(17);

        store
            .ingest_observations(std::slice::from_ref(&original))
            .await
            .expect("ingest");
        let loaded = store.recent_observations(entity, 10).await.expect("load");

        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0], original);
    }

    #[tokio::test]
    async fn a_slow_probe_is_not_evicted_by_a_flood_of_fast_ones() {
        // The bug this query exists for. Reachability runs every five seconds
        // from every observer; the Slurm node view runs every five minutes. A
        // fixed-size recent window fills with the first and loses the second,
        // so a rule comparing what Slurm configures against what the hardware
        // reports saw the Slurm side only in the moments right after it
        // landed. Its diagnosis then appeared and disappeared in step with the
        // eviction, which reaches an operator as a fault that keeps opening
        // and immediately resolving.
        let (store, entity) = store_with_entity("node-a").await;

        let mut flood = vec![Observation::new(ProbeId::new("slurm.node"), entity, ProbeStatus::Ok)
            .with_payload(serde_json::json!({"configured_gpu_count": 1}))];
        for _ in 0..200 {
            flood.push(Observation::new(ProbeId::new("network.tcp"), entity, ProbeStatus::Ok));
        }
        store.ingest_observations(&flood).await.expect("ingest");

        let recent = store.recent_observations(entity, 32).await.expect("recent");
        assert!(
            !recent.iter().any(|o| o.probe_id.as_str() == "slurm.node"),
            "the old window really did lose it, so this test is not vacuous"
        );

        let latest = store.latest_observations(entity).await.expect("latest");
        let slurm = latest
            .iter()
            .find(|o| o.probe_id.as_str() == "slurm.node")
            .expect("the slow probe survives");
        assert_eq!(slurm.payload["configured_gpu_count"], 1);
    }

    #[tokio::test]
    async fn the_latest_is_kept_per_probe_and_per_observer() {
        let (store, entity) = store_with_entity("node-a").await;
        let mut observers = Vec::new();
        for name in ["peer-a", "peer-b"] {
            let peer = ManagedEntity::new("lab", EntityType::Host, name);
            observers.push(peer.id);
            store.save_entity(&peer).await.expect("peer");
        }

        let mut observations = Vec::new();
        for observer in &observers {
            for status in [ProbeStatus::Failed, ProbeStatus::Ok] {
                observations
                    .push(Observation::new(ProbeId::new("network.tcp"), entity, status).with_observer(*observer));
            }
        }
        store.ingest_observations(&observations).await.expect("ingest");

        let latest = store.latest_observations(entity).await.expect("latest");
        assert_eq!(latest.len(), 2, "one per observer, not one overall: {latest:#?}");
        assert!(
            latest.iter().all(|o| o.status == ProbeStatus::Ok),
            "and it is each observer's newest"
        );
    }

    #[tokio::test]
    async fn an_entity_with_no_observations_yields_none() {
        let (store, entity) = store_with_entity("node-a").await;
        assert!(store.latest_observations(entity).await.expect("latest").is_empty());
    }

    #[tokio::test]
    async fn replaying_a_spool_inserts_nothing_twice() {
        // IMPLEMENTATION.md §45: reconnection must be idempotent.
        let (store, entity) = store_with_entity("node-a").await;
        let batch = vec![
            observation(entity, ProbeStatus::Ok),
            observation(entity, ProbeStatus::Ok),
        ];

        let first = store.ingest_observations(&batch).await.expect("first");
        assert_eq!(
            first,
            IngestOutcome {
                inserted: 2,
                duplicates: 0
            }
        );

        let replay = store.ingest_observations(&batch).await.expect("replay");
        assert_eq!(
            replay,
            IngestOutcome {
                inserted: 0,
                duplicates: 2
            }
        );
        assert_eq!(store.observation_count(entity).await.expect("count"), 2);
    }

    #[tokio::test]
    async fn a_partially_overlapping_replay_inserts_only_what_is_new() {
        let (store, entity) = store_with_entity("node-a").await;
        let first = observation(entity, ProbeStatus::Ok);
        store
            .ingest_observations(std::slice::from_ref(&first))
            .await
            .expect("first");

        let outcome = store
            .ingest_observations(&[first, observation(entity, ProbeStatus::Failed)])
            .await
            .expect("second");
        assert_eq!(
            outcome,
            IngestOutcome {
                inserted: 1,
                duplicates: 1
            }
        );
    }

    #[tokio::test]
    async fn observations_come_back_newest_first_and_respect_the_limit() {
        let (store, entity) = store_with_entity("node-a").await;
        let mut batch = Vec::new();
        for minute in 0..5 {
            let at = now() - chrono::Duration::minutes(minute);
            batch.push(observation(entity, ProbeStatus::Ok).with_times(at, at));
        }
        store.ingest_observations(&batch).await.expect("ingest");

        let loaded = store.recent_observations(entity, 3).await.expect("load");
        assert_eq!(loaded.len(), 3);
        assert!(loaded[0].finished_at >= loaded[1].finished_at);
        assert!(loaded[1].finished_at >= loaded[2].finished_at);
    }

    #[tokio::test]
    async fn ingesting_an_empty_batch_is_harmless() {
        let (store, _) = store_with_entity("node-a").await;
        assert_eq!(
            store.ingest_observations(&[]).await.expect("ingest"),
            IngestOutcome::default()
        );
    }

    #[tokio::test]
    async fn entity_state_round_trips_with_its_components() {
        let (store, entity) = store_with_entity("node-a").await;
        let mut state = EntityState::unknown(entity);
        state.set_component(StateComponent::Scheduler, ComponentState::new(Health::Degraded));
        state.set_component(StateComponent::Ssh, ComponentState::new(Health::Healthy));
        state.classify(crate::state::classification::SCHEDULER_DEGRADED);

        store.save_entity_state(&state).await.expect("save");
        let loaded = store.load_entity_states("lab").await.expect("load");

        let loaded = loaded.get(&entity).expect("state");
        assert_eq!(loaded.overall, Health::Degraded);
        assert_eq!(loaded.component(StateComponent::Scheduler), Health::Degraded);
        assert_eq!(loaded.component(StateComponent::Ssh), Health::Healthy);
        assert!(loaded.has_classification(crate::state::classification::SCHEDULER_DEGRADED));
    }

    #[tokio::test]
    async fn unchanged_health_keeps_its_original_since_timestamp() {
        // "Degraded for three hours" is a different situation from "degraded
        // for three seconds", and the difference must survive a rewrite.
        let (store, entity) = store_with_entity("node-a").await;
        let mut state = EntityState::unknown(entity);
        state.set_component(StateComponent::Scheduler, ComponentState::new(Health::Degraded));
        store.save_entity_state(&state).await.expect("first");

        let first_since: String = sqlx::query("SELECT since FROM entity_overall_states WHERE entity_id = ?")
            .bind(entity.to_string())
            .fetch_one(store.pool())
            .await
            .expect("query")
            .get("since");

        state.updated_at = now() + chrono::Duration::seconds(30);
        store.save_entity_state(&state).await.expect("second");

        let second_since: String = sqlx::query("SELECT since FROM entity_overall_states WHERE entity_id = ?")
            .bind(entity.to_string())
            .fetch_one(store.pool())
            .await
            .expect("query")
            .get("since");
        assert_eq!(first_since, second_since);
    }

    #[tokio::test]
    async fn a_health_change_restarts_the_since_timestamp() {
        let (store, entity) = store_with_entity("node-a").await;
        let mut state = EntityState::unknown(entity);
        state.set_component(StateComponent::Scheduler, ComponentState::new(Health::Healthy));
        store.save_entity_state(&state).await.expect("first");

        let before: String = sqlx::query("SELECT since FROM entity_overall_states WHERE entity_id = ?")
            .bind(entity.to_string())
            .fetch_one(store.pool())
            .await
            .expect("query")
            .get("since");

        state.set_component(StateComponent::Scheduler, ComponentState::new(Health::Unavailable));
        state.updated_at = now() + chrono::Duration::seconds(30);
        store.save_entity_state(&state).await.expect("second");

        let after: String = sqlx::query("SELECT since FROM entity_overall_states WHERE entity_id = ?")
            .bind(entity.to_string())
            .fetch_one(store.pool())
            .await
            .expect("query")
            .get("since");
        assert_ne!(before, after);
    }

    #[tokio::test]
    async fn state_transitions_round_trip_newest_first() {
        let (store, entity) = store_with_entity("node-a").await;
        let older = StateTransition::new(entity, Some(StateComponent::Ssh), Health::Healthy, Health::Degraded);
        let mut newer = StateTransition::new(entity, Some(StateComponent::Ssh), Health::Degraded, Health::Unavailable);
        newer.at = now() + chrono::Duration::seconds(60);

        store.save_state_transition(&older).await.expect("older");
        store.save_state_transition(&newer).await.expect("newer");

        let loaded = store.recent_transitions(entity, 10).await.expect("load");
        assert_eq!(loaded.len(), 2);
        assert_eq!(loaded[0].to, Health::Unavailable);
        assert_eq!(loaded[1].to, Health::Degraded);
    }
}
