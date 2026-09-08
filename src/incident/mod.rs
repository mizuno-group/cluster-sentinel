//! Incidents: the operator-facing unit of "something is wrong".
//!
//! Several diagnoses about several entities collapse into one incident when
//! they share a cause (SPEC.md §98-§102). An incident is not resolved just
//! because the root service came back — dependent clients must recover too
//! (IMPLEMENTATION.md §75).

mod engine;

pub use engine::{fingerprint, severity_for, IncidentEngine, IncidentUpdate, REOPEN_WINDOW_MINUTES};

use std::fmt;

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::diagnosis::Diagnosis;
use crate::entity::EntityId;
use crate::observation::ObservationId;
use crate::time::{now, Timestamp};

/// Lifecycle of an incident (IMPLEMENTATION.md §75).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IncidentStatus {
    /// Active and unacknowledged.
    Open,
    /// An operator has seen it.
    Acknowledged,
    /// The root cause looks fixed but dependents have not all recovered.
    Recovering,
    /// Everything involved is healthy again.
    Resolved,
    /// Deliberately silenced, e.g. by a maintenance window.
    Suppressed,
}

impl IncidentStatus {
    /// Stable string form used in the database.
    pub fn as_str(&self) -> &'static str {
        match self {
            IncidentStatus::Open => "open",
            IncidentStatus::Acknowledged => "acknowledged",
            IncidentStatus::Recovering => "recovering",
            IncidentStatus::Resolved => "resolved",
            IncidentStatus::Suppressed => "suppressed",
        }
    }

    /// Parse the stable string form.
    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "open" => IncidentStatus::Open,
            "acknowledged" => IncidentStatus::Acknowledged,
            "recovering" => IncidentStatus::Recovering,
            "resolved" => IncidentStatus::Resolved,
            "suppressed" => IncidentStatus::Suppressed,
            _ => return None,
        })
    }

    /// Whether the incident still demands attention.
    pub fn is_active(&self) -> bool {
        matches!(
            self,
            IncidentStatus::Open | IncidentStatus::Acknowledged | IncidentStatus::Recovering
        )
    }
}

impl fmt::Display for IncidentStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// How much this incident matters (SPEC.md §100).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Severity {
    /// Worth recording, not worth waking anyone.
    Info,
    /// Degraded service.
    Warning,
    /// Loss of service, or a fault with wide fan-out.
    Critical,
}

impl Severity {
    /// Stable string form used in the database.
    pub fn as_str(&self) -> &'static str {
        match self {
            Severity::Info => "info",
            Severity::Warning => "warning",
            Severity::Critical => "critical",
        }
    }

    /// Parse the stable string form.
    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "info" => Severity::Info,
            "warning" => Severity::Warning,
            "critical" => Severity::Critical,
            _ => return None,
        })
    }
}

impl fmt::Display for Severity {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// One entry in an incident's history.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TimelineEvent {
    /// When it happened (wall clock).
    pub at: Timestamp,
    /// Short machine-readable kind, e.g. `diagnosis_added`.
    pub kind: String,
    /// Human-readable detail.
    pub detail: String,
    /// Entity the event concerns, if any.
    pub entity: Option<EntityId>,
}

impl TimelineEvent {
    /// Record a timeline event.
    pub fn new(kind: impl Into<String>, detail: impl Into<String>) -> Self {
        Self {
            at: now(),
            kind: kind.into(),
            detail: detail.into(),
            entity: None,
        }
    }

    /// Builder: attach an entity.
    pub fn about(mut self, entity: EntityId) -> Self {
        self.entity = Some(entity);
        self
    }
}

/// A correlated failure.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Incident {
    /// Incident identifier.
    pub id: Uuid,
    /// Stable key used to recognise the same incident again and to deduplicate
    /// notifications between the controller and a fallback notifier
    /// (SPEC.md §111).
    pub fingerprint: String,
    /// Lifecycle status.
    pub status: IncidentStatus,
    /// Severity.
    pub severity: Severity,
    /// When the incident began.
    pub started_at: Timestamp,
    /// When it was resolved.
    pub ended_at: Option<Timestamp>,
    /// Entities showing symptoms.
    pub affected_entities: Vec<EntityId>,
    /// Entities suspected of causing them.
    pub suspected_root_entities: Vec<EntityId>,
    /// Diagnoses folded into this incident.
    pub diagnoses: Vec<Diagnosis>,
    /// Preserved observations (SPEC.md §103).
    pub evidence: Vec<ObservationId>,
    /// History.
    pub timeline: Vec<TimelineEvent>,
}

