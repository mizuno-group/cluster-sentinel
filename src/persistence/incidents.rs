//! Storing incidents and their evidence.
//!
//! Evidence preservation is the point (SPEC.md §103). A reboot, a log rotation
//! or a spool eviction must not take with it the reason an incident was
//! declared — that is exactly what makes a post-mortem impossible and is one of
//! the failure modes Sentinel exists to prevent.

use sqlx::Row;

use crate::diagnosis::{Confidence, Diagnosis, DiagnosisType, RuleId};
use crate::entity::EntityId;
use crate::incident::{Incident, IncidentStatus, Severity, TimelineEvent};
use crate::observation::ObservationId;
use crate::time::{now, parse_rfc3339, to_rfc3339};

use super::{SqliteStore, StoreError};

/// Roles an entity plays in an incident.
const ROLE_AFFECTED: &str = "affected";
const ROLE_ROOT: &str = "suspected_root";

impl SqliteStore {
    /// Write an incident and everything hanging off it.
    pub async fn save_incident(&self, environment: &str, incident: &Incident) -> Result<(), StoreError> {
        let id = incident.id.to_string();
        let mut tx = self.pool().begin().await?;

        sqlx::query(
            "INSERT INTO incidents (id, environment, fingerprint, status, severity, started_at, ended_at, updated_at)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?)
             ON CONFLICT(id) DO UPDATE SET
                 status = excluded.status,
                 severity = excluded.severity,
                 ended_at = excluded.ended_at,
                 updated_at = excluded.updated_at",
        )
        .bind(&id)
        .bind(environment)
        .bind(&incident.fingerprint)
        .bind(incident.status.as_str())
        .bind(incident.severity.as_str())
        .bind(to_rfc3339(incident.started_at))
        .bind(incident.ended_at.map(to_rfc3339))
        .bind(to_rfc3339(now()))
        .execute(&mut *tx)
        .await?;

        // Entities and evidence are replaced wholesale: they describe the
        // incident as it stands now, and a stale entity would misdirect.
        sqlx::query("DELETE FROM incident_entities WHERE incident_id = ?")
            .bind(&id)
            .execute(&mut *tx)
            .await?;
        for (entities, role) in [
            (&incident.affected_entities, ROLE_AFFECTED),
            (&incident.suspected_root_entities, ROLE_ROOT),
        ] {
            for entity in entities {
                sqlx::query(
                    "INSERT INTO incident_entities (incident_id, entity_id, role) VALUES (?, ?, ?)
                     ON CONFLICT DO NOTHING",
                )
                .bind(&id)
                .bind(entity.to_string())
                .bind(role)
                .execute(&mut *tx)
                .await?;
            }
        }

        // Evidence only ever accumulates. Removing an observation from an
        // incident would erase the reason it was raised.
        for observation in &incident.evidence {
            sqlx::query(
                "INSERT INTO incident_evidence (incident_id, observation_id) VALUES (?, ?)
                 ON CONFLICT DO NOTHING",
            )
            .bind(&id)
            .bind(observation.to_string())
            .execute(&mut *tx)
            .await?;
        }

        sqlx::query("DELETE FROM diagnoses WHERE incident_id = ?")
            .bind(&id)
            .execute(&mut *tx)
            .await?;
        for diagnosis in &incident.diagnoses {
            let diagnosis_id = diagnosis.id.to_string();
            sqlx::query(
                "INSERT INTO diagnoses (id, diagnosis_type, rule_id, confidence, summary, recommended_actions,
                                        incident_id, created_at)
                 VALUES (?, ?, ?, ?, ?, ?, ?, ?)
                 ON CONFLICT(id) DO UPDATE SET incident_id = excluded.incident_id",
            )
            .bind(&diagnosis_id)
            .bind(diagnosis.diagnosis_type.as_str())
            .bind(diagnosis.rule_id.as_str())
            .bind(diagnosis.confidence.as_str())
            .bind(&diagnosis.summary)
            .bind(serde_json::to_string(&diagnosis.recommended_actions).unwrap_or_else(|_| "[]".into()))
            .bind(&id)
            .bind(to_rfc3339(diagnosis.created_at))
            .execute(&mut *tx)
            .await?;

            for (entities, role) in [
                (&diagnosis.affected_entities, ROLE_AFFECTED),
                (&diagnosis.suspected_root_entities, ROLE_ROOT),
            ] {
                for entity in entities {
                    sqlx::query(
                        "INSERT INTO diagnosis_entities (diagnosis_id, entity_id, role) VALUES (?, ?, ?)
                         ON CONFLICT DO NOTHING",
                    )
                    .bind(&diagnosis_id)
                    .bind(entity.to_string())
                    .bind(role)
                    .execute(&mut *tx)
                    .await?;
                }
            }

            for observation in &diagnosis.evidence {
                sqlx::query(
                    "INSERT INTO diagnosis_evidence (diagnosis_id, observation_id) VALUES (?, ?)
                     ON CONFLICT DO NOTHING",
                )
                .bind(&diagnosis_id)
                .bind(observation.to_string())
                .execute(&mut *tx)
                .await?;
            }
        }

