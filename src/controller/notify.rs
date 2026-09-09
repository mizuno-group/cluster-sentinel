//! Sending notifications about what changed.
//!
//! Three filters sit between an incident and someone's phone, and each exists
//! because of a specific way alerting goes wrong:
//!
//! 1. **Change only.** An unchanged incident produces nothing, because
//!    re-notifying every interval is how a system teaches people to ignore it.
//! 2. **Severity floor.** A drained node belongs in `sentinel status`, not in
//!    someone's phone at 3am.
//! 3. **Maintenance.** Work that was planned should not wake the person who
//!    planned it — while the observation and the state continue regardless.

use std::sync::Arc;

use crate::incident::{IncidentUpdate, Severity};
use crate::notification::{
    notifications_for, Deduplicator, MaintenanceWindows, Notification, NotificationProvider, WebhookProvider,
};
use crate::persistence::StoreError;

use super::Controller;

/// What one notification pass did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct NotifyOutcome {
    /// Notifications delivered.
    pub sent: usize,
    /// Suppressed because an identical one was already sent.
    pub deduplicated: usize,
    /// Suppressed by a maintenance window.
    pub suppressed_by_maintenance: usize,
    /// Suppressed for being below the severity floor.
    pub below_threshold: usize,
    /// Delivery failures.
    pub failed: usize,
}

impl NotifyOutcome {
    /// Whether anything at all happened.
    pub fn is_empty(&self) -> bool {
        *self == Self::default()
    }
}

/// Build the configured providers.
pub fn providers_from_config(config: &crate::config::Config) -> Vec<Arc<dyn NotificationProvider>> {
    config
        .notification
        .webhooks
        .iter()
        .filter_map(|webhook| {
            match WebhookProvider::new(&webhook.name, &webhook.url, std::time::Duration::from_secs(10)) {
                Ok(provider) => Some(Arc::new(provider.with_format(webhook.format)) as Arc<dyn NotificationProvider>),
                Err(error) => {
                    // Refuse the destination, keep the controller running. A
                    // mistyped URL must not stop monitoring.
                    tracing::error!(webhook = %webhook.name, %error, "notification destination is unusable");
                    None
                }
            }
        })
        .collect()
}

/// The configured severity floor.
pub fn min_severity(config: &crate::config::Config) -> Severity {
    Severity::parse(&config.notification.min_severity).unwrap_or(Severity::Warning)
}

impl Controller {
    /// Send notifications for what a reconciliation changed.
    pub async fn notify(
        &mut self,
        update: &IncidentUpdate,
        providers: &[Arc<dyn NotificationProvider>],
        deduplicator: &mut Deduplicator,
        maintenance: &MaintenanceWindows,
    ) -> Result<NotifyOutcome, StoreError> {
        let mut outcome = NotifyOutcome::default();
        if providers.is_empty() {
            return Ok(outcome);
        }

        let floor = min_severity(self.config());
        let spacing = self.config().notification.min_interval;
        let notifications = notifications_for(update);
        let mut last_send: Option<std::time::Instant> = None;

        for notification in &notifications {
            // A recovery is always worth hearing, whatever its severity: an
            // operator told about a fault is owed the ending.
            if notification.severity < floor && !notification.trigger.is_recovery() {
                outcome.below_threshold += 1;
                continue;
            }

            if self.is_under_maintenance(notification, update, maintenance) {
                outcome.suppressed_by_maintenance += 1;
                continue;
            }

            for provider in providers {
                if !deduplicator.should_send(notification, provider.name()) {
                    outcome.deduplicated += 1;
                    continue;
                }

                // Spaced, not dropped. One fault can produce many
                // notifications at once, and a webhook is a shared,
                // rate-limited resource: sending them as fast as they are
                // produced is how the one that mattered gets a 429.
                if let Some(previous) = last_send {
                    let elapsed = previous.elapsed();
                    if elapsed < spacing {
                        tokio::time::sleep(spacing - elapsed).await;
                    }
                }
                last_send = Some(std::time::Instant::now());

                match provider.send(notification).await {
                    Ok(()) => {
                        deduplicator.record(notification, provider.name());
                        outcome.sent += 1;
                        tracing::info!(
                            provider = provider.name(),
                            trigger = notification.trigger.as_str(),
                            incident = %notification.incident_id,
                            "notification sent"
                        );
                    }
                    Err(error) => {
                        // Not recorded, so a transient failure is retried on
                        // the next pass rather than being lost.
                        outcome.failed += 1;
                        tracing::warn!(provider = provider.name(), %error, "cannot deliver notification");
                    }
                }
            }
        }

        Ok(outcome)
    }

