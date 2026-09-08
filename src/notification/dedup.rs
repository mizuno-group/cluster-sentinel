//! Notification deduplication (SPEC.md §111).
//!
//! Two problems share this mechanism:
//!
//! * The controller and a fallback notifier may both notice the same incident.
//!   The operator should hear about it once.
//! * A retried send must not become a second alert.
//!
//! The record is persisted, so a controller restart does not re-announce
//! everything it already announced.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use crate::time::{now, Timestamp};

use super::Notification;

/// How long a delivered notification suppresses an identical one.
///
/// Long enough that a flapping fault does not re-alert every cycle; short
/// enough that a genuinely recurring problem is heard about again.
pub const DEFAULT_LEASE_MINUTES: i64 = 60;

/// A notification that has been sent.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NotificationRecord {
    /// The key that identifies this piece of news.
    pub deduplication_key: String,
    /// Which provider sent it.
    pub provider: String,
    /// When it was sent.
    pub sent_at: Timestamp,
}

/// Decides whether a notification is new.
#[derive(Debug, Clone)]
pub struct Deduplicator {
    sent: HashMap<(String, String), Timestamp>,
    lease_minutes: i64,
}

impl Default for Deduplicator {
    fn default() -> Self {
        Self::new()
    }
}

impl Deduplicator {
    /// A deduplicator with the default lease.
    pub fn new() -> Self {
        Self {
            sent: HashMap::new(),
            lease_minutes: DEFAULT_LEASE_MINUTES,
        }
    }

    /// Builder: set the lease.
    pub fn with_lease_minutes(mut self, minutes: i64) -> Self {
        self.lease_minutes = minutes;
        self
    }

    /// Load previously sent notifications, for resuming after a restart.
    pub fn seed(&mut self, records: impl IntoIterator<Item = NotificationRecord>) {
        for record in records {
            self.sent
                .insert((record.provider, record.deduplication_key), record.sent_at);
        }
    }

    /// Whether this notification should be sent by this provider.
    pub fn should_send(&self, notification: &Notification, provider: &str) -> bool {
        let key = (provider.to_string(), notification.deduplication_key());
        match self.sent.get(&key) {
            None => true,
            Some(sent_at) => now() - *sent_at >= chrono::Duration::minutes(self.lease_minutes),
        }
    }

    /// Record that a notification was sent.
    pub fn record(&mut self, notification: &Notification, provider: &str) -> NotificationRecord {
        let sent_at = now();
        self.sent
            .insert((provider.to_string(), notification.deduplication_key()), sent_at);
        NotificationRecord {
            deduplication_key: notification.deduplication_key(),
            provider: provider.to_string(),
            sent_at,
        }
    }

