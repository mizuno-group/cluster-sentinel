//! Deleting records that have outlived their usefulness.
//!
//! Three rules constrain what may be deleted, and they are enforced in SQL
//! rather than trusted to the caller:
//!
//! 1. **An open incident is never pruned**, at any age. An incident open for a
//!    year is a year-old unfixed fault.
//! 2. **Evidence outlives what cites it.** An observation referenced by a
//!    surviving incident or diagnosis is kept even when it is older than the
//!    observation window, because a diagnosis whose evidence has been deleted
//!    is an assertion nobody can check (SPEC.md §116).
//! 3. **Every entity keeps its most recent observations**, however old they
//!    are. Without this, a host down longer than the retention window would
//!    lose every trace of ever having been seen, and the system would forget
//!    its longest outage precisely because it was long.
//!
//! Pruning is one statement per class rather than a batched loop. On a
//! database that has grown for months the first pass can hold the write lock
//! for a while; agents spool through that, which is what the spool is for, and
//! `sentinel prune` exists so the first pass can be done deliberately instead
//! of arriving by surprise.

use sqlx::SqliteConnection;

use crate::config::{RetentionConfig, RetentionPeriod};
use crate::time::{now, to_rfc3339, Timestamp};

use super::{SqliteStore, StoreError};

/// Whether a pass deletes, or only reports what it would delete.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PruneMode {
    /// Delete.
    Delete,
    /// Count what would be deleted, then roll it back.
    ///
    /// The counts come from running the real statements and undoing them, not
    /// from a second set of `SELECT COUNT` queries that could drift out of
    /// agreement with what deletion actually does.
    DryRun,
}

/// The floor below which `keep_per_entity` is not allowed to fall.
///
/// Diagnosis reads a fixed window of recent observations per entity; a floor
/// under that would let pruning blind the thing pruning exists to protect.
pub const MIN_KEEP_PER_ENTITY: u32 = 32;

/// What one pruning pass removed.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PruneOutcome {
    /// Observations deleted.
    pub observations: u64,
    /// State transitions deleted.
    pub transitions: u64,
    /// Resolved incidents deleted.
    pub incidents: u64,
    /// Diagnoses deleted.
    pub diagnoses: u64,
}

impl PruneOutcome {
    /// Total rows deleted across every class.
    pub fn total(&self) -> u64 {
        self.observations + self.transitions + self.incidents + self.diagnoses
    }

    /// Whether the pass deleted anything.
    pub fn is_empty(&self) -> bool {
        self.total() == 0
    }
}

impl std::fmt::Display for PruneOutcome {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{} observations, {} transitions, {} incidents, {} diagnoses",
            self.observations, self.transitions, self.incidents, self.diagnoses
        )
    }
}

impl SqliteStore {
    /// Delete everything older than `config` allows.
    ///
    /// The order matters. Incidents go first so that the evidence they were
    /// holding is released in the same pass, rather than surviving until the
    /// next one an hour later.
    pub async fn prune(&self, config: &RetentionConfig) -> Result<PruneOutcome, StoreError> {
        self.prune_at(config, now(), PruneMode::Delete).await
    }

    /// [`SqliteStore::prune`] with an explicit clock and mode.
    ///
    /// The whole pass is one transaction, so the three rules hold together:
    /// an observation is never left cited by a diagnosis that a half-applied
    /// pass has already removed.
    pub async fn prune_at(
        &self,
        config: &RetentionConfig,
        now: Timestamp,
        mode: PruneMode,
    ) -> Result<PruneOutcome, StoreError> {
        let mut outcome = PruneOutcome::default();
        if !config.enabled {
            return Ok(outcome);
        }

        let mut tx = self.pool().begin().await?;
        outcome.incidents = prune_incidents(&mut tx, config.resolved_incidents, now).await?;
        outcome.diagnoses = prune_diagnoses(&mut tx, config.diagnoses, now).await?;
        outcome.observations = prune_observations(&mut tx, config.observations, config.keep_per_entity, now).await?;
        outcome.transitions = prune_transitions(&mut tx, config.transitions, config.keep_per_entity, now).await?;

        match mode {
            PruneMode::Delete => tx.commit().await?,
            PruneMode::DryRun => tx.rollback().await?,
        }

        Ok(outcome)
    }

    /// Return freed pages to the filesystem.
    ///
    /// Pruning alone stops the file growing, because SQLite reuses freed
    /// pages, but it does not shrink it. Only an operator lowering a retention
    /// period wants the space back immediately, and this rewrites the whole
    /// database to get it, so it is never run automatically.
    pub async fn vacuum(&self) -> Result<(), StoreError> {
        sqlx::query("VACUUM").execute(self.pool()).await?;
        Ok(())
    }