    /// Whether every entity this notification concerns is under maintenance.
    fn is_under_maintenance(
        &self,
        notification: &Notification,
        update: &IncidentUpdate,
        maintenance: &MaintenanceWindows,
    ) -> bool {
        let Some(incident) = update
            .all()
            .into_iter()
            .find(|i| i.id.to_string() == notification.incident_id)
        else {
            return false;
        };
        maintenance.suppresses_all(&incident.affected_entities)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Config, NotificationConfig, WebhookConfig};
    use crate::diagnosis::{kind, Confidence, Diagnosis};
    use crate::entity::{EntityKey, EntityType};
    use crate::incident::Incident;
    use crate::notification::{MaintenanceWindow, ProviderError};
    use crate::persistence::SqliteStore;
    use async_trait::async_trait;

    fn host(name: &str) -> crate::entity::EntityId {
        EntityKey::new("lab", EntityType::Host, name).entity_id()
    }

    /// A provider that records what it was asked to send.
    struct Recording {
        name: String,
        sent: Arc<tokio::sync::Mutex<Vec<Notification>>>,
        fail: bool,
    }

    #[async_trait]
    impl NotificationProvider for Recording {
        fn name(&self) -> &str {
            &self.name
        }

        async fn send(&self, notification: &Notification) -> Result<(), ProviderError> {
            if self.fail {
                return Err(ProviderError::Unreachable {
                    destination: self.name.clone(),
                    detail: "test failure".into(),
                });
            }
            self.sent.lock().await.push(notification.clone());
            Ok(())
        }
    }

    fn recording(
        name: &str,
    ) -> (
        Arc<dyn NotificationProvider>,
        Arc<tokio::sync::Mutex<Vec<Notification>>>,
    ) {
        let sent = Arc::new(tokio::sync::Mutex::new(Vec::new()));
        let provider = Arc::new(Recording {
            name: name.into(),
            sent: Arc::clone(&sent),
            fail: false,
        }) as Arc<dyn NotificationProvider>;
        (provider, sent)
    }

    fn incident(severity: Severity, affected: &[&str]) -> Incident {
        let mut incident = Incident::open("cause:fs1", severity);
        incident.add_diagnosis(
            Diagnosis::new(kind::NFS_SERVICE_FAILURE, "storage.service_failure", Confidence::High)
                .with_summary("the export port is not answering")
                .affecting(affected.iter().map(|n| host(n)).collect::<Vec<_>>()),
        );
        incident
    }

    async fn controller(min_severity: &str) -> Controller {
        let mut config = Config {
            config_version: 1,
            environment: "lab".into(),
            ..Config::default()
        };
        config.controller.observe = false;
        config.notification = NotificationConfig {
            webhooks: vec![WebhookConfig {
                name: "test".into(),
                url: "http://example.org/hook".into(),
                format: Default::default(),
            }],
            min_severity: min_severity.into(),
            // Tests must not spend a second per notification.
            min_interval: std::time::Duration::ZERO,
        };
        Controller::new(config, SqliteStore::open_in_memory().await.expect("store"))
            .await
            .expect("controller")
    }

    #[tokio::test]
    async fn a_new_incident_is_notified() {
        let mut controller = controller("warning").await;
        let (provider, sent) = recording("test");
        let update = IncidentUpdate {
            opened: vec![incident(Severity::Critical, &["fs1"])],
            ..Default::default()
        };

        let outcome = controller
            .notify(
                &update,
                &[provider],
                &mut Deduplicator::new(),
                &MaintenanceWindows::new(),
            )
            .await
            .expect("notify");

        assert_eq!(outcome.sent, 1);
        assert_eq!(sent.lock().await.len(), 1);
    }

    #[tokio::test]
    async fn an_unchanged_incident_notifies_nobody() {
        let mut controller = controller("warning").await;
        let (provider, sent) = recording("test");
        let update = IncidentUpdate {
            updated: vec![incident(Severity::Critical, &["fs1"])],
            ..Default::default()
        };

        let outcome = controller
            .notify(
                &update,
                &[provider],
                &mut Deduplicator::new(),
                &MaintenanceWindows::new(),
            )
            .await
            .expect("notify");

        assert_eq!(outcome.sent, 0);
        assert!(sent.lock().await.is_empty());
    }

    #[tokio::test]
    async fn an_informational_finding_does_not_reach_a_phone() {
        // A drained node belongs in `sentinel status`.
        let mut controller = controller("warning").await;
        let (provider, _) = recording("test");
        let update = IncidentUpdate {
            opened: vec![incident(Severity::Info, &["fs1"])],
            ..Default::default()
        };

        let outcome = controller
            .notify(
                &update,
                &[provider],
                &mut Deduplicator::new(),
                &MaintenanceWindows::new(),
            )
            .await
            .expect("notify");

        assert_eq!(outcome.sent, 0);
        assert_eq!(outcome.below_threshold, 1);
    }

