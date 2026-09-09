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
    /// The incident it concerned.
    pub incident_id: String,
    /// The key that identifies this piece of news.
    pub deduplication_key: String,
    /// Which provider sent it.
    pub provider: String,
    /// When it was sent.
    pub sent_at: Timestamp,
}

/// The suffix a deduplication key carries when it announces an opening.
///
/// An opening is announced **once per incident**, so its record must not
/// expire the way repeatable news does. Keyed off [`Trigger::Opened`], and a
/// test below holds the two together.
fn is_opening(deduplication_key: &str) -> bool {
    deduplication_key.ends_with(&format!(":{}", super::Trigger::Opened.as_str()))
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
    ///
    /// The lease applies to news that can genuinely recur. **An opening
    /// cannot**: an incident is opened once, and re-announcing the same
    /// still-open incident an hour later is the behaviour this module exists
    /// to prevent. So an opening is sent only while no record of it exists,
    /// and the record is dropped when the incident resolves, which is what
    /// makes a genuine reopening audible again.
    pub fn should_send(&self, notification: &Notification, provider: &str) -> bool {
        let key = (provider.to_string(), notification.deduplication_key());
        match self.sent.get(&key) {
            None => true,
            Some(_) if is_opening(&key.1) => false,
            Some(sent_at) => now() - *sent_at >= chrono::Duration::minutes(self.lease_minutes),
        }
    }

    /// Forget that an incident's opening was announced.
    ///
    /// Called when the incident resolves: the same fault returning later is
    /// new news, and would otherwise be silenced by the one-shot rule above.
    /// Returns the keys dropped, so the persisted records can follow.
    pub fn forget_opening(&mut self, fingerprint: &str) -> Vec<String> {
        let suffix = format!(":{}", super::Trigger::Opened.as_str());
        let key = format!("{fingerprint}{suffix}");
        let mut dropped = Vec::new();
        self.sent.retain(|(_, existing), _| {
            let keep = existing != &key;
            if !keep {
                dropped.push(existing.clone());
            }
            keep
        });
        dropped
    }

    /// Record that a notification was sent.
    pub fn record(&mut self, notification: &Notification, provider: &str) -> NotificationRecord {
        let sent_at = now();
        self.sent
            .insert((provider.to_string(), notification.deduplication_key()), sent_at);
        NotificationRecord {
            incident_id: notification.incident_id.clone(),
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
    ///
    /// Openings are exempt: theirs is not a lease that expires but a record
    /// that this incident has been announced, and dropping it would make the
    /// controller announce a long-running incident all over again.
    pub fn prune(&mut self) -> usize {
        let cutoff = now() - chrono::Duration::minutes(self.lease_minutes * 2);
        let before = self.sent.len();
        self.sent
            .retain(|(_, key), sent_at| is_opening(key) || *sent_at >= cutoff);
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
        let notification = notification(Trigger::Escalated);

        deduplicator.record(&notification, "webhook");
        assert!(deduplicator.should_send(&notification, "webhook"));
    }

    #[test]
    fn an_opening_is_announced_once_however_long_the_incident_lasts() {
        // Every pass offers the opening of a still-open incident, which is
        // what lets an undelivered one be delivered later. If the lease
        // applied to it, a week-long incident would re-announce itself every
        // hour, which is precisely the behaviour this module exists to stop.
        let mut deduplicator = Deduplicator::new().with_lease_minutes(0);
        let notification = notification(Trigger::Opened);

        deduplicator.record(&notification, "webhook");
        assert!(!deduplicator.should_send(&notification, "webhook"));
    }

    #[test]
    fn pruning_does_not_forget_that_an_opening_was_announced() {
        // prune() exists to bound memory. Letting it drop opening records
        // would silently reintroduce the repeat it is unrelated to.
        let mut deduplicator = Deduplicator::new().with_lease_minutes(0);
        let notification = notification(Trigger::Opened);

        deduplicator.record(&notification, "webhook");
        deduplicator.prune();
        assert!(!deduplicator.should_send(&notification, "webhook"));
    }

    #[test]
    fn forgetting_an_opening_makes_the_same_fault_audible_again() {
        // Called when the incident resolves. The same fingerprint returning
        // next week is new news, not the old announcement.
        let mut deduplicator = Deduplicator::new();
        let notification = notification(Trigger::Opened);

        deduplicator.record(&notification, "webhook");
        let dropped = deduplicator.forget_opening(&notification.fingerprint);

        assert_eq!(dropped, vec![notification.deduplication_key()]);
        assert!(deduplicator.should_send(&notification, "webhook"));
    }

    #[test]
    fn forgetting_an_opening_leaves_other_news_about_it_alone() {
        let mut deduplicator = Deduplicator::new();
        let resolved = notification(Trigger::Resolved);

        deduplicator.record(&resolved, "webhook");
        assert!(deduplicator.forget_opening(&resolved.fingerprint).is_empty());
        assert!(!deduplicator.should_send(&resolved, "webhook"));
    }

    #[test]
    fn the_one_shot_rule_is_tied_to_the_trigger_it_names() {
        // is_opening() matches on the key's text. If Trigger::Opened were
        // renamed, openings would silently start expiring again.
        let key = notification(Trigger::Opened).deduplication_key();
        assert!(is_opening(&key));
        assert!(!is_opening(&notification(Trigger::Resolved).deduplication_key()));
    }

    #[test]
    fn seeded_records_survive_a_restart() {
        // Otherwise every controller restart re-announces every open incident.
        let notification = notification(Trigger::Opened);
        let mut deduplicator = Deduplicator::new();
        deduplicator.seed([NotificationRecord {
            incident_id: notification.incident_id.clone(),
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
        // Not an opening: those are exempt from pruning by design, and are
        // covered by their own test above.
        deduplicator.record(&notification(Trigger::Escalated), "webhook");
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