    /// Filter a batch down to what is actually new.
    pub fn filter<'a>(&self, notifications: &'a [Notification], provider: &str) -> Vec<&'a Notification> {
        notifications.iter().filter(|n| self.should_send(n, provider)).collect()
    }

    /// How many records are held.
    pub fn len(&self) -> usize {
        self.sent.len()
    }

    /// Whether nothing has been sent.
    pub fn is_empty(&self) -> bool {
        self.sent.is_empty()
    }

    /// Drop records older than the lease, so the map does not grow forever.
    pub fn prune(&mut self) -> usize {
        let cutoff = now() - chrono::Duration::minutes(self.lease_minutes * 2);
        let before = self.sent.len();
        self.sent.retain(|_, sent_at| *sent_at >= cutoff);
        before - self.sent.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::diagnosis::{kind, Confidence, Diagnosis};
    use crate::incident::{Incident, Severity};
    use crate::notification::Trigger;

    fn notification(trigger: Trigger) -> Notification {
        let mut incident = Incident::open("cause:fs1", Severity::Critical);
        incident.add_diagnosis(
            Diagnosis::new(kind::NFS_SERVICE_FAILURE, "storage.service_failure", Confidence::High)
                .with_summary("the export port is not answering"),
        );
        Notification::for_incident(&incident, trigger)
    }

    #[test]
    fn a_new_notification_is_sent() {
        let deduplicator = Deduplicator::new();
        assert!(deduplicator.should_send(&notification(Trigger::Opened), "webhook"));
    }

    #[test]
    fn the_same_notification_is_not_sent_twice() {
        let mut deduplicator = Deduplicator::new();
        let notification = notification(Trigger::Opened);

        deduplicator.record(&notification, "webhook");
        assert!(!deduplicator.should_send(&notification, "webhook"));
    }

    #[test]
    fn a_second_notifier_does_not_repeat_the_first() {
        // SPEC.md §111: the controller and a fallback notifier both noticing
        // one incident must not wake the operator twice.
        let mut deduplicator = Deduplicator::new();
        let from_controller = notification(Trigger::Opened);
        let from_fallback = notification(Trigger::Opened);

        deduplicator.record(&from_controller, "webhook");
        assert!(!deduplicator.should_send(&from_fallback, "webhook"));
    }

    #[test]
    fn a_resolution_is_still_delivered_after_the_alert() {
        // The most important exception. Suppressing this would leave an
        // operator believing a fault was ongoing after it was fixed.
        let mut deduplicator = Deduplicator::new();
        deduplicator.record(&notification(Trigger::Opened), "webhook");

        assert!(deduplicator.should_send(&notification(Trigger::Resolved), "webhook"));
    }

    #[test]
    fn an_escalation_is_delivered_after_the_opening() {
        let mut deduplicator = Deduplicator::new();
        deduplicator.record(&notification(Trigger::Opened), "webhook");
        assert!(deduplicator.should_send(&notification(Trigger::Escalated), "webhook"));
    }

    #[test]
    fn different_providers_each_get_their_own_copy() {
        // Deduplication is per destination: silencing Slack must not silence
        // the pager.
        let mut deduplicator = Deduplicator::new();
        let notification = notification(Trigger::Opened);

        deduplicator.record(&notification, "webhook");
        assert!(deduplicator.should_send(&notification, "ntfy"));
    }

    #[test]
    fn the_lease_expires_so_a_recurring_fault_is_heard_about_again() {
        let mut deduplicator = Deduplicator::new().with_lease_minutes(0);
        let notification = notification(Trigger::Opened);

        deduplicator.record(&notification, "webhook");
        assert!(deduplicator.should_send(&notification, "webhook"));
    }

    #[test]
    fn seeded_records_survive_a_restart() {
        // Otherwise every controller restart re-announces every open incident.
        let notification = notification(Trigger::Opened);
        let mut deduplicator = Deduplicator::new();
        deduplicator.seed([NotificationRecord {
            deduplication_key: notification.deduplication_key(),
            provider: "webhook".into(),
            sent_at: now(),
        }]);

        assert!(!deduplicator.should_send(&notification, "webhook"));
    }

    #[test]
    fn filtering_a_batch_keeps_only_what_is_new() {
        let mut deduplicator = Deduplicator::new();
        let batch = vec![notification(Trigger::Opened), notification(Trigger::Resolved)];

        deduplicator.record(&batch[0], "webhook");
        let new = deduplicator.filter(&batch, "webhook");

        assert_eq!(new.len(), 1);
        assert_eq!(new[0].trigger, Trigger::Resolved);
    }

    #[test]
    fn pruning_drops_stale_records_but_keeps_current_ones() {
        let mut deduplicator = Deduplicator::new();
        deduplicator.record(&notification(Trigger::Opened), "webhook");
        assert_eq!(deduplicator.prune(), 0, "a fresh record is kept");
        assert_eq!(deduplicator.len(), 1);

        deduplicator
            .sent
            .values_mut()
            .for_each(|at| *at = now() - chrono::Duration::days(7));
        assert_eq!(deduplicator.prune(), 1);
        assert!(deduplicator.is_empty());
    }
}