    #[tokio::test]
    async fn a_recovery_is_sent_even_below_the_severity_floor() {
        // An operator told about a fault is owed the ending, whatever its
        // severity turned out to be.
        let mut controller = controller("critical").await;
        let (provider, sent) = recording("test");

        let mut resolved = incident(Severity::Warning, &["fs1"]);
        resolved.resolve();
        let update = IncidentUpdate {
            resolved: vec![resolved],
            ..Default::default()
        };

        let outcome = controller
            .notify(
                &update,
                &[provider],
                &mut Deduplicator::new(),
                &MaintenanceWindows::new(),
            )
            .await
            .expect("notify");

        assert_eq!(outcome.sent, 1);
        assert!(sent.lock().await[0].trigger.is_recovery());
    }

    #[tokio::test]
    async fn a_repeated_notification_is_deduplicated() {
        let mut controller = controller("warning").await;
        let (provider, sent) = recording("test");
        let update = IncidentUpdate {
            opened: vec![incident(Severity::Critical, &["fs1"])],
            ..Default::default()
        };
        let mut deduplicator = Deduplicator::new();

        controller
            .notify(
                &update,
                std::slice::from_ref(&provider),
                &mut deduplicator,
                &MaintenanceWindows::new(),
            )
            .await
            .expect("first");
        let second = controller
            .notify(&update, &[provider], &mut deduplicator, &MaintenanceWindows::new())
            .await
            .expect("second");

        assert_eq!(second.sent, 0);
        assert_eq!(second.deduplicated, 1);
        assert_eq!(sent.lock().await.len(), 1);
    }

    #[tokio::test]
    async fn maintenance_suppresses_the_notification_only_when_it_covers_everything() {
        let mut controller = controller("warning").await;
        let maintenance = MaintenanceWindows::from_windows([MaintenanceWindow::for_entity(host("fs1"), "work")]);

        // Only fs1 affected, and fs1 is under maintenance: suppressed.
        let (provider, _) = recording("test");
        let covered = IncidentUpdate {
            opened: vec![incident(Severity::Critical, &["fs1"])],
            ..Default::default()
        };
        let outcome = controller
            .notify(&covered, &[provider], &mut Deduplicator::new(), &maintenance)
            .await
            .expect("notify");
        assert_eq!(outcome.suppressed_by_maintenance, 1);

        // Another host is affected too: not suppressed.
        let (provider, _) = recording("test");
        let wider = IncidentUpdate {
            opened: vec![incident(Severity::Critical, &["fs1", "c1"])],
            ..Default::default()
        };
        let outcome = controller
            .notify(&wider, &[provider], &mut Deduplicator::new(), &maintenance)
            .await
            .expect("notify");
        assert_eq!(
            outcome.sent, 1,
            "one machine under maintenance must not silence the rest"
        );
    }

    #[tokio::test]
    async fn a_failed_delivery_is_not_recorded_so_it_is_retried() {
        let mut controller = controller("warning").await;
        let failing = Arc::new(Recording {
            name: "test".into(),
            sent: Arc::new(tokio::sync::Mutex::new(Vec::new())),
            fail: true,
        }) as Arc<dyn NotificationProvider>;

        let update = IncidentUpdate {
            opened: vec![incident(Severity::Critical, &["fs1"])],
            ..Default::default()
        };
        let mut deduplicator = Deduplicator::new();

        let outcome = controller
            .notify(&update, &[failing], &mut deduplicator, &MaintenanceWindows::new())
            .await
            .expect("notify");

        assert_eq!(outcome.failed, 1);
        assert!(deduplicator.is_empty(), "a failed send must not suppress the retry");
    }

    #[tokio::test]
    async fn with_no_providers_nothing_is_attempted() {
        let mut controller = controller("warning").await;
        let update = IncidentUpdate {
            opened: vec![incident(Severity::Critical, &["fs1"])],
            ..Default::default()
        };

        let outcome = controller
            .notify(&update, &[], &mut Deduplicator::new(), &MaintenanceWindows::new())
            .await
            .expect("notify");
        assert!(outcome.is_empty());
    }

    #[test]
    fn a_mistyped_webhook_url_is_refused_without_stopping_the_controller() {
        let mut config = Config::default();
        config.notification.webhooks = vec![
            WebhookConfig {
                name: "bad".into(),
                url: "not-a-url".into(),
                format: Default::default(),
            },
            WebhookConfig {
                name: "good".into(),
                url: "https://example.org/hook".into(),
                format: Default::default(),
            },
        ];

        let providers = providers_from_config(&config);
        assert_eq!(providers.len(), 1, "the usable destination still works");
        assert_eq!(providers[0].name(), "good");
    }

    #[test]
    fn the_severity_floor_falls_back_to_warning_when_misconfigured() {
        let mut config = Config::default();
        config.notification.min_severity = "extremely".into();
        assert_eq!(min_severity(&config), Severity::Warning);
    }
}