impl Incident {
    /// Open a new incident.
    pub fn open(fingerprint: impl Into<String>, severity: Severity) -> Self {
        Self {
            id: Uuid::new_v4(),
            fingerprint: fingerprint.into(),
            status: IncidentStatus::Open,
            severity,
            started_at: now(),
            ended_at: None,
            affected_entities: Vec::new(),
            suspected_root_entities: Vec::new(),
            diagnoses: Vec::new(),
            evidence: Vec::new(),
            timeline: Vec::new(),
        }
    }

    /// The most evidence one incident retains.
    ///
    /// Evidence accumulates while an incident is open, and a long-running one
    /// would otherwise grow without bound. The **earliest** observations are
    /// kept, because the ones from around the onset are what explain the cause;
    /// the thousandth identical failure adds nothing a post-mortem needs
    /// (SPEC.md §103, IMPLEMENTATION.md §55).
    pub const MAX_EVIDENCE: usize = 200;

    /// Fold a diagnosis into the incident, merging its entities and evidence,
    /// and recording it on the timeline.
    pub fn add_diagnosis(&mut self, diagnosis: Diagnosis) {
        self.timeline.push(TimelineEvent::new(
            "diagnosis_added",
            format!("{} ({})", diagnosis.diagnosis_type, diagnosis.confidence),
        ));
        self.merge_diagnosis(diagnosis);
    }

    /// Fold a diagnosis in **without** a timeline entry.
    ///
    /// For refreshing an unchanged incident: a timeline that gains a line every
    /// polling interval buries the events that mattered under a transcript of
    /// nothing happening.
    pub fn merge_diagnosis(&mut self, diagnosis: Diagnosis) {
        for entity in &diagnosis.affected_entities {
            if !self.affected_entities.contains(entity) {
                self.affected_entities.push(*entity);
            }
        }
        for entity in &diagnosis.suspected_root_entities {
            if !self.suspected_root_entities.contains(entity) {
                self.suspected_root_entities.push(*entity);
            }
        }
        for observation in &diagnosis.evidence {
            if self.evidence.len() >= Self::MAX_EVIDENCE {
                break;
            }
            if !self.evidence.contains(observation) {
                self.evidence.push(*observation);
            }
        }
        self.diagnoses.push(diagnosis);
    }

    /// The most recently added diagnosis.
    pub fn primary_diagnosis(&self) -> Option<&Diagnosis> {
        self.diagnoses.last()
    }

    /// Whether the incident contains a diagnosis of the given type.
    pub fn has_diagnosis(&self, diagnosis_type: &str) -> bool {
        self.diagnoses.iter().any(|d| d.is(diagnosis_type))
    }

    /// Raise the severity if `severity` is higher; returns whether it changed.
    pub fn escalate(&mut self, severity: Severity) -> bool {
        if severity > self.severity {
            let previous = self.severity;
            self.severity = severity;
            self.timeline.push(TimelineEvent::new(
                "severity_escalated",
                format!("{previous} -> {severity}"),
            ));
            return true;
        }
        false
    }

    /// Mark the incident resolved.
    ///
    /// The caller is responsible for having checked that dependents recovered
    /// too; [`Incident::status`] should pass through
    /// [`IncidentStatus::Recovering`] while they have not.
    pub fn resolve(&mut self) {
        self.status = IncidentStatus::Resolved;
        self.ended_at = Some(now());
        self.timeline
            .push(TimelineEvent::new("resolved", "all involved entities recovered"));
    }

