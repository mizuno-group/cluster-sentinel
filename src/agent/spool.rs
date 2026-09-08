//! The agent's local spool (IMPLEMENTATION.md §54, §55).
//!
//! When the controller is unreachable, observations go here rather than being
//! dropped. That is the whole point: a controller outage is *exactly* when the
//! evidence is most valuable, and losing it would leave an operator with a gap
//! where the incident was (SPEC.md §105, §176).
//!
//! Requirements this implementation meets, each with a test:
//!
//! * WAL, so a crash mid-write does not corrupt the file;
//! * bounded by age, row count and byte size, so it cannot fill the disk;
//! * ordered replay, oldest first;
//! * idempotent resend, because ids come from the agent;
//! * important rows evicted last.

use std::path::Path;
use std::str::FromStr;
use std::time::Duration;

use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};
use sqlx::{Row, SqlitePool};

use crate::observation::{Observation, ObservationId, ProbeStatus};
use crate::persistence::StoreError;
use crate::probes::ProbeId;
use crate::time::{now, parse_rfc3339, to_rfc3339, Timestamp};

/// How much the spool may hold before it starts evicting.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SpoolLimits {
    /// Oldest row to keep.
    pub max_age: Duration,
    /// Most rows to keep.
    pub max_rows: u64,
    /// Most bytes of payload to keep.
    pub max_bytes: u64,
}

impl Default for SpoolLimits {
    fn default() -> Self {
        Self {
            // A day is long enough to survive a weekend controller outage
            // being noticed on Monday, without unbounded growth.
            max_age: Duration::from_secs(24 * 60 * 60),
            max_rows: 100_000,
            max_bytes: 64 * 1024 * 1024,
        }
    }
}

/// What an eviction pass removed.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct EvictionOutcome {
    /// Rows removed for being too old.
    pub aged_out: u64,
    /// Rows removed to satisfy the row limit.
    pub over_row_limit: u64,
    /// Rows removed to satisfy the byte limit.
    pub over_byte_limit: u64,
}

impl EvictionOutcome {
    /// Total rows removed.
    pub fn total(&self) -> u64 {
        self.aged_out + self.over_row_limit + self.over_byte_limit
    }
}

/// An agent's on-disk observation queue.
#[derive(Debug, Clone)]
pub struct Spool {
    pool: SqlitePool,
    limits: SpoolLimits,
}

