//! Planned work must not page anyone.
//!
//! The window type, its rules and its table were all present from the start,
//! and the notification path consulted a `MaintenanceWindows` on every pass --
//! one that production constructed empty every time. Nothing could write a
//! window and nothing ever loaded one, so the suppression branch was
//! unreachable in the only build that mattered.
//!
//! The visible consequence: swapping a fileserver paged whoever was on the
//! webhook, repeatedly, for as long as the work took. The operator's only
//! recourse was stopping the controller, which throws away exactly the
//! history a post-mortem needs.
//!
//! These tests exercise the whole path an operator uses -- write to the store,
//! read it back, suppress -- rather than the in-memory type on its own, which
//! was already tested and already passing while the feature did not exist.

use std::sync::Arc;

use sentinel::config::Config;
use sentinel::controller::Controller;
use sentinel::entity::{EntityId, EntityKey, EntityType};
use sentinel::notification::{
    Deduplicator, MaintenanceWindow, MaintenanceWindows, Notification, NotificationProvider, ProviderError,
};
use sentinel::observation::{Observation, ProbeStatus};
use sentinel::persistence::SqliteStore;

const SSH: &str = sentinel::probes::ssh::PROBE_ID;
const AGENT: &str = sentinel::probes::sentinel_rpc::PROBE_ID;
const NETWORK: &str = sentinel::probes::network::PROBE_ID;

fn host(name: &str) -> EntityId {
    EntityKey::new("lab", EntityType::Host, name).entity_id()
}

fn config() -> Config {
    Config::from_toml(
        r#"
config_version = 1
environment = "lab"

[controller]
observe = false

[[entities]]
type = "host"
name = "fs1"
capabilities = ["ssh.server", "sentinel.agent", "network.tcp"]

[[entities]]
type = "host"
name = "fs2"
capabilities = ["ssh.server", "sentinel.agent", "network.tcp"]

[[entities]]
type = "host"
name = "watcher"
"#,
        std::path::Path::new("test.toml"),
    )
    .expect("config")
}

async fn controller() -> Controller {
    let store = SqliteStore::open_in_memory().await.expect("store");
    let mut controller = Controller::new(config(), store).await.expect("controller");
    controller.discover_once().await.expect("discovery");
    controller
}

/// A provider that records what it was asked to send.
struct Recording(Arc<tokio::sync::Mutex<Vec<Notification>>>);

#[async_trait::async_trait]
impl NotificationProvider for Recording {
    fn name(&self) -> &str {
        "recording"
    }

    async fn send(&self, notification: &Notification) -> Result<(), ProviderError> {
        self.0.lock().await.push(notification.clone());
        Ok(())
    }
}

fn recording() -> (
    Arc<dyn NotificationProvider>,
    Arc<tokio::sync::Mutex<Vec<Notification>>>,
) {
    let sent = Arc::new(tokio::sync::Mutex::new(Vec::new()));
    (Arc::new(Recording(sent.clone())), sent)
}

/// SSH down while the agent answers: a lockout on one named host.
async fn report_ssh_failure(controller: &mut Controller, name: &str) {
    for _ in 0..4 {
        let observations = vec![
            Observation::new(SSH.into(), host(name), ProbeStatus::Failed)
                .with_observer(host("watcher"))
                .with_error("refused", "nothing is listening on the SSH port"),
            Observation::new(NETWORK.into(), host(name), ProbeStatus::Ok).with_observer(host("watcher")),
            Observation::new(AGENT.into(), host(name), ProbeStatus::Ok).with_observer(host("watcher")),
        ];
        controller
            .ingest_observations(&observations)
            .await
            .expect("observations");
    }
}

/// What the diagnosis loop does at the top of every pass.
async fn windows_as_the_loop_sees_them(controller: &Controller) -> MaintenanceWindows {
    MaintenanceWindows::from_windows(
        controller
            .store()
            .load_maintenance_windows("lab")
            .await
            .expect("load windows"),
    )
}

#[tokio::test]
async fn a_declared_window_reaches_the_notification_path() {
    // The whole point. Before this, the store had no method to write one and
    // the loop had no code to read one, so the entire feature was decorative.
    let mut controller = controller().await;
    controller
        .store()
        .save_maintenance_window("lab", &MaintenanceWindow::for_entity(host("fs1"), "disk swap"))
        .await
        .expect("save");

    report_ssh_failure(&mut controller, "fs1").await;
    let (_, update) = controller.diagnose_and_correlate().await.expect("correlate");
    assert_eq!(update.opened.len(), 1, "the fault is still diagnosed: {update:#?}");

    let (provider, sent) = recording();
    let maintenance = windows_as_the_loop_sees_them(&controller).await;
    let outcome = controller
        .notify(&update, &[provider], &mut Deduplicator::new(), &maintenance)
        .await
        .expect("notify");

    assert_eq!(outcome.sent, 0, "planned work paged someone");
    assert!(sent.lock().await.is_empty());
}

