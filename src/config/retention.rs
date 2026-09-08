//! How long the controller keeps what it has recorded.
//!
//! Every table the controller appends to grows without bound unless something
//! removes rows from it, and the append rates are not small: a five-host
//! testbed writes roughly ten kilobytes a second, so a few hundred nodes fill
//! a disk in weeks. A monitoring system that fills its own disk stops being a
//! monitoring system at the moment it is most needed.
//!
//! The knobs are per data class rather than one global age, because the
//! classes have very different value per byte. Observations are the bulk and
//! the least individually interesting -- one successful TCP connect from
//! yesterday tells nobody anything. An incident is a paragraph, and is the
//! thing an operator goes back to a year later to ask whether this has
//! happened before.
//!
//! Every period also accepts `"never"`, because a site with a compliance
//! requirement or a large disk is entitled to keep everything, and should be
//! able to say so explicitly rather than by setting an implausibly large
//! number of days.

use std::fmt;
use std::time::Duration;

use serde::de::{self, Deserializer};
use serde::{Deserialize, Serialize, Serializer};

/// How long one class of record is kept.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RetentionPeriod {
    /// Delete records older than this.
    For(Duration),
    /// Never delete.
    Forever,
}

impl RetentionPeriod {
    /// The words that mean "keep everything".
    const FOREVER_WORDS: [&'static str; 4] = ["never", "forever", "unlimited", "keep"];

    /// The cutoff before which records may be deleted, or `None` to keep all.
    pub fn cutoff(&self, now: crate::time::Timestamp) -> Option<crate::time::Timestamp> {
        match self {
            RetentionPeriod::Forever => None,
            RetentionPeriod::For(duration) => chrono::Duration::from_std(*duration).ok().map(|d| now - d),
        }
    }

    /// Whether anything will ever be deleted under this period.
    pub fn is_forever(&self) -> bool {
        matches!(self, RetentionPeriod::Forever)
    }
}

impl fmt::Display for RetentionPeriod {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            RetentionPeriod::Forever => f.write_str("never"),
            RetentionPeriod::For(duration) => write!(f, "{}", crate::time::format_duration(*duration)),
        }
    }
}

impl Serialize for RetentionPeriod {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.to_string())
    }
}

impl<'de> Deserialize<'de> for RetentionPeriod {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let text = String::deserialize(deserializer)?;
        let trimmed = text.trim();
        if Self::FOREVER_WORDS.iter().any(|w| trimmed.eq_ignore_ascii_case(w)) {
            return Ok(RetentionPeriod::Forever);
        }
        humantime::parse_duration(trimmed)
            .map(RetentionPeriod::For)
            .map_err(|e| {
                de::Error::custom(format!(
                    "{text:?} is not a retention period: {e} (use a duration like \"14d\", or \"never\")"
                ))
            })
    }
}

/// Retention settings.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RetentionConfig {
    /// Whether the controller prunes at all.
    ///
    /// Turning this off is a deliberate choice to manage the database by other
    /// means, not a default anyone should arrive at by accident.
    #[serde(default = "default_enabled")]
    pub enabled: bool,
    /// How often pruning runs. A pass also runs at startup.
    #[serde(default = "default_prune_interval", with = "humantime_serde")]
    pub interval: Duration,
    /// How long individual observations are kept.
    #[serde(default = "default_observations")]
    pub observations: RetentionPeriod,
    /// How many observations to keep per entity regardless of age.
    ///
    /// This is a floor, not a target. Without it, a host that has been down
    /// longer than the retention window would lose every observation proving
    /// it was ever seen, and would silently become an entity nobody has any
    /// evidence about -- the monitoring system forgetting the longest-running
    /// outage it has.
    #[serde(default = "default_keep_per_entity")]
    pub keep_per_entity: u32,
    /// How long state transitions are kept.
    #[serde(default = "default_transitions")]
    pub transitions: RetentionPeriod,
    /// How long resolved incidents are kept.
    ///
    /// Open incidents are never pruned, at any age. An incident that has been
    /// open for a year is a year-old unfixed fault, which is precisely the
    /// thing worth keeping.
    #[serde(default = "default_resolved_incidents")]
    pub resolved_incidents: RetentionPeriod,
    /// How long diagnoses not attached to any incident are kept.
    #[serde(default = "default_diagnoses")]
    pub diagnoses: RetentionPeriod,
}