impl Spool {
    /// Open (creating if needed) a spool.
    pub async fn open(path: &Path, limits: SpoolLimits) -> Result<Self, StoreError> {
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent).map_err(|source| StoreError::Directory {
                    path: parent.to_path_buf(),
                    source,
                })?;
            }
        }

        let options = SqliteConnectOptions::from_str(&format!("sqlite://{}?mode=rwc", path.display()))?
            .create_if_missing(true)
            .journal_mode(sqlx::sqlite::SqliteJournalMode::Wal)
            .synchronous(sqlx::sqlite::SqliteSynchronous::Normal)
            .busy_timeout(Duration::from_secs(5));

        Self::from_pool(
            SqlitePoolOptions::new()
                .max_connections(2)
                .connect_with(options)
                .await?,
            limits,
        )
        .await
    }

    /// Open an in-memory spool, for tests.
    pub async fn open_in_memory(limits: SpoolLimits) -> Result<Self, StoreError> {
        let options = SqliteConnectOptions::from_str("sqlite::memory:")?;
        Self::from_pool(
            SqlitePoolOptions::new()
                .max_connections(1)
                .connect_with(options)
                .await?,
            limits,
        )
        .await
    }

    async fn from_pool(pool: SqlitePool, limits: SpoolLimits) -> Result<Self, StoreError> {
        // The spool schema is separate from the controller's: it is a queue,
        // not a copy of the database, and it must be creatable by an agent that
        // has never spoken to a controller.
        sqlx::query(
            "CREATE TABLE IF NOT EXISTS spooled_observations (
                 id          TEXT PRIMARY KEY,
                 recorded_at TEXT NOT NULL,
                 priority    INTEGER NOT NULL DEFAULT 0,
                 size_bytes  INTEGER NOT NULL,
                 payload     TEXT NOT NULL
             ) STRICT",
        )
        .execute(&pool)
        .await?;
        sqlx::query(
            "CREATE INDEX IF NOT EXISTS idx_spool_order
             ON spooled_observations(priority ASC, recorded_at ASC)",
        )
        .execute(&pool)
        .await?;

        Ok(Self { pool, limits })
    }

    /// The limits in force.
    pub fn limits(&self) -> SpoolLimits {
        self.limits
    }

    /// Add observations to the spool, evicting if it is over its limits.
    pub async fn push(&self, observations: &[Observation]) -> Result<EvictionOutcome, StoreError> {
        let mut tx = self.pool.begin().await?;
        for observation in observations {
            let payload = serde_json::to_string(observation).map_err(|e| StoreError::Decode {
                kind: "observation",
                detail: e.to_string(),
            })?;
            sqlx::query(
                "INSERT INTO spooled_observations (id, recorded_at, priority, size_bytes, payload)
                 VALUES (?, ?, ?, ?, ?)
                 ON CONFLICT(id) DO NOTHING",
            )
            .bind(observation.id.to_string())
            .bind(to_rfc3339(observation.finished_at))
            .bind(priority_of(observation))
            .bind(payload.len() as i64)
            .bind(payload)
            .execute(&mut *tx)
            .await?;
        }
        tx.commit().await?;

        self.evict().await
    }

    /// Read the oldest `limit` observations without removing them.
    ///
    /// Peek-then-acknowledge, not pop: an observation must not be lost because
    /// the network failed after it left the queue but before it arrived.
    pub async fn peek(&self, limit: u32) -> Result<Vec<Observation>, StoreError> {
        let rows = sqlx::query(
            "SELECT payload FROM spooled_observations
             ORDER BY priority DESC, recorded_at ASC, id ASC LIMIT ?",
        )
        .bind(limit as i64)
        .fetch_all(&self.pool)
        .await?;

        rows.iter()
            .map(|row| {
                let payload: String = row.try_get("payload")?;
                serde_json::from_str(&payload).map_err(|e| StoreError::Decode {
                    kind: "spooled observation",
                    detail: e.to_string(),
                })
            })
            .collect()
    }

    /// Remove observations the controller has confirmed.
    pub async fn acknowledge(&self, ids: &[ObservationId]) -> Result<u64, StoreError> {
        let mut removed = 0;
        let mut tx = self.pool.begin().await?;
        for id in ids {
            removed += sqlx::query("DELETE FROM spooled_observations WHERE id = ?")
                .bind(id.to_string())
                .execute(&mut *tx)
                .await?
                .rows_affected();
        }
        tx.commit().await?;
        Ok(removed)
    }

    /// How many observations are waiting.
    pub async fn len(&self) -> Result<u64, StoreError> {
        let row = sqlx::query("SELECT COUNT(*) AS n FROM spooled_observations")
            .fetch_one(&self.pool)
            .await?;
        Ok(row.try_get::<i64, _>("n")? as u64)
    }

    /// Whether the spool is empty.
    pub async fn is_empty(&self) -> Result<bool, StoreError> {
        Ok(self.len().await? == 0)
    }

    /// Total payload bytes held.
    pub async fn size_bytes(&self) -> Result<u64, StoreError> {
        let row = sqlx::query("SELECT COALESCE(SUM(size_bytes), 0) AS n FROM spooled_observations")
            .fetch_one(&self.pool)
            .await?;
        Ok(row.try_get::<i64, _>("n")? as u64)
    }

    /// Enforce the limits, removing the least valuable rows first.
    pub async fn evict(&self) -> Result<EvictionOutcome, StoreError> {
        let mut outcome = EvictionOutcome::default();

        let cutoff = now() - chrono::Duration::from_std(self.limits.max_age).unwrap_or(chrono::Duration::MAX);
        outcome.aged_out = sqlx::query("DELETE FROM spooled_observations WHERE recorded_at < ?")
            .bind(to_rfc3339(cutoff))
            .execute(&self.pool)
            .await?
            .rows_affected();

        // Eviction order is the reverse of replay order: lowest priority and
        // oldest first, so a state transition outlives a routine metric.
        outcome.over_row_limit = sqlx::query(
            "DELETE FROM spooled_observations WHERE id IN (
                 SELECT id FROM spooled_observations
                 ORDER BY priority ASC, recorded_at ASC, id ASC
                 LIMIT MAX(0, (SELECT COUNT(*) FROM spooled_observations) - ?)
             )",
        )
        .bind(self.limits.max_rows as i64)
        .execute(&self.pool)
        .await?
        .rows_affected();

        while self.size_bytes().await? > self.limits.max_bytes {
            let removed = sqlx::query(
                "DELETE FROM spooled_observations WHERE id IN (
                     SELECT id FROM spooled_observations
                     ORDER BY priority ASC, recorded_at ASC, id ASC LIMIT 64
                 )",
            )
            .execute(&self.pool)
            .await?
            .rows_affected();
            if removed == 0 {
                break;
            }
            outcome.over_byte_limit += removed;
        }

        Ok(outcome)
    }

    /// The timestamp of the oldest waiting observation.
    pub async fn oldest(&self) -> Result<Option<Timestamp>, StoreError> {
        let row = sqlx::query("SELECT MIN(recorded_at) AS oldest FROM spooled_observations")
            .fetch_one(&self.pool)
            .await?;
        let value: Option<String> = row.try_get("oldest")?;
        value
            .map(|v| {
                parse_rfc3339(&v).map_err(|e| StoreError::Decode {
                    kind: "spool timestamp",
                    detail: e.to_string(),
                })
            })
            .transpose()
    }

    /// Close the spool.
    pub async fn close(&self) {
        self.pool.close().await;
    }
}

