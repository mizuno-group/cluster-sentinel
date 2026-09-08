//! Diagnosis: turning state and topology into a cause.
//!
//! Rules are typed Rust, evaluated deterministically. There is no rule DSL and
//! no LLM in this path (SPEC.md §94, IMPLEMENTATION.md §33, §71): an
//! explanation an operator is woken up for must be reproducible from the stored
//! evidence.

mod context;
mod engine;
pub mod rules;

pub use context::{DiagnosisContext, ObservationIndex};
pub use engine::{builtin_rules, DiagnosisEngine, DiagnosisRule};

use std::fmt;

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::entity::EntityId;
use crate::observation::ObservationId;
use crate::time::{now, Timestamp};

/// The conclusion a rule reached. A string, so an integration can add its own
/// without editing a core enum; the built-in ones are in [`kind`].
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct DiagnosisType(String);

impl DiagnosisType {
    /// Build a diagnosis type.
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    /// The diagnosis type as a string.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl From<&str> for DiagnosisType {
    fn from(s: &str) -> Self {
        DiagnosisType::new(s)
    }
}

impl fmt::Display for DiagnosisType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// The diagnoses the built-in rules can produce (IMPLEMENTATION.md §71).
pub mod kind {
    /// No independent observer can reach the entity.
    pub const HOST_UNREACHABLE: &str = "HOST_UNREACHABLE";
    /// Some observers reach the entity and others do not: the path is at fault.
    pub const PATH_SPECIFIC_NETWORK_FAILURE: &str = "PATH_SPECIFIC_NETWORK_FAILURE";
    /// SSH is down while the host answers otherwise.
    pub const SSH_SERVICE_FAILURE: &str = "SSH_SERVICE_FAILURE";
    /// The Sentinel agent is down while the host answers otherwise.
    pub const SENTINEL_AGENT_FAILURE: &str = "SENTINEL_AGENT_FAILURE";
    /// `slurmd` is down while the host is healthy.
    pub const SLURMD_SERVICE_FAILURE: &str = "SLURMD_SERVICE_FAILURE";
    /// The host is healthy but the scheduler will not use it.
    pub const SLURM_ONLY_DEGRADATION: &str = "SLURM_ONLY_DEGRADATION";
    /// The scheduler control plane itself is impaired.
    pub const SLURM_CONTROL_PLANE_FAILURE: &str = "SLURM_CONTROL_PLANE_FAILURE";
    /// Configured resources do not match observed hardware.
    pub const RESOURCE_CONFIGURATION_MISMATCH: &str = "RESOURCE_CONFIGURATION_MISMATCH";
    /// An NFS server's export service has failed.
    pub const NFS_SERVICE_FAILURE: &str = "NFS_SERVICE_FAILURE";
    /// One client's NFS access is broken while the server is fine.
    pub const NFS_CLIENT_FAILURE: &str = "NFS_CLIENT_FAILURE";
    /// Several clients of one storage entity failed together.
    pub const SHARED_STORAGE_FAILURE: &str = "SHARED_STORAGE_FAILURE";
    /// GPU count or model disagrees with the scheduler's expectation.
    pub const GPU_CONFIGURATION_MISMATCH: &str = "GPU_CONFIGURATION_MISMATCH";
    /// Wall clocks disagree beyond the configured tolerance.
    pub const CLOCK_SKEW: &str = "CLOCK_SKEW";
    /// The host's boot id changed.
    pub const HOST_REBOOTED: &str = "HOST_REBOOTED";
}

/// How sure the engine is (SPEC.md §93, IMPLEMENTATION.md §73).
///
/// [`Confidence::Confirmed`] requires *direct* evidence. Peer reachability
/// alone never exceeds [`Confidence::High`] — which is exactly why Sentinel
/// reports `HOST_UNREACHABLE` and not `POWER_OFF` (SPEC.md §52).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Confidence {
    /// A weak hypothesis worth showing but not acting on.
    Low,
    /// Plausible, with partial evidence.
    Medium,
    /// Strongly supported by independent evidence.
    High,
    /// Directly evidenced; no inference involved.
    Confirmed,
}

impl Confidence {
    /// Stable string form used in the database.
    pub fn as_str(&self) -> &'static str {
        match self {
            Confidence::Low => "low",
            Confidence::Medium => "medium",
            Confidence::High => "high",
            Confidence::Confirmed => "confirmed",
        }
    }

    /// Parse the stable string form.
    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "low" => Confidence::Low,
            "medium" => Confidence::Medium,
            "high" => Confidence::High,
            "confirmed" => Confidence::Confirmed,
            _ => return None,
        })
    }
}

