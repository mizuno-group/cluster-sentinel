//! M10 acceptance: alerting that people will still be reading in six months.
//!
//! Every assertion here is about *not* sending something. That is the whole
//! difficulty: a monitoring system that notifies on state rather than on change
//! trains its operators to filter it, and then the one alert that mattered gets
//! filtered too.

use std::sync::Arc;

use async_trait::async_trait;
use sentinel::config::{Config, NotificationConfig, WebhookConfig};
use sentinel::controller::Controller;
use sentinel::dependency::{DependencyEdge, DependencyType};
use sentinel::diagnosis::{kind, Confidence, Diagnosis};
use sentinel::entity::{DiscoverySource, EntityId, EntityKey, EntityType, ManagedEntity};
use sentinel::incident::{Incident, IncidentUpdate, Severity};
use sentinel::inventory::InventorySnapshot;
use sentinel::notification::{
    Deduplicator, MaintenanceWindow, MaintenanceWindows, Notification, NotificationProvider, ProviderError, Trigger,
};
use sentinel::observation::{Observation, ProbeStatus};
use sentinel::persistence::SqliteStore;
use sentinel::probes::nfs::PROBE_SERVER_PORT;
use sentinel::probes::ProbeId;

const NETWORK: &str = sentinel::probes::network::PROBE_ID;
const AGENT: &str = sentinel::probes::sentinel_rpc::PROBE_ID;

fn host(name: &str) -> EntityId {
    EntityKey::new("lab", EntityType::Host, name).entity_id()
}

/// A provider that records everything it is asked to send.
struct Recording {
    sent: Arc<tokio::sync::Mutex<Vec<Notification>>>,
}

#[async_trait]
impl NotificationProvider for Recording {
    fn name(&self) -> &str {
        "recording"
    }

    async fn send(&self, notification: &Notification) -> Result<(), ProviderError> {
        self.sent.lock().await.push(notification.clone());
        Ok(())
    }
}

fn recording() -> (
    Arc<dyn NotificationProvider>,
    Arc<tokio::sync::Mutex<Vec<Notification>>>,
) {
    let sent = Arc::new(tokio::sync::Mutex::new(Vec::new()));
    (
        Arc::new(Recording {
            sent: Arc::clone(&sent),
        }) as Arc<dyn NotificationProvider>,
        sent,
    )
}

/// A controller over a fileserver with two clients.
async fn cluster() -> Controller {
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
        }],
        min_severity: "warning".into(),
    };

    let store = SqliteStore::open_in_memory().await.expect("store");
    let mut controller = Controller::new(config, store).await.expect("controller");

    let mut snapshot = InventorySnapshot::new(DiscoverySource::StaticConfig);
    snapshot.add_entity(
        ManagedEntity::new("lab", EntityType::Host, "fs1")
            .with_capabilities(["storage.nfs.server"].into_iter().collect()),
    );
    snapshot.add_entity(ManagedEntity::new("lab", EntityType::Storage, "storage-a"));
    snapshot.add_dependency(DependencyEdge::new(
        EntityKey::new("lab", EntityType::Storage, "storage-a").entity_id(),
        host("fs1"),
        DependencyType::Provides,
    ));
    for client in ["c1", "c2"] {
        snapshot.add_entity(
            ManagedEntity::new("lab", EntityType::Host, client)
                .with_capabilities(["storage.nfs.client"].into_iter().collect()),
        );
        snapshot.add_dependency(DependencyEdge::new(
            host(client),
            EntityKey::new("lab", EntityType::Storage, "storage-a").entity_id(),
            DependencyType::UsesStorage,
        ));
    }
    controller.ingest_snapshot(&snapshot).await.expect("inventory");
    controller
}

async fn observe(controller: &mut Controller, name: &str, probe: &str, status: ProbeStatus) {
    let mut observations = Vec::new();
    for _ in 0..3 {
        observations.push(Observation::new(ProbeId::new(probe), host(name), status));
    }
    controller
        .ingest_observations(&observations)
        .await
        .expect("observations");
}