/// How important an observation is to keep when space runs short.
///
/// Anything abnormal outranks routine health: a spool that evicts the record of
/// a failure to make room for a hundred "everything is fine" rows has thrown
/// away the only part that mattered (IMPLEMENTATION.md §55).
fn priority_of(observation: &Observation) -> i64 {
    match observation.status {
        ProbeStatus::Failed | ProbeStatus::Timeout | ProbeStatus::Stuck => 2,
        ProbeStatus::Degraded => 1,
        _ if observation.probe_id == ProbeId::new(crate::controller::registration::PROBE_BOOT) => 2,
        _ => 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::entity::{EntityId, EntityKey, EntityType};

    fn entity() -> EntityId {
        EntityKey::new("lab", EntityType::Host, "node-a").entity_id()
    }

    fn observation(status: ProbeStatus) -> Observation {
        Observation::new(ProbeId::new("test.probe"), entity(), status)
    }

    fn observation_at(status: ProbeStatus, at: Timestamp) -> Observation {
        observation(status).with_times(at, at)
    }

    async fn spool() -> Spool {
        Spool::open_in_memory(SpoolLimits::default()).await.expect("open")
    }

    #[tokio::test]
    async fn a_new_spool_is_empty() {
        let spool = spool().await;
        assert!(spool.is_empty().await.expect("len"));
        assert_eq!(spool.oldest().await.expect("oldest"), None);
    }

    #[tokio::test]
    async fn observations_survive_a_push_and_peek_unchanged() {
        let spool = spool().await;
        let original = observation(ProbeStatus::Failed)
            .with_payload(serde_json::json!({"port": 22}))
            .with_error("refused", "connection refused");

        spool.push(std::slice::from_ref(&original)).await.expect("push");
        let peeked = spool.peek(10).await.expect("peek");

        assert_eq!(peeked.len(), 1);
        assert_eq!(peeked[0], original);
    }

    #[tokio::test]
    async fn peek_does_not_remove_so_a_failed_send_loses_nothing() {
        // The controller may vanish between the read and the acknowledgement.
        let spool = spool().await;
        spool.push(&[observation(ProbeStatus::Ok)]).await.expect("push");

        assert_eq!(spool.peek(10).await.expect("first peek").len(), 1);
        assert_eq!(spool.peek(10).await.expect("second peek").len(), 1);
        assert_eq!(spool.len().await.expect("len"), 1);
    }

    #[tokio::test]
    async fn acknowledging_removes_exactly_what_was_confirmed() {
        let spool = spool().await;
        let batch = vec![observation(ProbeStatus::Ok), observation(ProbeStatus::Ok)];
        spool.push(&batch).await.expect("push");

        assert_eq!(spool.acknowledge(&[batch[0].id]).await.expect("ack"), 1);
        let remaining = spool.peek(10).await.expect("peek");
        assert_eq!(remaining.len(), 1);
        assert_eq!(remaining[0].id, batch[1].id);
    }

    #[tokio::test]
    async fn acknowledging_something_already_gone_is_harmless() {
        let spool = spool().await;
        assert_eq!(spool.acknowledge(&[ObservationId::new()]).await.expect("ack"), 0);
    }

    #[tokio::test]
    async fn pushing_the_same_observation_twice_stores_it_once() {
        let spool = spool().await;
        let batch = vec![observation(ProbeStatus::Ok)];
        spool.push(&batch).await.expect("first");
        spool.push(&batch).await.expect("second");
        assert_eq!(spool.len().await.expect("len"), 1);
    }

    #[tokio::test]
    async fn replay_is_ordered_oldest_first_within_a_priority() {
        let spool = spool().await;
        let base = now() - chrono::Duration::hours(1);
        let batch: Vec<_> = (0..5)
            .map(|i| observation_at(ProbeStatus::Ok, base + chrono::Duration::minutes(i)))
            .collect();
        // Insert out of order to prove the ordering comes from the timestamps.
        spool
            .push(&[batch[3].clone(), batch[0].clone(), batch[4].clone()])
            .await
            .expect("push");
        spool.push(&[batch[1].clone(), batch[2].clone()]).await.expect("push");

        let peeked = spool.peek(10).await.expect("peek");
        let times: Vec<_> = peeked.iter().map(|o| o.finished_at).collect();
        let mut sorted = times.clone();
        sorted.sort();
        assert_eq!(times, sorted);
    }

    #[tokio::test]
    async fn failures_are_replayed_before_routine_successes() {
        // On reconnection the controller should learn about the outage first.
        let spool = spool().await;
        let at = now();
        spool
            .push(&[
                observation_at(ProbeStatus::Ok, at),
                observation_at(ProbeStatus::Failed, at + chrono::Duration::seconds(1)),
                observation_at(ProbeStatus::Degraded, at + chrono::Duration::seconds(2)),
            ])
            .await
            .expect("push");

        let statuses: Vec<_> = spool.peek(10).await.expect("peek").iter().map(|o| o.status).collect();
        assert_eq!(statuses, [ProbeStatus::Failed, ProbeStatus::Degraded, ProbeStatus::Ok]);
    }

    #[tokio::test]
    async fn observations_older_than_the_age_limit_are_dropped() {
        let spool = Spool::open_in_memory(SpoolLimits {
            max_age: Duration::from_secs(60),
            ..Default::default()
        })
        .await
        .expect("open");

        let outcome = spool
            .push(&[
                observation_at(ProbeStatus::Ok, now() - chrono::Duration::hours(2)),
                observation_at(ProbeStatus::Ok, now()),
            ])
            .await
            .expect("push");

        assert_eq!(outcome.aged_out, 1);
        assert_eq!(spool.len().await.expect("len"), 1);
    }

    #[tokio::test]
    async fn the_row_limit_is_enforced_and_evicts_the_oldest() {
        let spool = Spool::open_in_memory(SpoolLimits {
            max_rows: 3,
            ..Default::default()
        })
        .await
        .expect("open");
        let base = now();
        let batch: Vec<_> = (0..6)
            .map(|i| observation_at(ProbeStatus::Ok, base + chrono::Duration::seconds(i)))
            .collect();

        spool.push(&batch).await.expect("push");
        assert_eq!(spool.len().await.expect("len"), 3);

        let remaining: Vec<_> = spool.peek(10).await.expect("peek").iter().map(|o| o.id).collect();
        assert!(remaining.contains(&batch[5].id), "the newest is kept");
        assert!(!remaining.contains(&batch[0].id), "the oldest is evicted");
    }

    #[tokio::test]
    async fn an_important_observation_outlives_routine_ones_under_pressure() {
        // IMPLEMENTATION.md §55: the failure is the part worth keeping.
        let spool = Spool::open_in_memory(SpoolLimits {
            max_rows: 2,
            ..Default::default()
        })
        .await
        .expect("open");
        let base = now();
        let failure = observation_at(ProbeStatus::Failed, base);
        let mut batch = vec![failure.clone()];
        for i in 1..6 {
            batch.push(observation_at(ProbeStatus::Ok, base + chrono::Duration::seconds(i)));
        }

        spool.push(&batch).await.expect("push");
        let remaining: Vec<_> = spool.peek(10).await.expect("peek").iter().map(|o| o.id).collect();
        assert!(
            remaining.contains(&failure.id),
            "the oldest row was the failure, and it must not be evicted to keep successes"
        );
    }

    #[tokio::test]
    async fn the_byte_limit_bounds_growth() {
        let spool = Spool::open_in_memory(SpoolLimits {
            max_bytes: 4096,
            ..Default::default()
        })
        .await
        .expect("open");

        let large = serde_json::json!({ "blob": "x".repeat(512) });
        let batch: Vec<_> = (0..64)
            .map(|_| observation(ProbeStatus::Ok).with_payload(large.clone()))
            .collect();

        spool.push(&batch).await.expect("push");
        assert!(spool.size_bytes().await.expect("size") <= 4096);
        assert!(spool.len().await.expect("len") < 64);
    }

    #[tokio::test]
    async fn the_oldest_timestamp_is_reported_for_operator_visibility() {
        let spool = spool().await;
        let old = now() - chrono::Duration::minutes(30);
        spool
            .push(&[
                observation_at(ProbeStatus::Ok, old),
                observation_at(ProbeStatus::Ok, now()),
            ])
            .await
            .expect("push");
        assert_eq!(spool.oldest().await.expect("oldest"), Some(old));
    }

    #[tokio::test]
    async fn a_spool_on_disk_survives_being_reopened() {
        // A crashed agent must come back to find its evidence intact.
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("state/spool.db");
        let observation = observation(ProbeStatus::Failed);

        {
            let spool = Spool::open(&path, SpoolLimits::default()).await.expect("open");
            spool.push(std::slice::from_ref(&observation)).await.expect("push");
            spool.close().await;
        }

        let reopened = Spool::open(&path, SpoolLimits::default()).await.expect("reopen");
        let peeked = reopened.peek(10).await.expect("peek");
        assert_eq!(peeked.len(), 1);
        assert_eq!(peeked[0].id, observation.id);
    }

    #[tokio::test]
    async fn pushing_nothing_is_harmless() {
        let spool = spool().await;
        assert_eq!(spool.push(&[]).await.expect("push"), EvictionOutcome::default());
    }
}
