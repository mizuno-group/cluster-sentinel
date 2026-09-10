//! The audit has to catch the bug it was built for.
//!
//! `nfs.server.exports` was defined, capability gated, listed in the catalogue
//! and consulted by a diagnosis rule -- and nothing scheduled it. It ran
//! nowhere, for months, behind a board that read entirely healthy, because a
//! probe that never runs produces no observation, so nothing fails and nothing
//! is diagnosed.
//!
//! This file reconstructs that situation and requires the audit to find it.

use sentinel::audit::{silent_probes, summary};
use sentinel::config::Config;
use sentinel::controller::Controller;
use sentinel::entity::{EntityId, EntityKey, EntityType};
use sentinel::observation::{Observation, ProbeStatus};
use sentinel::persistence::SqliteStore;
use sentinel::probes::catalog;
use sentinel::probes::ProbeId;

const EXPORTS: &str = sentinel::probes::nfs::PROBE_SERVER_EXPORTS;

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
capabilities = ["sentinel.agent", "storage.nfs.server", "storage.nfs.client"]

[[entities]]
type = "host"
name = "fs2"
capabilities = ["sentinel.agent", "storage.nfs.server", "storage.nfs.client"]
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

/// Every probe that applies, reporting now, except the ones named.
fn everything_but(names: &[&str]) -> Vec<Observation> {
    let mut observations = Vec::new();
    for name in ["fs1", "fs2"] {
        for entry in catalog::catalog() {
            if names.contains(&entry.id()) {
                continue;
            }
            observations.push(Observation::new(ProbeId::new(entry.id()), host(name), ProbeStatus::Ok));
        }
    }
    observations
}

#[tokio::test]
async fn a_probe_that_nothing_schedules_is_found() {
    let mut controller = controller().await;
    controller
        .ingest_observations(&everything_but(&[EXPORTS]))
        .await
        .expect("observations");

    let inventory = controller.store().load_inventory("lab").await.expect("inventory");
    let last_seen = controller.store().probe_last_seen("lab").await.expect("last seen");

    let observed = sentinel::audit::observed_entities(&inventory, &config());
    let findings = silent_probes(&inventory, &config(), &observed, &last_seen, sentinel::time::now());

    let exports: Vec<_> = findings.iter().filter(|f| f.probe == EXPORTS).collect();
    assert_eq!(exports.len(), 2, "both fileservers, not just one: {findings:#?}");
    assert!(
        exports.iter().all(|f| f.never_observed()),
        "and it has never run on either"
    );

    let text = summary(&findings).expect("something to say");
    assert!(text.contains("never"), "{text}");
}

#[tokio::test]
async fn a_cluster_where_everything_reports_is_quiet() {
    // The audit has to be silent when it should be, or it gets switched off.
    let mut controller = controller().await;
    controller
        .ingest_observations(&everything_but(&[]))
        .await
        .expect("observations");

    let inventory = controller.store().load_inventory("lab").await.expect("inventory");
    let last_seen = controller.store().probe_last_seen("lab").await.expect("last seen");

    let observed = sentinel::audit::observed_entities(&inventory, &config());
    let findings = silent_probes(&inventory, &config(), &observed, &last_seen, sentinel::time::now());
    assert!(findings.is_empty(), "{findings:#?}");
    assert_eq!(summary(&findings), None);
}

#[tokio::test]
async fn status_says_so_without_being_asked() {
    // The whole failure mode is that nobody thought to look. A command that
    // has to be remembered is not enough on its own.
    let mut controller = controller().await;
    controller
        .ingest_observations(&everything_but(&[EXPORTS]))
        .await
        .expect("observations");

    let report = sentinel::cli::status_cmd::load_report_with(controller.store(), "lab", Some(&config()))
        .await
        .expect("report");

    let line = report.silent_probes.as_deref().expect("status mentions it");
    assert!(line.contains("sentinel audit"), "{line}");

    let rendered = sentinel::cli::status_cmd::render(&report);
    assert!(rendered.contains("sentinel audit"), "{rendered}");
}

#[tokio::test]
async fn a_report_built_without_a_configuration_does_not_guess() {
    // A probe switched off on purpose is not silent, and that is only knowable
    // from the configuration. Without one, the audit says nothing rather than
    // reporting every disabled probe as a fault.
    let mut controller = controller().await;
    controller
        .ingest_observations(&everything_but(&[EXPORTS]))
        .await
        .expect("observations");

    let report = sentinel::cli::status_cmd::load_report(controller.store(), "lab")
        .await
        .expect("report");
    assert_eq!(report.silent_probes, None);
}