#[tokio::test]
async fn the_incident_is_still_opened_and_still_visible() {
    // Suppression is of notification only. A window that made the cluster look
    // healthy would hide a fault that began during it and leave nobody able to
    // say when it started.
    let mut controller = controller().await;
    controller
        .store()
        .save_maintenance_window("lab", &MaintenanceWindow::for_entity(host("fs1"), "disk swap"))
        .await
        .expect("save");

    report_ssh_failure(&mut controller, "fs1").await;
    controller.diagnose_and_correlate().await.expect("correlate");

    let open = controller
        .store()
        .load_active_incidents("lab")
        .await
        .expect("incidents");
    assert_eq!(open.len(), 1, "the incident must still exist: {open:#?}");
}

#[tokio::test]
async fn a_window_on_one_host_does_not_silence_another() {
    let mut controller = controller().await;
    controller
        .store()
        .save_maintenance_window("lab", &MaintenanceWindow::for_entity(host("fs1"), "disk swap"))
        .await
        .expect("save");

    report_ssh_failure(&mut controller, "fs2").await;
    let (_, update) = controller.diagnose_and_correlate().await.expect("correlate");

    let (provider, sent) = recording();
    let maintenance = windows_as_the_loop_sees_them(&controller).await;
    controller
        .notify(&update, &[provider], &mut Deduplicator::new(), &maintenance)
        .await
        .expect("notify");

    assert_eq!(sent.lock().await.len(), 1, "fs2 is not under maintenance");
}

#[tokio::test]
async fn ending_the_window_lets_the_fault_through() {
    // The state an operator is in when the swap finishes and the machine is
    // still broken: nothing was announced during the work, and the moment the
    // window closes they must hear about it.
    let mut controller = controller().await;
    let window = MaintenanceWindow::for_entity(host("fs1"), "disk swap");
    controller
        .store()
        .save_maintenance_window("lab", &window)
        .await
        .expect("save");

    report_ssh_failure(&mut controller, "fs1").await;
    let (_, update) = controller.diagnose_and_correlate().await.expect("correlate");
    let mut deduplicator = Deduplicator::new();

    let (provider, sent) = recording();
    let maintenance = windows_as_the_loop_sees_them(&controller).await;
    controller
        .notify(
            &update,
            std::slice::from_ref(&provider),
            &mut deduplicator,
            &maintenance,
        )
        .await
        .expect("notify");
    assert!(sent.lock().await.is_empty(), "suppressed during the window");

    controller
        .store()
        .end_maintenance_window(window.id, sentinel::time::now())
        .await
        .expect("end");

    // The next pass offers the still-open incident again, exactly as the loop
    // does, and now nothing suppresses it.
    let (_, update) = controller.diagnose_and_correlate().await.expect("correlate");
    let maintenance = windows_as_the_loop_sees_them(&controller).await;
    controller
        .notify(&update, &[provider], &mut deduplicator, &maintenance)
        .await
        .expect("notify");

    assert_eq!(
        sent.lock().await.len(),
        1,
        "a fault that outlived the maintenance was never announced"
    );
}

#[tokio::test]
async fn an_expired_window_suppresses_nothing() {
    // The failure mode of open-ended windows: one declared for an afternoon's
    // work silencing a real outage a week later. A window with an end time
    // stops mattering on its own.
    let mut controller = controller().await;
    let past = sentinel::time::now() - chrono::Duration::hours(2);
    controller
        .store()
        .save_maintenance_window(
            "lab",
            &MaintenanceWindow::for_entity(host("fs1"), "yesterday's work")
                .from(past)
                .until(past + chrono::Duration::hours(1)),
        )
        .await
        .expect("save");

    report_ssh_failure(&mut controller, "fs1").await;
    let (_, update) = controller.diagnose_and_correlate().await.expect("correlate");

    let (provider, sent) = recording();
    let maintenance = windows_as_the_loop_sees_them(&controller).await;
    controller
        .notify(&update, &[provider], &mut Deduplicator::new(), &maintenance)
        .await
        .expect("notify");

    assert_eq!(sent.lock().await.len(), 1, "an expired window still silenced a fault");
}

#[tokio::test]
async fn an_environment_wide_window_covers_everything() {
    // What a whole-cluster power cut or a network change needs.
    let mut controller = controller().await;
    controller
        .store()
        .save_maintenance_window("lab", &MaintenanceWindow::for_environment("scheduled power work"))
        .await
        .expect("save");

    report_ssh_failure(&mut controller, "fs1").await;
    report_ssh_failure(&mut controller, "fs2").await;
    let (_, update) = controller.diagnose_and_correlate().await.expect("correlate");
    assert!(!update.opened.is_empty());

    let (provider, sent) = recording();
    let maintenance = windows_as_the_loop_sees_them(&controller).await;
    controller
        .notify(&update, &[provider], &mut Deduplicator::new(), &maintenance)
        .await
        .expect("notify");

    assert!(sent.lock().await.is_empty(), "a cluster-wide window let a page through");
}