    /// Size of the database file in bytes, as SQLite accounts for it.
    pub async fn database_bytes(&self) -> Result<u64, StoreError> {
        use sqlx::Row;
        let row = sqlx::query("SELECT page_count * page_size AS bytes FROM pragma_page_count(), pragma_page_size()")
            .fetch_one(self.pool())
            .await?;
        Ok(row.try_get::<i64, _>("bytes")?.max(0) as u64)
    }
}

/// Delete resolved incidents that ended before the cutoff.
async fn prune_incidents(
    conn: &mut SqliteConnection,
    period: RetentionPeriod,
    now: Timestamp,
) -> Result<u64, StoreError> {
    let Some(cutoff) = period.cutoff(now) else {
        return Ok(0);
    };
    // `status = 'resolved'` and a non-null `ended_at` are both required:
    // an incident with no end has not ended, whatever its status column
    // says, and deleting it would lose an ongoing fault.
    let result = sqlx::query(
        "DELETE FROM incidents
             WHERE status = 'resolved' AND ended_at IS NOT NULL AND ended_at < ?",
    )
    .bind(to_rfc3339(cutoff))
    .execute(&mut *conn)
    .await?;
    Ok(result.rows_affected())
}

/// Delete diagnoses that belong to no surviving incident.
async fn prune_diagnoses(
    conn: &mut SqliteConnection,
    period: RetentionPeriod,
    now: Timestamp,
) -> Result<u64, StoreError> {
    let Some(cutoff) = period.cutoff(now) else {
        return Ok(0);
    };
    // A diagnosis attached to a live incident is part of that incident's
    // record and is kept as long as it is, regardless of this period.
    let result = sqlx::query(
        "DELETE FROM diagnoses
             WHERE created_at < ?
               AND (incident_id IS NULL OR incident_id NOT IN (SELECT id FROM incidents))",
    )
    .bind(to_rfc3339(cutoff))
    .execute(&mut *conn)
    .await?;
    Ok(result.rows_affected())
}

/// Delete observations older than the cutoff, keeping cited and recent ones.
async fn prune_observations(
    conn: &mut SqliteConnection,
    period: RetentionPeriod,
    keep_per_entity: u32,
    now: Timestamp,
) -> Result<u64, StoreError> {
    let Some(cutoff) = period.cutoff(now) else {
        return Ok(0);
    };
    let keep = keep_per_entity.max(MIN_KEEP_PER_ENTITY);
    let result = sqlx::query(
        "WITH keep AS (
                 SELECT target_entity_id AS eid, MIN(finished_at) AS keep_from
                 FROM (
                     SELECT target_entity_id, finished_at,
                            ROW_NUMBER() OVER (
                                PARTITION BY target_entity_id
                                ORDER BY finished_at DESC, id DESC
                            ) AS rn
                     FROM observations
                 )
                 WHERE rn <= ?2
                 GROUP BY target_entity_id
             )
             DELETE FROM observations
             WHERE finished_at < ?1
               AND finished_at < COALESCE(
                     (SELECT keep_from FROM keep WHERE eid = target_entity_id), ?1)
               AND id NOT IN (SELECT observation_id FROM diagnosis_evidence)
               AND id NOT IN (SELECT observation_id FROM incident_evidence)",
    )
    .bind(to_rfc3339(cutoff))
    .bind(keep)
    .execute(&mut *conn)
    .await?;
    Ok(result.rows_affected())
}

/// Delete state transitions older than the cutoff, keeping recent ones.
async fn prune_transitions(
    conn: &mut SqliteConnection,
    period: RetentionPeriod,
    keep_per_entity: u32,
    now: Timestamp,
) -> Result<u64, StoreError> {
    let Some(cutoff) = period.cutoff(now) else {
        return Ok(0);
    };
    let keep = keep_per_entity.max(MIN_KEEP_PER_ENTITY);
    let result = sqlx::query(
        "WITH keep AS (
                 SELECT entity_id AS eid, MIN(occurred_at) AS keep_from
                 FROM (
                     SELECT entity_id, occurred_at,
                            ROW_NUMBER() OVER (
                                PARTITION BY entity_id
                                ORDER BY occurred_at DESC, id DESC
                            ) AS rn
                     FROM state_transitions
                 )
                 WHERE rn <= ?2
                 GROUP BY entity_id
             )
             DELETE FROM state_transitions
             WHERE occurred_at < ?1
               AND occurred_at < COALESCE(
                     (SELECT keep_from FROM keep WHERE eid = entity_id), ?1)",
    )
    .bind(to_rfc3339(cutoff))
    .bind(keep)
    .execute(&mut *conn)
    .await?;
    Ok(result.rows_affected())
}
