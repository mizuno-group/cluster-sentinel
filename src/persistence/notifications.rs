//! Remembering which notifications have actually been delivered.
//!
//! Without this the record lived only in the notifying process, which made
//! "already told the operator" indistinguishable from "never managed to tell
//! the operator" the moment that process restarted. Both look like silence,
//! and only one of them is correct.
//!
//! The rows are keyed by `(provider, deduplication_key)` and cascade with
//! their incident, so retention takes them away at the same time as the thing
//! they describe.

use sqlx::Row;

use crate::notification::NotificationRecord;
use crate::time::{parse_rfc3339, to_rfc3339};

use super::{SqliteStore, StoreError};

/// Delivered, as opposed to attempted and failed.
const STATUS_SENT: &str = "sent";

impl SqliteStore {
    /// Record that a notification was delivered.
    pub async fn save_notification(&self, record: &NotificationRecord) -> Result<(), StoreError> {
        sqlx::query(
            "INSERT INTO notifications (id, incident_id, provider, deduplication_key, status, sent_at, created_at)
             VALUES (?, ?, ?, ?, ?, ?, ?)
             ON CONFLICT(provider, deduplication_key) DO UPDATE SET
                 incident_id = excluded.incident_id,
                 status = excluded.status,
                 sent_at = excluded.sent_at",
        )
        .bind(uuid::Uuid::new_v4().to_string())
        .bind(&record.incident_id)
        .bind(&record.provider)
        .bind(&record.deduplication_key)
        .bind(STATUS_SENT)
        .bind(to_rfc3339(record.sent_at))
        .bind(to_rfc3339(record.sent_at))
        .execute(self.pool())
        .await?;

        Ok(())
    }

    /// Every delivery recorded for one environment, to resume after a restart.
    pub async fn load_notifications(&self, environment: &str) -> Result<Vec<NotificationRecord>, StoreError> {
        let rows = sqlx::query(
            "SELECT n.incident_id, n.provider, n.deduplication_key, n.sent_at
             FROM notifications n
             JOIN incidents i ON i.id = n.incident_id
             WHERE i.environment = ? AND n.status = ? AND n.sent_at IS NOT NULL",
        )
        .bind(environment)
        .bind(STATUS_SENT)
        .fetch_all(self.pool())
        .await?;

        let mut records = Vec::with_capacity(rows.len());
        for row in rows {
            let sent_at: String = row.try_get("sent_at")?;
            records.push(NotificationRecord {
                incident_id: row.try_get("incident_id")?,
                provider: row.try_get("provider")?,
                deduplication_key: row.try_get("deduplication_key")?,
                sent_at: parse_rfc3339(&sent_at).map_err(|e| StoreError::Decode {
                    kind: "timestamp",
                    detail: format!("{sent_at}: {e}"),
                })?,
            });
        }

        Ok(records)
    }

    /// Drop the delivery records for a deduplication key.
    ///
    /// Used when an incident resolves: the same fault returning is new news,
    /// and the record of the old announcement must not silence it.
    pub async fn forget_notifications(&self, deduplication_key: &str) -> Result<u64, StoreError> {
        let result = sqlx::query("DELETE FROM notifications WHERE deduplication_key = ?")
            .bind(deduplication_key)
            .execute(self.pool())
            .await?;

        Ok(result.rows_affected())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::incident::{Incident, Severity};
    use crate::time::now;

    async fn store_with_incident() -> (SqliteStore, Incident) {
        let store = SqliteStore::open_in_memory().await.expect("store");
        store.ensure_environment("lab").await.expect("environment");
        let incident = Incident::open("cause:fs1", Severity::Critical);
        store.save_incident("lab", &incident).await.expect("save incident");
        (store, incident)
    }

    fn record(incident: &Incident, key: &str) -> NotificationRecord {
        NotificationRecord {
            incident_id: incident.id.to_string(),
            deduplication_key: key.to_string(),
            provider: "ops".to_string(),
            sent_at: now(),
        }
    }

    #[tokio::test]
    async fn a_delivery_survives_a_restart() {
        // The whole reason this table is written to: a controller that
        // restarts must not re-announce what it already announced, and must
        // still announce what it never managed to.
        let (store, incident) = store_with_incident().await;
        store
            .save_notification(&record(&incident, "cause:fs1:opened"))
            .await
            .expect("save");

        let loaded = store.load_notifications("lab").await.expect("load");
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].deduplication_key, "cause:fs1:opened");
        assert_eq!(loaded[0].provider, "ops");
    }

    #[tokio::test]
    async fn recording_the_same_delivery_twice_does_not_duplicate_it() {
        let (store, incident) = store_with_incident().await;
        for _ in 0..3 {
            store
                .save_notification(&record(&incident, "cause:fs1:opened"))
                .await
                .expect("save");
        }
        assert_eq!(store.load_notifications("lab").await.expect("load").len(), 1);
    }

    #[tokio::test]
    async fn forgetting_a_key_makes_the_same_news_sendable_again() {
        let (store, incident) = store_with_incident().await;
        store
            .save_notification(&record(&incident, "cause:fs1:opened"))
            .await
            .expect("save");

        assert_eq!(store.forget_notifications("cause:fs1:opened").await.expect("forget"), 1);
        assert!(store.load_notifications("lab").await.expect("load").is_empty());
    }

    #[tokio::test]
    async fn another_environments_deliveries_are_not_returned() {
        let (store, incident) = store_with_incident().await;
        store
            .save_notification(&record(&incident, "cause:fs1:opened"))
            .await
            .expect("save");

        store.ensure_environment("other").await.expect("environment");
        assert!(store.load_notifications("other").await.expect("load").is_empty());
    }

    #[tokio::test]
    async fn deleting_an_incident_takes_its_delivery_records_with_it() {
        // Retention must not leave rows pointing at incidents that are gone.
        let (store, incident) = store_with_incident().await;
        store
            .save_notification(&record(&incident, "cause:fs1:opened"))
            .await
            .expect("save");

        sqlx::query("DELETE FROM incidents WHERE id = ?")
            .bind(incident.id.to_string())
            .execute(store.pool())
            .await
            .expect("delete");

        assert!(store.load_notifications("lab").await.expect("load").is_empty());
    }
}
