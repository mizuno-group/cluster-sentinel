//! An incident that is opened must be announced.
//!
//! Correlation used to run in two places: the discovery cycle and the
//! diagnosis loop. Only the second notifies. Whichever ran first consumed the
//! opening, so an incident unlucky enough to be opened by a discovery cycle
//! was recorded and never announced -- a missed alert, arriving
//! non-deterministically, which is worse than no alerting because it looks
//! like it works.
//!
//! Seen on a live testbed: the fault was diagnosed, the incident was open in
//! `incident list`, and the webhook never fired.

use sentinel::config::Config;
use sentinel::controller::Controller;
use sentinel::entity::{EntityId, EntityKey, EntityType};
use sentinel::notification::{notifications_for, Deduplicator, Trigger};
use sentinel::observation::{Observation, ProbeStatus};
use sentinel::persistence::SqliteStore;

const SSH: &str = sentinel::probes::ssh::PROBE_ID;
const AGENT: &str = sentinel::probes::sentinel_rpc::PROBE_ID;
const NETWORK: &str = sentinel::probes::network::PROBE_ID;

fn host(name: &str) -> EntityId {
    EntityKey::new("lab", EntityType::Host, name).entity_id()
}

fn observer() -> EntityId {
    EntityKey::new("lab", EntityType::Host, "watcher").entity_id()
}

/// The configuration every controller in this file shares.
fn config() -> Config {
    Config::from_toml(
        r#"
config_version = 1
environment = "lab"

[controller]
observe = false

[[entities]]
type = "host"
name = "node01"
capabilities = ["ssh.server", "sentinel.agent", "network.tcp"]

# The observer has to exist: observations are signed, and an unknown
# signature is not evidence.
[[entities]]
type = "host"
name = "watcher"
"#,
        std::path::Path::new("test.toml"),
    )
    .expect("config")
}

/// A controller that knows one host, with no background loops running.
async fn controller() -> Controller {
    // Declared in the configuration rather than injected, so that a discovery
    // cycle re-declares them. Capabilities are retracted per source, and a
    // static provider with nothing to say retracts what it once said.
    let config = config();

    let store = SqliteStore::open_in_memory().await.expect("store");
    let mut controller = Controller::new(config, store).await.expect("controller");
    controller.discover_once().await.expect("initial discovery");
    controller
}

/// The same controller, but on disk, so it can be restarted.
async fn controller_at(path: &std::path::Path) -> Controller {
    let store = SqliteStore::open(path).await.expect("store");
    let mut controller = Controller::new(config(), store).await.expect("controller");
    controller.discover_once().await.expect("initial discovery");
    controller
}

/// SSH is down while the agent answers: a lockout, not an outage.
async fn report_ssh_failure(controller: &mut Controller) {
    for _ in 0..4 {
        let observations = vec![
            Observation::new(SSH.into(), host("node01"), ProbeStatus::Failed)
                .with_observer(observer())
                .with_error("refused", "nothing is listening on the SSH port"),
            // The host answers and its agent answers: that is what makes this
            // a lockout rather than an outage, and the rule requires it.
            Observation::new(NETWORK.into(), host("node01"), ProbeStatus::Ok).with_observer(observer()),
            Observation::new(AGENT.into(), host("node01"), ProbeStatus::Ok).with_observer(observer()),
        ];
        controller
            .ingest_observations(&observations)
            .await
            .expect("observations");
    }
}

#[tokio::test]
async fn a_discovery_cycle_does_not_open_incidents() {
    // Because it cannot announce them. One place opens incidents, and it is
    // the one that notifies.
    let mut controller = controller().await;
    report_ssh_failure(&mut controller).await;

    controller.discover_once().await.expect("discovery");

    let incidents = controller.store().load_incidents("lab", 100).await.expect("incidents");
    assert!(
        incidents.is_empty(),
        "a discovery cycle opened an incident nothing will announce: {incidents:#?}"
    );
}

#[tokio::test]
async fn the_diagnosis_pass_opens_it_and_produces_a_notification() {
    let mut controller = controller().await;
    report_ssh_failure(&mut controller).await;

    let (_, update) = controller.diagnose_and_correlate().await.expect("correlate");
    assert_eq!(update.opened.len(), 1, "{update:#?}");

    let notifications = notifications_for(&update);
    assert_eq!(notifications.len(), 1);
    assert_eq!(notifications[0].trigger, Trigger::Opened);
}

#[tokio::test]
async fn an_incident_nobody_was_told_about_is_still_announced_after_a_restart() {
    // The second way an alert went missing, and the harder one to see. An
    // incident had exactly one chance to be announced: the pass that opened
    // it. A controller restarted after that pass resumes its open incidents
    // -- correctly, so it does not re-alert on everything an operator is
    // already handling -- and from then on the incident is never "opened"
    // again, so it is never news, so it is never sent. It stays open and
    // silent for as long as the fault lasts.
    //
    // Seen on a live cluster: `incident list` showed one open CRITICAL, and
    // the webhook had never fired for it.
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("sentinel.db");

    {
        let mut controller = controller_at(&path).await;
        report_ssh_failure(&mut controller).await;
        let (_, update) = controller.diagnose_and_correlate().await.expect("correlate");
        assert_eq!(update.opened.len(), 1, "the incident opens");
        // And nobody is told: no destinations were configured yet.
    }

    // Restart.
    let mut controller = controller_at(&path).await;
    report_ssh_failure(&mut controller).await;
    let (_, update) = controller.diagnose_and_correlate().await.expect("correlate");

    assert!(
        update.opened.is_empty(),
        "the incident is resumed, not reopened -- which is exactly why it used to go silent"
    );
    assert_eq!(update.updated.len(), 1, "{update:#?}");

    let notifications = notifications_for(&update);
    let opening = notifications
        .iter()
        .find(|n| n.trigger == Trigger::Opened)
        .expect("the open incident is still offered for announcement");

    // Nothing was ever delivered, so nothing suppresses it.
    let mut deduplicator = Deduplicator::new();
    deduplicator.seed(
        controller
            .store()
            .load_notifications("lab")
            .await
            .expect("notification history"),
    );
    assert!(
        deduplicator.should_send(opening, "ops"),
        "an incident that was never announced must still be announceable"
    );
}

#[tokio::test]
async fn an_incident_already_announced_stays_quiet_across_a_restart() {
    // The other half of the same property, and the reason the history is
    // persisted rather than simply re-announcing every open incident on
    // startup: a controller restart must not re-alert on faults the operator
    // is already dealing with.
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("sentinel.db");

    let opening = {
        let mut controller = controller_at(&path).await;
        report_ssh_failure(&mut controller).await;
        let (_, update) = controller.diagnose_and_correlate().await.expect("correlate");

        let mut deduplicator = Deduplicator::new();
        let notifications = notifications_for(&update);
        let opening = notifications[0].clone();
        let record = deduplicator.record(&opening, "ops");
        controller
            .store()
            .save_notification(&record)
            .await
            .expect("record the delivery");
        opening
    };

    let controller = controller_at(&path).await;
    let mut deduplicator = Deduplicator::new();
    deduplicator.seed(
        controller
            .store()
            .load_notifications("lab")
            .await
            .expect("notification history"),
    );

    assert!(
        !deduplicator.should_send(&opening, "ops"),
        "a restart re-announced an incident the operator had already been told about"
    );
}