impl fmt::Display for Confidence {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Identifier of the rule that produced a diagnosis, so a surprising
/// conclusion can be traced back to the code that drew it.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct RuleId(String);

impl RuleId {
    /// Build a rule id.
    pub fn new(id: impl Into<String>) -> Self {
        Self(id.into())
    }

    /// The rule id as a string.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl From<&str> for RuleId {
    fn from(s: &str) -> Self {
        RuleId::new(s)
    }
}

impl fmt::Display for RuleId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// One conclusion, with the evidence behind it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Diagnosis {
    /// Diagnosis identifier.
    pub id: Uuid,
    /// What the rule concluded.
    pub diagnosis_type: DiagnosisType,
    /// Entities showing symptoms.
    pub affected_entities: Vec<EntityId>,
    /// Entities suspected of causing them.
    pub suspected_root_entities: Vec<EntityId>,
    /// How sure the rule is.
    pub confidence: Confidence,
    /// Observations backing the conclusion. Never empty for a real diagnosis.
    pub evidence: Vec<ObservationId>,
    /// The rule that fired.
    pub rule_id: RuleId,
    /// Human-readable summary. Supplementary — never the only record
    /// (IMPLEMENTATION.md §72).
    pub summary: String,
    /// Read-only commands an operator may want to run (SPEC.md §114).
    pub recommended_actions: Vec<String>,
    /// When the diagnosis was produced.
    pub created_at: Timestamp,
}

impl Diagnosis {
    /// Build a diagnosis.
    pub fn new(diagnosis_type: impl Into<DiagnosisType>, rule_id: impl Into<RuleId>, confidence: Confidence) -> Self {
        Self {
            id: Uuid::new_v4(),
            diagnosis_type: diagnosis_type.into(),
            affected_entities: Vec::new(),
            suspected_root_entities: Vec::new(),
            confidence,
            evidence: Vec::new(),
            rule_id: rule_id.into(),
            summary: String::new(),
            recommended_actions: Vec::new(),
            created_at: now(),
        }
    }

    /// Builder: set affected entities.
    pub fn affecting(mut self, entities: impl IntoIterator<Item = EntityId>) -> Self {
        self.affected_entities = entities.into_iter().collect();
        self
    }

    /// Builder: set suspected root entities.
    pub fn rooted_at(mut self, entities: impl IntoIterator<Item = EntityId>) -> Self {
        self.suspected_root_entities = entities.into_iter().collect();
        self
    }

    /// Builder: attach evidence.
    pub fn with_evidence(mut self, evidence: impl IntoIterator<Item = ObservationId>) -> Self {
        self.evidence = evidence.into_iter().collect();
        self
    }

    /// Builder: set the summary.
    pub fn with_summary(mut self, summary: impl Into<String>) -> Self {
        self.summary = summary.into();
        self
    }

    /// Builder: suggest read-only investigation commands.
    pub fn recommending(mut self, actions: impl IntoIterator<Item = String>) -> Self {
        self.recommended_actions = actions.into_iter().collect();
        self
    }

    /// Whether this diagnosis is of the given type.
    pub fn is(&self, diagnosis_type: &str) -> bool {
        self.diagnosis_type.as_str() == diagnosis_type
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::entity::{EntityKey, EntityType};

    #[test]
    fn confidence_orders_from_low_to_confirmed() {
        assert!(Confidence::Low < Confidence::Medium);
        assert!(Confidence::Medium < Confidence::High);
        assert!(Confidence::High < Confidence::Confirmed);
    }

    #[test]
    fn confidence_names_roundtrip() {
        for confidence in [
            Confidence::Low,
            Confidence::Medium,
            Confidence::High,
            Confidence::Confirmed,
        ] {
            assert_eq!(Confidence::parse(confidence.as_str()), Some(confidence));
        }
    }

    #[test]
    fn a_diagnosis_carries_structured_evidence_not_only_prose() {
        let entity = EntityKey::new("env", EntityType::Host, "node01").entity_id();
        let evidence = ObservationId::new();
        let diagnosis = Diagnosis::new(kind::SSH_SERVICE_FAILURE, "ssh.only", Confidence::High)
            .affecting([entity])
            .rooted_at([entity])
            .with_evidence([evidence])
            .with_summary("sshd is down");

        assert!(diagnosis.is(kind::SSH_SERVICE_FAILURE));
        assert_eq!(diagnosis.evidence, vec![evidence]);
        assert_eq!(diagnosis.affected_entities, vec![entity]);
        assert_eq!(diagnosis.rule_id.as_str(), "ssh.only");
    }

    #[test]
    fn diagnosis_type_is_open_for_integrations() {
        let custom = Diagnosis::new("CEPH_PG_DEGRADED", "ceph.pg", Confidence::Medium);
        assert!(custom.is("CEPH_PG_DEGRADED"));
    }
}
