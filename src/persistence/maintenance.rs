//! Storing maintenance windows.
//!
//! The table has been in the schema since the first migration and nothing ever
//! wrote to it. The notification path has always consulted a
//! `MaintenanceWindows`, and in production that value was constructed empty
//! every time -- so the feature existed in the type system, in the schema and
//! in its own tests, and an operator had no way to reach it. Swapping a
//! fileserver paged whoever was on the webhook, because saying "this is
//! planned" was not expressible.

use sqlx::Row;

use crate::entity::EntityId;
use crate::notification::MaintenanceWindow;
use crate::time::{parse_rfc3339, to_rfc3339, Timestamp};

use super::{SqliteStore, StoreError};

fn decode_time(value: &str) -> Result<Timestamp, StoreError> {
    parse_rfc3339(value).map_err(|e| StoreError::Decode {
        kind: "timestamp",
        detail: format!("{value}: {e}"),
    })
}

impl SqliteStore {
    /// Record a maintenance window.
    pub async fn save_maintenance_window(
        &self,
        environment: &str,
        window: &MaintenanceWindow,
    ) -> Result<(), StoreError> {
        sqlx::query(
            "INSERT INTO maintenance_windows
                 (id, environment, entity_id, reason, starts_at, ends_at, created_by, created_at)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?)
             ON CONFLICT(id) DO UPDATE SET
                 reason = excluded.reason,
                 starts_at = excluded.starts_at,
                 ends_at = excluded.ends_at",
        )
        .bind(window.id.to_string())
        .bind(environment)
        .bind(window.entity.map(|id| id.to_string()))
        .bind(&window.reason)
        .bind(to_rfc3339(window.starts_at))
        .bind(window.ends_at.map(to_rfc3339))
        .bind(window.created_by.clone())
        .bind(to_rfc3339(crate::time::now()))
        .execute(self.pool())
        .await?;

        Ok(())
    }

    /// Every window recorded for an environment, newest first.
    pub async fn load_maintenance_windows(&self, environment: &str) -> Result<Vec<MaintenanceWindow>, StoreError> {
        let rows = sqlx::query(
            "SELECT id, entity_id, reason, starts_at, ends_at, created_by
             FROM maintenance_windows
             WHERE environment = ?
             ORDER BY starts_at DESC",
        )
        .bind(environment)
        .fetch_all(self.pool())
        .await?;

        let mut windows = Vec::with_capacity(rows.len());
        for row in rows {
            let id: String = row.try_get("id")?;
            let entity: Option<String> = row.try_get("entity_id")?;
            let starts_at: String = row.try_get("starts_at")?;
            let ends_at: Option<String> = row.try_get("ends_at")?;

            windows.push(MaintenanceWindow {
                id: id.parse().map_err(|e| StoreError::Decode {
                    kind: "uuid",
                    detail: format!("{id}: {e}"),
                })?,
                entity: entity.and_then(|id| id.parse::<EntityId>().ok()),
                reason: row.try_get("reason")?,
                starts_at: decode_time(&starts_at)?,
                ends_at: ends_at.as_deref().map(decode_time).transpose()?,
                created_by: row.try_get("created_by")?,
            });
        }

        Ok(windows)
    }

    /// End a window now, so notifications resume.
    ///
    /// The row stays: what was suppressed and when is part of the record, and
    /// deleting it would leave a gap in the notification history that nobody
    /// could explain afterwards.
    pub async fn end_maintenance_window(&self, id: uuid::Uuid, at: Timestamp) -> Result<bool, StoreError> {
        let result = sqlx::query("UPDATE maintenance_windows SET ends_at = ? WHERE id = ?")
            .bind(to_rfc3339(at))
            .bind(id.to_string())
            .execute(self.pool())
            .await?;

        Ok(result.rows_affected() > 0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::entity::{EntityKey, EntityType, ManagedEntity};

    async fn store() -> SqliteStore {
        let store = SqliteStore::open_in_memory().await.expect("store");
        store.ensure_environment("lab").await.expect("environment");
        store
            .save_entity(&ManagedEntity::new("lab", EntityType::Host, "fs1"))
            .await
            .expect("entity");
        store
    }

    fn fs1() -> EntityId {
        EntityKey::new("lab", EntityType::Host, "fs1").entity_id()
    }

    #[tokio::test]
    async fn a_window_round_trips() {
        let store = store().await;
        let window = MaintenanceWindow::for_entity(fs1(), "disk swap").by("an operator");
        store.save_maintenance_window("lab", &window).await.expect("save");

        let loaded = store.load_maintenance_windows("lab").await.expect("load");
        assert_eq!(loaded, vec![window]);
    }

    #[tokio::test]
    async fn an_environment_wide_window_has_no_entity() {
        let store = store().await;
        let window = MaintenanceWindow::for_environment("whole-cluster power work");
        store.save_maintenance_window("lab", &window).await.expect("save");

        let loaded = store.load_maintenance_windows("lab").await.expect("load");
        assert_eq!(loaded[0].entity, None);
        assert!(loaded[0].covers(fs1()), "it covers everything");
    }

    #[tokio::test]
    async fn ending_a_window_keeps_the_row() {
        // What was suppressed, and when, is part of the record. Deleting it
        // would leave a gap in the notification history nobody could explain.
        let store = store().await;
        let window = MaintenanceWindow::for_entity(fs1(), "disk swap");
        store.save_maintenance_window("lab", &window).await.expect("save");

        let at = crate::time::now();
        assert!(store.end_maintenance_window(window.id, at).await.expect("end"));

        let loaded = store.load_maintenance_windows("lab").await.expect("load");
        assert_eq!(loaded.len(), 1, "the record survives");
        assert!(loaded[0].ends_at.is_some());
        assert!(!loaded[0].is_active_at(at + chrono::Duration::seconds(1)));
    }

    #[tokio::test]
    async fn ending_a_window_that_does_not_exist_says_so() {
        let store = store().await;
        assert!(!store
            .end_maintenance_window(uuid::Uuid::new_v4(), crate::time::now())
            .await
            .expect("end"));
    }

    #[tokio::test]
    async fn another_environments_windows_are_not_returned() {
        let store = store().await;
        store
            .save_maintenance_window("lab", &MaintenanceWindow::for_environment("work"))
            .await
            .expect("save");
        store.ensure_environment("other").await.expect("environment");

        assert!(store.load_maintenance_windows("other").await.expect("load").is_empty());
    }
}
