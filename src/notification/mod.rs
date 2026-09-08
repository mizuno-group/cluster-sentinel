//! Notifications.
//!
//! The hard part of alerting is not sending messages; it is **not** sending
//! them. A monitoring system that re-notifies every polling interval trains its
//! operators to filter it out, and then the one alert that mattered is filtered
//! out too.
//!
//! So notification here is driven by *change*, never by state
//! (IMPLEMENTATION.md §77). An incident that is still open and still says the
//! same thing produces nothing at all.
//!
//! Deduplication is separate from that and belongs to a different problem: the
//! controller and a fallback notifier may both notice the same incident, and
//! the operator should hear about it once (SPEC.md §110, §111).

mod dedup;
mod maintenance;
mod provider;

pub use dedup::{Deduplicator, NotificationRecord};
pub use maintenance::{MaintenanceWindow, MaintenanceWindows};
pub use provider::{NotificationProvider, ProviderError, WebhookProvider};

use serde::{Deserialize, Serialize};

use crate::incident::{Incident, IncidentUpdate, Severity};
use crate::time::{now, Timestamp};

/// Why a notification is being sent.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Trigger {
    /// A new incident was opened.
    Opened,
    /// An existing incident became more severe.
    Escalated,
    /// The diagnosis changed in a way worth reporting.
    DiagnosisChanged,
    /// The cause is gone but dependents have not recovered.
    Recovering,
    /// Everything involved has recovered.
    Resolved,
}

impl Trigger {
    /// Stable string form.
    pub fn as_str(&self) -> &'static str {
        match self {
            Trigger::Opened => "opened",
            Trigger::Escalated => "escalated",
            Trigger::DiagnosisChanged => "diagnosis_changed",
            Trigger::Recovering => "recovering",
            Trigger::Resolved => "resolved",
        }
    }

    /// Whether this trigger is good news.
    pub fn is_recovery(&self) -> bool {
        matches!(self, Trigger::Recovering | Trigger::Resolved)
    }
}

/// A message about one incident.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Notification {
    /// The incident this concerns.
    pub incident_id: String,
    /// The incident's fingerprint, which is also the deduplication scope.
    pub fingerprint: String,
    /// Why this is being sent.
    pub trigger: Trigger,
    /// Severity at the time of sending.
    pub severity: Severity,
    /// One-line summary.
    pub title: String,
    /// The detail an operator needs to decide whether to get up.
    pub body: String,
    /// Read-only commands worth running.
    pub recommended_actions: Vec<String>,
    /// When this was produced.
    pub created_at: Timestamp,
}

impl Notification {
    /// The key that decides whether this has already been sent.
    ///
    /// Scoped to the incident *and the trigger*, so a resolution is still
    /// delivered after the opening was: they are different news.
    pub fn deduplication_key(&self) -> String {
        format!("{}:{}", self.fingerprint, self.trigger.as_str())
    }

    /// Build a notification for an incident.
    pub fn for_incident(incident: &Incident, trigger: Trigger) -> Self {
        let cause = if incident.suspected_root_entities.is_empty() {
            String::new()
        } else {
            format!(" ({} suspected)", incident.suspected_root_entities.len())
        };

        let title = match trigger {
            Trigger::Resolved => format!("RESOLVED: {}", summary_of(incident)),
            Trigger::Recovering => format!("RECOVERING: {}", summary_of(incident)),
            Trigger::Escalated => {
                format!(
                    "{} (escalated): {}",
                    incident.severity.to_string().to_uppercase(),
                    summary_of(incident)
                )
            }
            _ => format!(
                "{}: {}{cause}",
                incident.severity.to_string().to_uppercase(),
                summary_of(incident)
            ),
        };

        let mut body = String::new();
        for diagnosis in &incident.diagnoses {
            body.push_str(&format!(
                "{} [{}]\n{}\n\n",
                diagnosis.diagnosis_type, diagnosis.confidence, diagnosis.summary
            ));
        }
        body.push_str(&format!("Incident: {}\n", incident.id));
        body.push_str(&format!("Status:   {}\n", incident.status));
        body.push_str(&format!("Evidence: {} observation(s)\n", incident.evidence.len()));

        Self {
            incident_id: incident.id.to_string(),
            fingerprint: incident.fingerprint.clone(),
            trigger,
            severity: incident.severity,
            title,
            body,
            recommended_actions: incident
                .diagnoses
                .iter()
                .flat_map(|d| d.recommended_actions.clone())
                .collect(),
            created_at: now(),
        }
    }
}

fn summary_of(incident: &Incident) -> String {
    incident
        .primary_diagnosis()
        .map(|d| d.summary.clone())
        .unwrap_or_else(|| format!("incident {}", incident.fingerprint))
}

/// Turn what changed into the notifications worth sending.
///
/// Note what is absent: nothing is produced for an incident that is merely
/// still open. Only change is news.
pub fn notifications_for(update: &IncidentUpdate) -> Vec<Notification> {
    let mut notifications = Vec::new();

    for incident in &update.opened {
        notifications.push(Notification::for_incident(incident, Trigger::Opened));
    }
    for incident in &update.recovering {
        notifications.push(Notification::for_incident(incident, Trigger::Recovering));
    }
    for incident in &update.resolved {
        notifications.push(Notification::for_incident(incident, Trigger::Resolved));
    }

    // An updated incident is only news if something about it changed. The
    // timeline is the record of that.
    for incident in &update.updated {
        if let Some(trigger) = trigger_for_update(incident) {
            notifications.push(Notification::for_incident(incident, trigger));
        }
    }

    notifications
}