    /// Duration so far, or total duration if resolved.
    pub fn duration(&self) -> chrono::Duration {
        self.ended_at.unwrap_or_else(now) - self.started_at
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::diagnosis::{kind, Confidence};
    use crate::entity::{EntityKey, EntityType};

    fn host(name: &str) -> EntityId {
        EntityKey::new("env", EntityType::Host, name).entity_id()
    }

    #[test]
    fn severity_orders_correctly() {
        assert!(Severity::Info < Severity::Warning);
        assert!(Severity::Warning < Severity::Critical);
    }

    #[test]
    fn status_names_roundtrip() {
        for status in [
            IncidentStatus::Open,
            IncidentStatus::Acknowledged,
            IncidentStatus::Recovering,
            IncidentStatus::Resolved,
            IncidentStatus::Suppressed,
        ] {
            assert_eq!(IncidentStatus::parse(status.as_str()), Some(status));
        }
    }

    #[test]
    fn only_open_acknowledged_and_recovering_are_active() {
        assert!(IncidentStatus::Open.is_active());
        assert!(IncidentStatus::Acknowledged.is_active());
        assert!(IncidentStatus::Recovering.is_active());
        assert!(!IncidentStatus::Resolved.is_active());
        assert!(!IncidentStatus::Suppressed.is_active());
    }

    #[test]
    fn folding_diagnoses_merges_entities_and_evidence_without_duplicates() {
        let observation = ObservationId::new();
        let mut incident = Incident::open("test", Severity::Warning);

        incident.add_diagnosis(
            Diagnosis::new(kind::NFS_SERVICE_FAILURE, "nfs.server", Confidence::High)
                .affecting([host("fs1")])
                .rooted_at([host("fs1")])
                .with_evidence([observation]),
        );
        incident.add_diagnosis(
            Diagnosis::new(kind::SHARED_STORAGE_FAILURE, "storage.shared", Confidence::High)
                .affecting([host("fs1"), host("c2")])
                .rooted_at([host("fs1")])
                .with_evidence([observation]),
        );

        assert_eq!(incident.affected_entities, vec![host("fs1"), host("c2")]);
        assert_eq!(incident.suspected_root_entities, vec![host("fs1")]);
        assert_eq!(incident.evidence, vec![observation], "evidence must be deduplicated");
        assert!(incident.has_diagnosis(kind::SHARED_STORAGE_FAILURE));
        assert_eq!(incident.diagnoses.len(), 2);
    }

    #[test]
    fn refreshing_an_incident_does_not_grow_its_timeline() {
        // A timeline that gains a line every ten seconds buries the events
        // that mattered under a transcript of nothing happening.
        let mut incident = Incident::open("test", Severity::Warning);
        let diagnosis = Diagnosis::new(kind::NFS_SERVICE_FAILURE, "nfs.server", Confidence::High);

        incident.add_diagnosis(diagnosis.clone());
        let after_first = incident.timeline.len();

        for _ in 0..10 {
            incident.merge_diagnosis(diagnosis.clone());
        }
        assert_eq!(incident.timeline.len(), after_first);
    }

    #[test]
    fn evidence_is_capped_so_a_long_incident_does_not_grow_without_bound() {
        let mut incident = Incident::open("test", Severity::Warning);
        let first = ObservationId::new();

        for _ in 0..(Incident::MAX_EVIDENCE * 2) {
            incident.merge_diagnosis(
                Diagnosis::new(kind::NFS_SERVICE_FAILURE, "nfs.server", Confidence::High)
                    .with_evidence([ObservationId::new()]),
            );
        }

        assert_eq!(incident.evidence.len(), Incident::MAX_EVIDENCE);

        // The earliest evidence is what explains the onset, so it is what is
        // kept.
        let mut fresh = Incident::open("test", Severity::Warning);
        fresh.merge_diagnosis(
            Diagnosis::new(kind::NFS_SERVICE_FAILURE, "nfs.server", Confidence::High).with_evidence([first]),
        );
        for _ in 0..(Incident::MAX_EVIDENCE * 2) {
            fresh.merge_diagnosis(
                Diagnosis::new(kind::NFS_SERVICE_FAILURE, "nfs.server", Confidence::High)
                    .with_evidence([ObservationId::new()]),
            );
        }
        assert_eq!(fresh.evidence.first(), Some(&first), "onset evidence must survive");
    }

    #[test]
    fn escalation_only_moves_upwards() {
        let mut incident = Incident::open("test", Severity::Warning);
        assert!(!incident.escalate(Severity::Info));
        assert_eq!(incident.severity, Severity::Warning);
        assert!(incident.escalate(Severity::Critical));
        assert_eq!(incident.severity, Severity::Critical);
        assert!(!incident.escalate(Severity::Warning), "severity must not silently drop");
    }

    #[test]
    fn resolving_stamps_the_end_time_and_the_timeline() {
        let mut incident = Incident::open("test", Severity::Critical);
        assert!(incident.ended_at.is_none());
        incident.resolve();
        assert_eq!(incident.status, IncidentStatus::Resolved);
        assert!(incident.ended_at.is_some());
        assert!(incident.timeline.iter().any(|e| e.kind == "resolved"));
    }
}