fn default_enabled() -> bool {
    true
}

fn default_prune_interval() -> Duration {
    Duration::from_secs(3600)
}

fn default_observations() -> RetentionPeriod {
    RetentionPeriod::For(Duration::from_secs(14 * 24 * 3600))
}

fn default_keep_per_entity() -> u32 {
    64
}

fn default_transitions() -> RetentionPeriod {
    RetentionPeriod::For(Duration::from_secs(90 * 24 * 3600))
}

fn default_resolved_incidents() -> RetentionPeriod {
    RetentionPeriod::For(Duration::from_secs(180 * 24 * 3600))
}

fn default_diagnoses() -> RetentionPeriod {
    RetentionPeriod::For(Duration::from_secs(30 * 24 * 3600))
}

impl Default for RetentionConfig {
    fn default() -> Self {
        Self {
            enabled: default_enabled(),
            interval: default_prune_interval(),
            observations: default_observations(),
            keep_per_entity: default_keep_per_entity(),
            transitions: default_transitions(),
            resolved_incidents: default_resolved_incidents(),
            diagnoses: default_diagnoses(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Deserialize)]
    struct Wrapper {
        period: RetentionPeriod,
    }

    fn parse(text: &str) -> Result<RetentionPeriod, toml::de::Error> {
        toml::from_str::<Wrapper>(&format!("period = {text}")).map(|w| w.period)
    }

    #[test]
    fn a_duration_is_a_period() {
        assert_eq!(
            parse("\"14d\"").unwrap(),
            RetentionPeriod::For(Duration::from_secs(14 * 24 * 3600))
        );
    }

    #[test]
    fn several_spellings_mean_keep_everything() {
        for word in ["never", "Never", "forever", "unlimited", "keep"] {
            assert_eq!(parse(&format!("\"{word}\"")).unwrap(), RetentionPeriod::Forever);
        }
    }

    #[test]
    fn a_period_that_is_neither_is_refused_with_advice() {
        let error = parse("\"sometimes\"").unwrap_err().to_string();
        assert!(error.contains("14d"), "{error}");
        assert!(error.contains("never"), "{error}");
    }

    #[test]
    fn a_period_round_trips_through_toml() {
        for period in [
            RetentionPeriod::Forever,
            RetentionPeriod::For(Duration::from_secs(90 * 24 * 3600)),
        ] {
            let text = toml::to_string(&Wrapper2 { period }).unwrap();
            assert_eq!(toml::from_str::<Wrapper>(&text).unwrap().period, period);
        }
    }

    #[derive(Serialize)]
    struct Wrapper2 {
        period: RetentionPeriod,
    }

    #[test]
    fn forever_has_no_cutoff() {
        assert!(RetentionPeriod::Forever.cutoff(crate::time::now()).is_none());
    }

    #[test]
    fn a_cutoff_is_that_far_in_the_past() {
        let now = crate::time::now();
        let cutoff = RetentionPeriod::For(Duration::from_secs(3600)).cutoff(now).unwrap();
        assert_eq!(now - cutoff, chrono::Duration::hours(1));
    }

    #[test]
    fn the_defaults_prune_but_keep_incidents_longest() {
        let config = RetentionConfig::default();
        assert!(config.enabled);
        assert!(!config.observations.is_forever());
        // The cheapest bytes go first and the most valuable last.
        let age = |p: RetentionPeriod| match p {
            RetentionPeriod::For(d) => d,
            RetentionPeriod::Forever => Duration::MAX,
        };
        assert!(age(config.observations) < age(config.transitions));
        assert!(age(config.transitions) < age(config.resolved_incidents));
    }

    #[test]
    fn the_per_entity_floor_covers_what_diagnosis_reads() {
        // Diagnosis looks at a fixed window of recent observations per entity.
        // A floor below that would let pruning blind it.
        assert!(RetentionConfig::default().keep_per_entity >= 32);
    }
}
