//! SQLite-backed store.

use std::path::Path;
use std::str::FromStr;

use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};
use sqlx::{Row, SqlitePool};

use super::{sqlite_url, StoreError};
use crate::time::{now, to_rfc3339};

/// Migrations are compiled into the binary so that a single file can bring up
/// its own database (SPEC.md §40, §127).
static MIGRATOR: sqlx::migrate::Migrator = sqlx::migrate!("./migrations");

/// A SQLite connection pool with the schema applied.
#[derive(Debug, Clone)]
pub struct SqliteStore {
    pool: SqlitePool,
}

impl SqliteStore {
    /// Open (creating if needed) and migrate a database at `path`.
    pub async fn open(path: &Path) -> Result<Self, StoreError> {
        if path != Path::new(":memory:") {
            if let Some(parent) = path.parent() {
                if !parent.as_os_str().is_empty() {
                    std::fs::create_dir_all(parent).map_err(|source| StoreError::Directory {
                        path: parent.to_path_buf(),
                        source,
                    })?;
                }
            }
        }

        let options = SqliteConnectOptions::from_str(&sqlite_url(path))?
            .create_if_missing(true)
            // WAL keeps readers (the CLI) from blocking the controller's
            // writers (SPEC.md §124).
            .journal_mode(sqlx::sqlite::SqliteJournalMode::Wal)
            .synchronous(sqlx::sqlite::SqliteSynchronous::Normal)
            .foreign_keys(true)
            .busy_timeout(std::time::Duration::from_secs(10));

        let pool = SqlitePoolOptions::new()
            .max_connections(8)
            .connect_with(options)
            .await?;
        let store = Self { pool };
        store.migrate().await?;
        Ok(store)
    }

    /// Open an in-memory database, for tests.
    pub async fn open_in_memory() -> Result<Self, StoreError> {
        // A single connection: each new connection to `sqlite::memory:` would
        // otherwise get its own empty database.
        let options = SqliteConnectOptions::from_str("sqlite::memory:")?.foreign_keys(true);
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect_with(options)
            .await?;
        let store = Self { pool };
        store.migrate().await?;
        Ok(store)
    }

    /// Apply any outstanding migrations.
    pub async fn migrate(&self) -> Result<(), StoreError> {
        MIGRATOR.run(&self.pool).await?;
        Ok(())
    }

    /// The connection pool, for repositories in other modules.
    pub fn pool(&self) -> &SqlitePool {
        &self.pool
    }

    /// The highest applied migration version.
    pub async fn schema_version(&self) -> Result<i64, StoreError> {
        let row = sqlx::query("SELECT MAX(version) AS version FROM _sqlx_migrations")
            .fetch_one(&self.pool)
            .await?;
        Ok(row.try_get::<Option<i64>, _>("version")?.unwrap_or(0))
    }

