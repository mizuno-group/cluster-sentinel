//! Persistence.
//!
//! SQLite with WAL for the MVP (SPEC.md §124). Everything goes through the
//! repository traits in this module so that moving to PostgreSQL later is a new
//! implementation rather than a rewrite (SPEC.md §125).

mod entities;
mod incidents;
mod observations;
mod retention;
mod sqlite;

pub use observations::IngestOutcome;
pub use retention::{PruneMode, PruneOutcome, MIN_KEEP_PER_ENTITY};
pub use sqlite::SqliteStore;

use std::path::Path;

use thiserror::Error;

/// A persistence failure.
#[derive(Debug, Error)]
pub enum StoreError {
    /// The underlying database reported an error.
    #[error("database error: {0}")]
    Database(#[from] sqlx::Error),
    /// A migration could not be applied.
    #[error("migration failed: {0}")]
    Migration(#[from] sqlx::migrate::MigrateError),
    /// A stored value could not be decoded into a domain type.
    #[error("cannot decode stored {kind}: {detail}")]
    Decode {
        /// What was being decoded.
        kind: &'static str,
        /// Why it failed.
        detail: String,
    },
    /// The database directory could not be prepared.
    #[error("cannot prepare database directory {path}: {source}")]
    Directory {
        /// The path that failed.
        path: std::path::PathBuf,
        /// The underlying I/O error.
        #[source]
        source: std::io::Error,
    },
}

/// Build a SQLite connection URL that creates the file if it is missing.
pub(crate) fn sqlite_url(path: &Path) -> String {
    if path == Path::new(":memory:") {
        return "sqlite::memory:".to_string();
    }
    format!("sqlite://{}?mode=rwc", path.display())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn memory_databases_get_the_memory_url() {
        assert_eq!(sqlite_url(Path::new(":memory:")), "sqlite::memory:");
    }

    #[test]
    fn file_databases_are_created_if_missing() {
        let url = sqlite_url(Path::new("/var/lib/sentinel/sentinel.db"));
        assert!(url.starts_with("sqlite:///var/lib/sentinel/sentinel.db"));
        assert!(url.contains("mode=rwc"));
    }
}