async fn make_healthy(controller: &mut Controller, names: &[&str]) {
    for name in names {
        observe(controller, name, NETWORK, ProbeStatus::Ok).await;
        observe(controller, name, AGENT, ProbeStatus::Ok).await;
    }
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

#[tokio::test]
async fn a_real_incident_produces_exactly_one_notification() {
    let mut controller = cluster().await;
    make_healthy(&mut controller, &["fs1", "c1", "c2"]).await;
    observe(&mut controller, "fs1", PROBE_SERVER_PORT, ProbeStatus::Failed).await;

    let (_, update) = controller.diagnose_and_correlate().await.expect("correlate");
    let (provider, sent) = recording();

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
    let sent = sent.lock().await;
    assert_eq!(sent.len(), 1);
    assert_eq!(sent[0].trigger, Trigger::Opened);
    assert!(sent[0].body.contains("export port"), "{}", sent[0].body);
}

#[tokio::test]
async fn an_ongoing_incident_never_notifies_again() {
    // The property that decides whether anyone still reads these in six months.
    let mut controller = cluster().await;
    make_healthy(&mut controller, &["fs1", "c1", "c2"]).await;
    observe(&mut controller, "fs1", PROBE_SERVER_PORT, ProbeStatus::Failed).await;

    let (provider, sent) = recording();
    let mut deduplicator = Deduplicator::new();

    // Ten cycles with the fault present throughout.
    for _ in 0..10 {
        observe(&mut controller, "fs1", PROBE_SERVER_PORT, ProbeStatus::Failed).await;
        let (_, update) = controller.diagnose_and_correlate().await.expect("correlate");
        controller
            .notify(
                &update,
                std::slice::from_ref(&provider),
                &mut deduplicator,
                &MaintenanceWindows::new(),
            )
            .await
            .expect("notify");
    }

    assert_eq!(sent.lock().await.len(), 1, "one fault, one alert");
}

#[tokio::test]
async fn the_resolution_is_always_delivered() {
    // An operator told about a fault is owed the ending. Suppressing this as a
    // duplicate would leave them believing it was ongoing.
    let mut controller = cluster().await;
    make_healthy(&mut controller, &["fs1", "c1", "c2"]).await;
    observe(&mut controller, "fs1", PROBE_SERVER_PORT, ProbeStatus::Failed).await;

    let (provider, sent) = recording();
    let mut deduplicator = Deduplicator::new();

    let (_, opened) = controller.diagnose_and_correlate().await.expect("correlate");
    controller
        .notify(
            &opened,
            std::slice::from_ref(&provider),
            &mut deduplicator,
            &MaintenanceWindows::new(),
        )
        .await
        .expect("notify");

    // The fault clears.
    observe(&mut controller, "fs1", PROBE_SERVER_PORT, ProbeStatus::Ok).await;
    let (_, resolved) = controller.diagnose_and_correlate().await.expect("correlate");
    controller
        .notify(&resolved, &[provider], &mut deduplicator, &MaintenanceWindows::new())
        .await
        .expect("notify");

    let sent = sent.lock().await;
    let triggers: Vec<_> = sent.iter().map(|n| n.trigger).collect();
    assert!(triggers.contains(&Trigger::Opened));
    assert!(
        triggers.iter().any(|t| t.is_recovery()),
        "the ending must be delivered: {triggers:?}"
    );
}

#[tokio::test]
async fn an_informational_finding_stays_out_of_the_alert_stream() {
    // A drained node belongs in `sentinel status`.
    let mut controller = cluster().await;
    let (provider, sent) = recording();
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

    assert_eq!(outcome.below_threshold, 1);
    assert!(sent.lock().await.is_empty());
}

#[tokio::test]
async fn planned_work_does_not_wake_the_person_who_planned_it() {
    let mut controller = cluster().await;
    let (provider, sent) = recording();
    let maintenance =
        MaintenanceWindows::from_windows([
            MaintenanceWindow::for_entity(host("fs1"), "disk replacement").by("operator")
        ]);

    let update = IncidentUpdate {
        opened: vec![incident(Severity::Critical, &["fs1"])],
        ..Default::default()
    };
    let outcome = controller
        .notify(&update, &[provider], &mut Deduplicator::new(), &maintenance)
        .await
        .expect("notify");

    assert_eq!(outcome.suppressed_by_maintenance, 1);
    assert!(sent.lock().await.is_empty());
}

#[tokio::test]
async fn maintenance_on_one_host_does_not_silence_a_wider_incident() {
    // One machine being worked on must not hide a fault affecting four others.
    let mut controller = cluster().await;
    let (provider, sent) = recording();
    let maintenance =
        MaintenanceWindows::from_windows([MaintenanceWindow::for_entity(host("fs1"), "disk replacement")]);

    let update = IncidentUpdate {
        opened: vec![incident(Severity::Critical, &["fs1", "c1", "c2"])],
        ..Default::default()
    };
    let outcome = controller
        .notify(&update, &[provider], &mut Deduplicator::new(), &maintenance)
        .await
        .expect("notify");

    assert_eq!(outcome.sent, 1);
    assert_eq!(sent.lock().await.len(), 1);
}

#[tokio::test]
async fn maintenance_does_not_alter_what_sentinel_knows() {
    // IMPLEMENTATION.md §76. A window that made things *look* fine would hide
    // a genuine fault that began during it, and leave nobody able to
    // reconstruct when it started.
    let mut controller = cluster().await;
    make_healthy(&mut controller, &["fs1", "c1", "c2"]).await;
    observe(&mut controller, "fs1", PROBE_SERVER_PORT, ProbeStatus::Failed).await;

    let (_, update) = controller.diagnose_and_correlate().await.expect("correlate");
    let maintenance = MaintenanceWindows::from_windows([MaintenanceWindow::for_entity(host("fs1"), "work")]);
    let (provider, _) = recording();

    controller
        .notify(&update, &[provider], &mut Deduplicator::new(), &maintenance)
        .await
        .expect("notify");

    // The incident was still opened, the diagnosis still made, the state still
    // derived. Only the message was withheld.
    assert_eq!(controller.incident_engine().active().count(), 1);
    let diagnoses = controller.diagnose().await.expect("diagnose");
    assert!(diagnoses.iter().any(|d| d.is(kind::NFS_SERVICE_FAILURE)));

    let state = controller.engine().state(host("fs1")).expect("state");
    assert!(
        state.overall.is_problem(),
        "maintenance must not rewrite health: {:?}",
        state.overall
    );
}

#[tokio::test]
async fn a_transient_delivery_failure_is_retried_not_dropped() {
    struct Flaky {
        attempts: std::sync::atomic::AtomicU32,
        sent: Arc<tokio::sync::Mutex<Vec<Notification>>>,
    }

    #[async_trait]
    impl NotificationProvider for Flaky {
        fn name(&self) -> &str {
            "flaky"
        }

        async fn send(&self, notification: &Notification) -> Result<(), ProviderError> {
            if self.attempts.fetch_add(1, std::sync::atomic::Ordering::SeqCst) == 0 {
                return Err(ProviderError::Unreachable {
                    destination: "flaky".into(),
                    detail: "first attempt fails".into(),
                });
            }
            self.sent.lock().await.push(notification.clone());
            Ok(())
        }
    }

    let mut controller = cluster().await;
    let sent = Arc::new(tokio::sync::Mutex::new(Vec::new()));
    let provider = Arc::new(Flaky {
        attempts: std::sync::atomic::AtomicU32::new(0),
        sent: Arc::clone(&sent),
    }) as Arc<dyn NotificationProvider>;

    let update = IncidentUpdate {
        opened: vec![incident(Severity::Critical, &["fs1"])],
        ..Default::default()
    };
    let mut deduplicator = Deduplicator::new();

    let first = controller
        .notify(
            &update,
            std::slice::from_ref(&provider),
            &mut deduplicator,
            &MaintenanceWindows::new(),
        )
        .await
        .expect("first");
    assert_eq!(first.failed, 1);
    assert!(sent.lock().await.is_empty());

    let second = controller
        .notify(&update, &[provider], &mut deduplicator, &MaintenanceWindows::new())
        .await
        .expect("second");
    assert_eq!(
        second.sent, 1,
        "the retry succeeds because the failure was not recorded"
    );
    assert_eq!(sent.lock().await.len(), 1);
}

#[tokio::test]
async fn two_notifiers_watching_one_incident_do_not_double_alert() {
    // SPEC.md §111: the controller and a fallback notifier both noticing one
    // incident must not wake the operator twice.
    let mut controller = cluster().await;
    let (provider, sent) = recording();
    let update = IncidentUpdate {
        opened: vec![incident(Severity::Critical, &["fs1"])],
        ..Default::default()
    };

    // A deduplicator shared between the two notifiers, as a shared store is.
    let mut shared = Deduplicator::new();
    controller
        .notify(
            &update,
            std::slice::from_ref(&provider),
            &mut shared,
            &MaintenanceWindows::new(),
        )
        .await
        .expect("controller");
    let fallback = controller
        .notify(&update, &[provider], &mut shared, &MaintenanceWindows::new())
        .await
        .expect("fallback");

    assert_eq!(fallback.deduplicated, 1);
    assert_eq!(sent.lock().await.len(), 1);
}

#[tokio::test]
async fn a_notification_carries_enough_to_act_on_without_logging_in() {
    let mut controller = cluster().await;
    make_healthy(&mut controller, &["fs1", "c1", "c2"]).await;
    observe(&mut controller, "fs1", PROBE_SERVER_PORT, ProbeStatus::Failed).await;

    let (_, update) = controller.diagnose_and_correlate().await.expect("correlate");
    let (provider, sent) = recording();
    controller
        .notify(
            &update,
            &[provider],
            &mut Deduplicator::new(),
            &MaintenanceWindows::new(),
        )
        .await
        .expect("notify");

    let sent = sent.lock().await;
    let notification = &sent[0];

    assert!(!notification.title.is_empty());
    assert!(notification.body.contains("Incident:"), "an id to look up");
    assert!(
        notification.body.contains("Evidence:"),
        "and how much evidence there is"
    );
    assert!(
        !notification.recommended_actions.is_empty(),
        "and where to start looking"
    );

    // Read-only, always (SPEC.md §113).
    for action in &notification.recommended_actions {
        for mutating in ["restart", "reboot", "mount ", "scontrol update"] {
            assert!(!action.contains(mutating), "recommended a mutating command: {action}");
        }
    }
}