/// Whether an updated incident changed in a way worth reporting.
fn trigger_for_update(incident: &Incident) -> Option<Trigger> {
    // Only events from this reconciliation matter, and those are the ones at
    // the end of the timeline.
    let recent = incident.timeline.iter().rev().take(4);

    for event in recent {
        match event.kind.as_str() {
            "severity_escalated" => return Some(Trigger::Escalated),
            "diagnosis_changed" => return Some(Trigger::DiagnosisChanged),
            "reopened" => return Some(Trigger::Opened),
            _ => {}
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::diagnosis::{kind, Confidence, Diagnosis};
    use crate::incident::{IncidentStatus, TimelineEvent};
    use crate::observation::ObservationId;

    fn incident(severity: Severity) -> Incident {
        let mut incident = Incident::open("cause:fs1", severity);
        incident.add_diagnosis(
            Diagnosis::new(kind::NFS_SERVICE_FAILURE, "storage.service_failure", Confidence::High)
                .with_summary("fs1 is up but the export port is not answering")
                .with_evidence([ObservationId::new()])
                .recommending(vec!["systemctl status nfs-server".into()]),
        );
        incident
    }

    #[test]
    fn a_new_incident_produces_one_notification() {
        let update = IncidentUpdate {
            opened: vec![incident(Severity::Critical)],
            ..Default::default()
        };
        let notifications = notifications_for(&update);

        assert_eq!(notifications.len(), 1);
        assert_eq!(notifications[0].trigger, Trigger::Opened);
        assert!(
            notifications[0].title.starts_with("CRITICAL"),
            "{}",
            notifications[0].title
        );
        assert!(
            notifications[0].body.contains("export port"),
            "{}",
            notifications[0].body
        );
    }

    #[test]
    fn an_unchanged_incident_produces_nothing() {
        // The single most important property here. Re-notifying every polling
        // interval is how a monitoring system teaches people to ignore it.
        let update = IncidentUpdate {
            updated: vec![incident(Severity::Critical)],
            ..Default::default()
        };
        assert!(notifications_for(&update).is_empty());
    }

    #[test]
    fn an_escalation_is_news() {
        let mut incident = incident(Severity::Warning);
        incident.escalate(Severity::Critical);

        let update = IncidentUpdate {
            updated: vec![incident],
            ..Default::default()
        };
        let notifications = notifications_for(&update);

        assert_eq!(notifications.len(), 1);
        assert_eq!(notifications[0].trigger, Trigger::Escalated);
        assert!(
            notifications[0].title.contains("escalated"),
            "{}",
            notifications[0].title
        );
    }

    #[test]
    fn a_changed_diagnosis_is_news() {
        let mut incident = incident(Severity::Warning);
        incident.timeline.push(TimelineEvent::new(
            "diagnosis_changed",
            "NFS_SERVICE_FAILURE -> SHARED_STORAGE_FAILURE",
        ));

        let update = IncidentUpdate {
            updated: vec![incident],
            ..Default::default()
        };
        assert_eq!(notifications_for(&update)[0].trigger, Trigger::DiagnosisChanged);
    }

    #[test]
    fn a_reopened_incident_is_news() {
        let mut incident = incident(Severity::Warning);
        incident
            .timeline
            .push(TimelineEvent::new("reopened", "the fault returned"));

        let update = IncidentUpdate {
            updated: vec![incident],
            ..Default::default()
        };
        assert_eq!(notifications_for(&update)[0].trigger, Trigger::Opened);
    }

    #[test]
    fn recovery_and_resolution_are_both_reported() {
        // An operator who was told about the fault is owed the ending.
        let mut recovering = incident(Severity::Critical);
        recovering.status = IncidentStatus::Recovering;
        let mut resolved = incident(Severity::Critical);
        resolved.resolve();

        let update = IncidentUpdate {
            recovering: vec![recovering],
            resolved: vec![resolved],
            ..Default::default()
        };
        let triggers: Vec<_> = notifications_for(&update).into_iter().map(|n| n.trigger).collect();

        assert!(triggers.contains(&Trigger::Recovering));
        assert!(triggers.contains(&Trigger::Resolved));
    }

    #[test]
    fn a_resolution_notification_reads_as_good_news() {
        let mut resolved = incident(Severity::Critical);
        resolved.resolve();

        let notification = Notification::for_incident(&resolved, Trigger::Resolved);
        assert!(notification.title.starts_with("RESOLVED"), "{}", notification.title);
        assert!(notification.trigger.is_recovery());
    }

    #[test]
    fn the_deduplication_key_separates_opening_from_resolution() {
        // Otherwise the resolution would be suppressed as a duplicate of the
        // alert, and the operator would never learn it was over.
        let incident = incident(Severity::Critical);
        let opened = Notification::for_incident(&incident, Trigger::Opened);
        let resolved = Notification::for_incident(&incident, Trigger::Resolved);

        assert_ne!(opened.deduplication_key(), resolved.deduplication_key());
        assert!(opened.deduplication_key().starts_with("cause:fs1"));
    }

    #[test]
    fn the_same_event_from_two_notifiers_shares_a_key() {
        // SPEC.md §111: the controller and a fallback notifier must not both
        // wake the same person.
        let incident = incident(Severity::Critical);
        let from_controller = Notification::for_incident(&incident, Trigger::Opened);
        let from_fallback = Notification::for_incident(&incident, Trigger::Opened);

        assert_eq!(from_controller.deduplication_key(), from_fallback.deduplication_key());
    }

    #[test]
    fn a_notification_carries_the_read_only_actions() {
        let notification = Notification::for_incident(&incident(Severity::Critical), Trigger::Opened);
        assert_eq!(notification.recommended_actions, ["systemctl status nfs-server"]);
    }

    #[test]
    fn an_empty_update_produces_nothing() {
        assert!(notifications_for(&IncidentUpdate::default()).is_empty());
    }
}