        // The timeline is append-only: it is the record of what happened, and
        // rewriting it would defeat the purpose.
        for event in &incident.timeline {
            sqlx::query(
                "INSERT INTO incident_timeline (id, incident_id, occurred_at, kind, detail, entity_id)
                 VALUES (?, ?, ?, ?, ?, ?)
                 ON CONFLICT DO NOTHING",
            )
            .bind(timeline_id(incident, event))
            .bind(&id)
            .bind(to_rfc3339(event.at))
            .bind(&event.kind)
            .bind(&event.detail)
            .bind(event.entity.map(|e| e.to_string()))
            .execute(&mut *tx)
            .await?;
        }

        tx.commit().await?;
        Ok(())
    }

    /// Load incidents in an environment, newest first.
    pub async fn load_incidents(&self, environment: &str, limit: u32) -> Result<Vec<Incident>, StoreError> {
        let rows = sqlx::query(
            "SELECT id, fingerprint, status, severity, started_at, ended_at FROM incidents
             WHERE environment = ? ORDER BY started_at DESC LIMIT ?",
        )
        .bind(environment)
        .bind(limit as i64)
        .fetch_all(self.pool())
        .await?;

        let mut incidents = Vec::with_capacity(rows.len());
        for row in rows {
            incidents.push(self.hydrate_incident(&row).await?);
        }
        Ok(incidents)
    }

    /// Load the incidents that still need attention.
    pub async fn load_active_incidents(&self, environment: &str) -> Result<Vec<Incident>, StoreError> {
        Ok(self
            .load_incidents(environment, 1000)
            .await?
            .into_iter()
            .filter(|i| i.status.is_active())
            .collect())
    }

    /// Load one incident by id.
    pub async fn load_incident(&self, id: &str) -> Result<Option<Incident>, StoreError> {
        let row =
            sqlx::query("SELECT id, fingerprint, status, severity, started_at, ended_at FROM incidents WHERE id = ?")
                .bind(id)
                .fetch_optional(self.pool())
                .await?;

        match row {
            Some(row) => Ok(Some(self.hydrate_incident(&row).await?)),
            None => Ok(None),
        }
    }

    async fn hydrate_incident(&self, row: &sqlx::sqlite::SqliteRow) -> Result<Incident, StoreError> {
        let id: String = row.try_get("id")?;
        let status_text: String = row.try_get("status")?;
        let severity_text: String = row.try_get("severity")?;
        let ended_at: Option<String> = row.try_get("ended_at")?;

        let mut incident = Incident {
            id: id.parse().map_err(|e: uuid::Error| StoreError::Decode {
                kind: "incident id",
                detail: e.to_string(),
            })?,
            fingerprint: row.try_get("fingerprint")?,
            status: IncidentStatus::parse(&status_text).unwrap_or(IncidentStatus::Open),
            severity: Severity::parse(&severity_text).unwrap_or(Severity::Warning),
            started_at: decode_time(row.try_get("started_at")?)?,
            ended_at: ended_at.map(decode_time).transpose()?,
            affected_entities: Vec::new(),
            suspected_root_entities: Vec::new(),
            diagnoses: Vec::new(),
            evidence: Vec::new(),
            timeline: Vec::new(),
        };

        for entity_row in sqlx::query("SELECT entity_id, role FROM incident_entities WHERE incident_id = ?")
            .bind(&id)
            .fetch_all(self.pool())
            .await?
        {
            let entity = decode_entity(entity_row.try_get("entity_id")?)?;
            match entity_row.try_get::<String, _>("role")?.as_str() {
                ROLE_ROOT => incident.suspected_root_entities.push(entity),
                _ => incident.affected_entities.push(entity),
            }
        }

        for evidence_row in sqlx::query("SELECT observation_id FROM incident_evidence WHERE incident_id = ?")
            .bind(&id)
            .fetch_all(self.pool())
            .await?
        {
            if let Ok(observation) = evidence_row
                .try_get::<String, _>("observation_id")?
                .parse::<ObservationId>()
            {
                incident.evidence.push(observation);
            }
        }

        for diagnosis_row in sqlx::query(
            "SELECT id, diagnosis_type, rule_id, confidence, summary, recommended_actions, created_at
             FROM diagnoses WHERE incident_id = ? ORDER BY created_at",
        )
        .bind(&id)
        .fetch_all(self.pool())
        .await?
        {
            let diagnosis_id: String = diagnosis_row.try_get("id")?;
            let confidence_text: String = diagnosis_row.try_get("confidence")?;
            let actions_text: String = diagnosis_row.try_get("recommended_actions")?;

            let mut diagnosis = Diagnosis {
                id: diagnosis_id.parse().map_err(|e: uuid::Error| StoreError::Decode {
                    kind: "diagnosis id",
                    detail: e.to_string(),
                })?,
                diagnosis_type: DiagnosisType::new(diagnosis_row.try_get::<String, _>("diagnosis_type")?),
                affected_entities: Vec::new(),
                suspected_root_entities: Vec::new(),
                confidence: Confidence::parse(&confidence_text).unwrap_or(Confidence::Low),
                evidence: Vec::new(),
                rule_id: RuleId::new(diagnosis_row.try_get::<String, _>("rule_id")?),
                summary: diagnosis_row.try_get("summary")?,
                recommended_actions: serde_json::from_str(&actions_text).unwrap_or_default(),
                created_at: decode_time(diagnosis_row.try_get("created_at")?)?,
            };

            for entity_row in sqlx::query("SELECT entity_id, role FROM diagnosis_entities WHERE diagnosis_id = ?")
                .bind(&diagnosis_id)
                .fetch_all(self.pool())
                .await?
            {
                let entity = decode_entity(entity_row.try_get("entity_id")?)?;
                match entity_row.try_get::<String, _>("role")?.as_str() {
                    ROLE_ROOT => diagnosis.suspected_root_entities.push(entity),
                    _ => diagnosis.affected_entities.push(entity),
                }
            }

            for evidence_row in sqlx::query("SELECT observation_id FROM diagnosis_evidence WHERE diagnosis_id = ?")
                .bind(&diagnosis_id)
                .fetch_all(self.pool())
                .await?
            {
                if let Ok(observation) = evidence_row
                    .try_get::<String, _>("observation_id")?
                    .parse::<ObservationId>()
                {
                    diagnosis.evidence.push(observation);
                }
            }

            incident.diagnoses.push(diagnosis);
        }

        for event_row in sqlx::query(
            "SELECT occurred_at, kind, detail, entity_id FROM incident_timeline
             WHERE incident_id = ? ORDER BY occurred_at",
        )
        .bind(&id)
        .fetch_all(self.pool())
        .await?
        {
            let entity: Option<String> = event_row.try_get("entity_id")?;
            incident.timeline.push(TimelineEvent {
                at: decode_time(event_row.try_get("occurred_at")?)?,
                kind: event_row.try_get("kind")?,
                detail: event_row.try_get("detail")?,
                entity: entity.and_then(|e| e.parse().ok()),
            });
        }

        Ok(incident)
    }
}