    /// Register an environment if it is not already present.
    pub async fn ensure_environment(&self, name: &str) -> Result<(), StoreError> {
        sqlx::query(
            "INSERT INTO environments (name, display_name, created_at) VALUES (?1, ?1, ?2)
             ON CONFLICT(name) DO NOTHING",
        )
        .bind(name)
        .bind(to_rfc3339(now()))
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Names of the known environments.
    pub async fn environments(&self) -> Result<Vec<String>, StoreError> {
        let rows = sqlx::query("SELECT name FROM environments ORDER BY name")
            .fetch_all(&self.pool)
            .await?;
        rows.iter()
            .map(|r| r.try_get::<String, _>("name").map_err(StoreError::from))
            .collect()
    }

    /// Close the pool, flushing anything outstanding.
    pub async fn close(&self) {
        self.pool.close().await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn migrations_apply_to_a_fresh_database() {
        let store = SqliteStore::open_in_memory().await.expect("open");
        assert!(store.schema_version().await.expect("version") > 0);
    }

    #[tokio::test]
    async fn migrations_are_idempotent() {
        let store = SqliteStore::open_in_memory().await.expect("open");
        let before = store.schema_version().await.expect("version");
        store.migrate().await.expect("re-run migrations");
        assert_eq!(store.schema_version().await.expect("version"), before);
    }

    #[tokio::test]
    async fn every_core_table_from_the_specification_exists() {
        let store = SqliteStore::open_in_memory().await.expect("open");
        let rows = sqlx::query("SELECT name FROM sqlite_master WHERE type = 'table'")
            .fetch_all(store.pool())
            .await
            .expect("query");
        let tables: Vec<String> = rows.iter().map(|r| r.get::<String, _>("name")).collect();

        for expected in [
            "environments",
            "clusters",
            "entities",
            "entity_labels",
            "entity_capabilities",
            "entity_addresses",
            "dependencies",
            "agent_instances",
            "agent_sessions",
            "probes",
            "observations",
            "entity_states",
            "state_transitions",
            "diagnoses",
            "incidents",
            "incident_entities",
            "incident_evidence",
            "notifications",
            "acknowledgements",
            "maintenance_windows",
        ] {
            assert!(
                tables.iter().any(|t| t == expected),
                "missing table {expected}; have {tables:?}"
            );
        }
    }

    #[tokio::test]
    async fn the_schema_permits_more_than_one_controller_per_environment() {
        // SPEC.md §43: v1 need not implement HA, but the schema must not
        // foreclose it.
        let store = SqliteStore::open_in_memory().await.expect("open");
        store.ensure_environment("lab").await.expect("environment");
        for (id, endpoint) in [("c1", "192.0.2.1:7443"), ("c2", "192.0.2.2:7443")] {
            sqlx::query("INSERT INTO controllers (id, environment, endpoint, created_at) VALUES (?, 'lab', ?, ?)")
                .bind(id)
                .bind(endpoint)
                .bind(to_rfc3339(now()))
                .execute(store.pool())
                .await
                .expect("insert controller");
        }
        let row = sqlx::query("SELECT COUNT(*) AS n FROM controllers WHERE environment = 'lab'")
            .fetch_one(store.pool())
            .await
            .expect("count");
        assert_eq!(row.get::<i64, _>("n"), 2);
    }

    #[tokio::test]
    async fn the_dependency_table_accepts_a_cycle() {
        // SPEC.md §28: operational dependencies really do contain cycles.
        let store = SqliteStore::open_in_memory().await.expect("open");
        store.ensure_environment("lab").await.expect("environment");
        let ts = to_rfc3339(now());
        for name in ["a", "b"] {
            sqlx::query(
                "INSERT INTO entities (id, environment, entity_type, canonical_name, display_name, created_at, updated_at)
                 VALUES (?, 'lab', 'host', ?, ?, ?, ?)",
            )
            .bind(name)
            .bind(name)
            .bind(name)
            .bind(&ts)
            .bind(&ts)
            .execute(store.pool())
            .await
            .expect("insert entity");
        }
        for (id, from, to) in [("e1", "a", "b"), ("e2", "b", "a")] {
            sqlx::query(
                "INSERT INTO dependencies (id, source_entity_id, target_entity_id, dependency_type,
                 discovery_source, first_seen_at, last_seen_at) VALUES (?, ?, ?, 'depends_on', 'manual', ?, ?)",
            )
            .bind(id)
            .bind(from)
            .bind(to)
            .bind(&ts)
            .bind(&ts)
            .execute(store.pool())
            .await
            .expect("insert dependency");
        }
        let row = sqlx::query("SELECT COUNT(*) AS n FROM dependencies")
            .fetch_one(store.pool())
            .await
            .expect("count");
        assert_eq!(row.get::<i64, _>("n"), 2);
    }

    #[tokio::test]
    async fn ensure_environment_is_idempotent() {
        let store = SqliteStore::open_in_memory().await.expect("open");
        store.ensure_environment("lab").await.expect("first");
        store.ensure_environment("lab").await.expect("second");
        assert_eq!(store.environments().await.expect("list"), vec!["lab".to_string()]);
    }

    #[tokio::test]
    async fn a_database_file_and_its_parent_directory_are_created() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("nested/state/sentinel.db");
        let store = SqliteStore::open(&path).await.expect("open");
        store.ensure_environment("lab").await.expect("environment");
        store.close().await;
        assert!(path.exists());
    }
}