/// A stable id for a timeline entry, so re-saving does not duplicate it.
fn timeline_id(incident: &Incident, event: &TimelineEvent) -> String {
    let name = format!(
        "{}\u{1f}{}\u{1f}{}\u{1f}{}",
        incident.id,
        to_rfc3339(event.at),
        event.kind,
        event.detail
    );
    uuid::Uuid::new_v5(
        &uuid::Uuid::from_u128(0x9e3a_71bc_44d2_5f80_a1c3_66be_2f04_7d19),
        name.as_bytes(),
    )
    .to_string()
}

fn decode_time(value: String) -> Result<crate::time::Timestamp, StoreError> {
    parse_rfc3339(&value).map_err(|e| StoreError::Decode {
        kind: "timestamp",
        detail: format!("{value}: {e}"),
    })
}

fn decode_entity(value: String) -> Result<EntityId, StoreError> {
    value.parse().map_err(|e: uuid::Error| StoreError::Decode {
        kind: "entity id",
        detail: e.to_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::diagnosis::kind;
    use crate::entity::{EntityKey, EntityType, ManagedEntity};

    fn host(name: &str) -> EntityId {
        EntityKey::new("lab", EntityType::Host, name).entity_id()
    }

    async fn store() -> SqliteStore {
        let store = SqliteStore::open_in_memory().await.expect("open");
        store.ensure_environment("lab").await.expect("environment");
        for name in ["fs1", "c1", "c2"] {
            store
                .save_entity(&ManagedEntity::new("lab", EntityType::Host, name))
                .await
                .expect("entity");
        }
        store
    }

    fn incident() -> Incident {
        let mut incident = Incident::open("cause:fs1", Severity::Critical);
        incident.add_diagnosis(
            Diagnosis::new(kind::SHARED_STORAGE_FAILURE, "storage.shared_failure", Confidence::High)
                .rooted_at([host("fs1")])
                .affecting([host("c1"), host("c2")])
                .with_evidence([ObservationId::new(), ObservationId::new()])
                .with_summary("two clients of storage-a are impaired together")
                .recommending(vec!["sentinel entity show fs1".into()]),
        );
        incident
    }

    #[tokio::test]
    async fn an_incident_round_trips_with_everything_hanging_off_it() {
        let store = store().await;
        let original = incident();
        store.save_incident("lab", &original).await.expect("save");

        let loaded = store.load_incidents("lab", 10).await.expect("load");
        assert_eq!(loaded.len(), 1);

        let loaded = &loaded[0];
        assert_eq!(loaded.id, original.id);
        assert_eq!(loaded.fingerprint, original.fingerprint);
        assert_eq!(loaded.severity, Severity::Critical);
        assert_eq!(loaded.status, IncidentStatus::Open);

        let affected: std::collections::BTreeSet<_> = loaded.affected_entities.iter().collect();
        assert_eq!(affected, [host("c1"), host("c2")].iter().collect());
        assert_eq!(loaded.suspected_root_entities, vec![host("fs1")]);
    }

    #[tokio::test]
    async fn the_evidence_survives_the_round_trip() {
        // SPEC.md §103: this is what makes a post-mortem possible.
        let store = store().await;
        let original = incident();
        let expected: std::collections::BTreeSet<_> = original.evidence.iter().copied().collect();

        store.save_incident("lab", &original).await.expect("save");
        let loaded = &store.load_incidents("lab", 10).await.expect("load")[0];

        let held: std::collections::BTreeSet<_> = loaded.evidence.iter().copied().collect();
        assert_eq!(held, expected);
        assert!(!held.is_empty());
    }

    #[tokio::test]
    async fn the_diagnosis_and_its_reasoning_survive() {
        let store = store().await;
        store.save_incident("lab", &incident()).await.expect("save");

        let loaded = &store.load_incidents("lab", 10).await.expect("load")[0];
        assert_eq!(loaded.diagnoses.len(), 1);

        let diagnosis = &loaded.diagnoses[0];
        assert!(diagnosis.is(kind::SHARED_STORAGE_FAILURE));
        assert_eq!(diagnosis.rule_id.as_str(), "storage.shared_failure");
        assert_eq!(diagnosis.confidence, Confidence::High);
        assert!(diagnosis.summary.contains("impaired together"));
        assert_eq!(diagnosis.recommended_actions.len(), 1);
        assert_eq!(diagnosis.suspected_root_entities, vec![host("fs1")]);
        assert_eq!(diagnosis.evidence.len(), 2);
    }

    #[tokio::test]
    async fn the_timeline_survives_and_is_ordered() {
        let store = store().await;
        let mut original = incident();
        original.escalate(Severity::Critical);
        original.resolve();

        store.save_incident("lab", &original).await.expect("save");
        let loaded = &store.load_incidents("lab", 10).await.expect("load")[0];

        assert!(loaded.timeline.len() >= 2);
        assert!(loaded.timeline.iter().any(|e| e.kind == "resolved"));
        let times: Vec<_> = loaded.timeline.iter().map(|e| e.at).collect();
        let mut sorted = times.clone();
        sorted.sort();
        assert_eq!(times, sorted);
    }

    #[tokio::test]
    async fn re_saving_updates_rather_than_duplicating() {
        let store = store().await;
        let mut original = incident();
        store.save_incident("lab", &original).await.expect("first");

        original.resolve();
        store.save_incident("lab", &original).await.expect("second");

        let loaded = store.load_incidents("lab", 10).await.expect("load");
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].status, IncidentStatus::Resolved);
        assert!(loaded[0].ended_at.is_some());
    }

    #[tokio::test]
    async fn re_saving_does_not_duplicate_timeline_entries() {
        let store = store().await;
        let original = incident();
        store.save_incident("lab", &original).await.expect("first");
        store.save_incident("lab", &original).await.expect("second");

        let loaded = &store.load_incidents("lab", 10).await.expect("load")[0];
        assert_eq!(loaded.timeline.len(), original.timeline.len());
    }

    #[tokio::test]
    async fn only_active_incidents_come_back_from_the_active_query() {
        let store = store().await;
        store.save_incident("lab", &incident()).await.expect("open one");

        let mut resolved = Incident::open("cause:other", Severity::Warning);
        resolved.resolve();
        store.save_incident("lab", &resolved).await.expect("resolved one");

        let active = store.load_active_incidents("lab").await.expect("load");
        assert_eq!(active.len(), 1);
        assert_eq!(active[0].fingerprint, "cause:fs1");
    }

    #[tokio::test]
    async fn one_incident_can_be_loaded_by_id() {
        let store = store().await;
        let original = incident();
        store.save_incident("lab", &original).await.expect("save");

        let loaded = store.load_incident(&original.id.to_string()).await.expect("load");
        assert_eq!(loaded.expect("found").id, original.id);
        assert!(store
            .load_incident(&uuid::Uuid::new_v4().to_string())
            .await
            .expect("load")
            .is_none());
    }

    #[tokio::test]
    async fn incidents_come_back_newest_first() {
        let store = store().await;
        let older = Incident::open("cause:older", Severity::Warning);
        let mut newer = Incident::open("cause:newer", Severity::Warning);
        newer.started_at = older.started_at + chrono::Duration::minutes(5);

        store.save_incident("lab", &older).await.expect("older");
        store.save_incident("lab", &newer).await.expect("newer");

        let loaded = store.load_incidents("lab", 10).await.expect("load");
        assert_eq!(loaded[0].fingerprint, "cause:newer");
    }
}
