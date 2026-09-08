//! Retention: the database must not grow without bound, and pruning must not
//! destroy the evidence the system exists to preserve.
//!
//! The measurement that motivated this: a five-host testbed wrote roughly ten
//! kilobytes a second, about 170 MB per host per day. Nothing deleted any of
//! it. At a few hundred nodes that fills a disk in weeks, and a monitoring
//! system that fills its own disk fails at the moment it is most needed.
//!
//! Deleting is the easy half. These tests are mostly about what must survive.

use std::time::Duration;

use sentinel::config::{Config, RetentionConfig, RetentionPeriod};
use sentinel::diagnosis::{Confidence, Diagnosis};
use sentinel::entity::{DiscoverySource, EntityId, EntityKey, EntityType, ManagedEntity};
use sentinel::incident::{Incident, Severity};
use sentinel::observation::{Observation, ProbeStatus};
use sentinel::persistence::{PruneMode, SqliteStore};
use sentinel::state::{Health, StateComponent, StateTransition};
use sentinel::time::{now, Timestamp};

const PROBE: &str = sentinel::probes::network::PROBE_ID;

fn host(name: &str) -> EntityId {
    EntityKey::new("lab", EntityType::Host, name).entity_id()
}

/// A store with two known hosts.
async fn store() -> SqliteStore {
    let store = SqliteStore::open_in_memory().await.expect("store");
    store.ensure_environment("lab").await.expect("environment");

    for name in ["n1", "n2"] {
        let entity =
            ManagedEntity::new("lab", EntityType::Host, name).with_discovery_source(DiscoverySource::StaticConfig);
        store.save_entity(&entity).await.expect("entity");
    }
    store
}

fn days_ago(days: i64) -> Timestamp {
    now() - chrono::Duration::days(days)
}

/// An observation about `name`, recorded `days` ago.
fn observation(name: &str, days: i64) -> Observation {
    let at = days_ago(days);
    Observation::new(PROBE.into(), host(name), ProbeStatus::Ok).with_times(at, at)
}

/// A retention config that keeps nothing but the mandated floor.
fn keeping(observations: &str) -> RetentionConfig {
    RetentionConfig {
        observations: parse(observations),
        keep_per_entity: 0, // clamped up to the floor by the store
        ..RetentionConfig::default()
    }
}

fn parse(text: &str) -> RetentionPeriod {
    #[derive(serde::Deserialize)]
    struct Wrapper {
        period: RetentionPeriod,
    }
    toml::from_str::<Wrapper>(&format!("period = {text:?}"))
        .expect("period")
        .period
}

async fn observation_count(store: &SqliteStore, name: &str) -> i64 {
    store.observation_count(host(name)).await.expect("count")
}

#[tokio::test]
async fn observations_past_the_window_are_deleted() {
    let store = store().await;
    let old: Vec<Observation> = (0..200).map(|i| observation("n1", 30 + i)).collect();
    store.ingest_observations(&old).await.expect("ingest");
    assert_eq!(observation_count(&store, "n1").await, 200);

    let outcome = store.prune(&keeping("14d")).await.expect("prune");

    assert!(outcome.observations > 0, "nothing was pruned");
    assert!(
        observation_count(&store, "n1").await < 200,
        "the database did not shrink"
    );
}

#[tokio::test]
async fn every_entity_keeps_its_most_recent_observations_however_old() {
    // A host that has been down for a month is the longest outage the system
    // has. Pruning must not be the thing that makes it disappear.
    let store = store().await;
    let ancient: Vec<Observation> = (0..40).map(|i| observation("n1", 365 + i)).collect();
    store.ingest_observations(&ancient).await.expect("ingest");

    store.prune(&keeping("1d")).await.expect("prune");

    let left = observation_count(&store, "n1").await;
    assert_eq!(
        left, 32,
        "the per-entity floor must survive any age; {left} observations left"
    );
}

#[tokio::test]
async fn the_floor_is_per_entity_not_global() {
    // Otherwise a busy host's observations would satisfy the floor on behalf
    // of a quiet one, and the quiet one would be erased.
    let store = store().await;
    let mut all: Vec<Observation> = (0..100).map(|i| observation("n1", 400 + i)).collect();
    all.extend((0..100).map(|i| observation("n2", 400 + i)));
    store.ingest_observations(&all).await.expect("ingest");

    store.prune(&keeping("1d")).await.expect("prune");

    assert_eq!(observation_count(&store, "n1").await, 32);
    assert_eq!(observation_count(&store, "n2").await, 32);
}

#[tokio::test]
async fn an_observation_cited_as_evidence_outlives_the_window() {
    // A diagnosis whose evidence has been deleted is an assertion nobody can
    // check. SPEC.md §116 requires every diagnosis to be traceable to the
    // observations behind it, and that has to keep being true tomorrow.
    let store = store().await;
    let cited = observation("n1", 500);
    let cited_id = cited.id;
    let filler: Vec<Observation> = (0..100).map(|i| observation("n1", 400 + i)).collect();
    store
        .ingest_observations(&[vec![cited], filler].concat())
        .await
        .expect("ingest");

    let mut incident = Incident::open("fingerprint", Severity::Critical);
    incident.started_at = days_ago(1);
    incident.evidence.push(cited_id);
    store.save_incident("lab", &incident).await.expect("incident");

    store.prune(&keeping("1d")).await.expect("prune");

    let survivors = store.recent_observations(host("n1"), 1000).await.expect("observations");
    assert!(
        survivors.iter().any(|o| o.id == cited_id),
        "evidence for a live incident was deleted"
    );
}

#[tokio::test]
async fn an_open_incident_is_never_pruned_at_any_age() {
    // An incident open for a year is a year-old unfixed fault, which is
    // exactly the thing worth keeping.
    let store = store().await;
    let mut open = Incident::open("still-broken", Severity::Critical);
    open.started_at = days_ago(400);
    store.save_incident("lab", &open).await.expect("incident");

    let config = RetentionConfig {
        resolved_incidents: parse("1d"),
        ..RetentionConfig::default()
    };
    store.prune(&config).await.expect("prune");

    let left = store.load_incidents("lab", 100).await.expect("incidents");
    assert_eq!(left.len(), 1, "an open incident was pruned");
}

#[tokio::test]
async fn a_resolved_incident_is_pruned_once_it_is_old_enough() {
    let store = store().await;
    let mut resolved = Incident::open("long-fixed", Severity::Warning);
    resolved.started_at = days_ago(400);
    resolved.resolve();
    resolved.ended_at = Some(days_ago(399));
    store.save_incident("lab", &resolved).await.expect("incident");

    let config = RetentionConfig {
        resolved_incidents: parse("180d"),
        ..RetentionConfig::default()
    };
    let outcome = store.prune(&config).await.expect("prune");

    assert_eq!(outcome.incidents, 1);
    assert!(store.load_incidents("lab", 100).await.expect("incidents").is_empty());
}

#[tokio::test]
async fn a_never_period_deletes_nothing_of_that_class() {
    let store = store().await;
    let old: Vec<Observation> = (0..100).map(|i| observation("n1", 400 + i)).collect();
    store.ingest_observations(&old).await.expect("ingest");

    let config = RetentionConfig {
        observations: RetentionPeriod::Forever,
        ..RetentionConfig::default()
    };
    let outcome = store.prune(&config).await.expect("prune");

    assert_eq!(outcome.observations, 0);
    assert_eq!(observation_count(&store, "n1").await, 100);
}

#[tokio::test]
async fn pruning_can_be_switched_off_entirely() {
    let store = store().await;
    let old: Vec<Observation> = (0..100).map(|i| observation("n1", 400 + i)).collect();
    store.ingest_observations(&old).await.expect("ingest");

    let config = RetentionConfig {
        enabled: false,
        observations: parse("1s"),
        ..RetentionConfig::default()
    };

    assert!(store.prune(&config).await.expect("prune").is_empty());
    assert_eq!(observation_count(&store, "n1").await, 100);
}

#[tokio::test]
async fn a_dry_run_reports_exactly_what_a_real_run_would_delete() {
    // The counts have to come from the real statements, rolled back. A second
    // set of COUNT queries would be free to drift out of agreement with the
    // deletes, and the drift would only show up as a surprise in production.
    let store = store().await;
    let old: Vec<Observation> = (0..100).map(|i| observation("n1", 400 + i)).collect();
    store.ingest_observations(&old).await.expect("ingest");

    let config = keeping("1d");
    let dry = store
        .prune_at(&config, now(), PruneMode::DryRun)
        .await
        .expect("dry run");
    assert_eq!(observation_count(&store, "n1").await, 100, "a dry run deleted rows");

    let real = store.prune(&config).await.expect("prune");
    assert_eq!(dry, real, "the dry run disagreed with the real one");
}

#[tokio::test]
async fn state_transitions_are_pruned_but_keep_a_floor_too() {
    // Without a floor, an entity that stopped changing would lose the record
    // of the last thing that happened to it.
    let store = store().await;
    for i in 0..100 {
        let mut transition = StateTransition::new(
            host("n1"),
            Some(StateComponent::Host),
            Health::Healthy,
            Health::Unavailable,
        );
        transition.at = days_ago(400 + i);
        store.save_state_transition(&transition).await.expect("transition");
    }

    let config = RetentionConfig {
        transitions: parse("1d"),
        keep_per_entity: 0,
        ..RetentionConfig::default()
    };
    store.prune(&config).await.expect("prune");

    let left = store.recent_transitions(host("n1"), 1000).await.expect("transitions");
    assert_eq!(left.len(), 32);
}

#[tokio::test]
async fn a_diagnosis_belonging_to_a_live_incident_is_kept() {
    let store = store().await;
    let mut incident = Incident::open("live", Severity::Critical);
    incident.started_at = days_ago(400);

    let mut diagnosis = Diagnosis::new(
        sentinel::diagnosis::kind::SLURM_ONLY_DEGRADATION,
        "rule",
        Confidence::High,
    );
    diagnosis.created_at = days_ago(400);
    incident.add_diagnosis(diagnosis);
    store.save_incident("lab", &incident).await.expect("incident");

    let config = RetentionConfig {
        diagnoses: parse("1d"),
        ..RetentionConfig::default()
    };
    let outcome = store.prune(&config).await.expect("prune");

    assert_eq!(outcome.diagnoses, 0, "a diagnosis was cut loose from its incident");
}

#[tokio::test]
async fn the_defaults_bound_the_database() {
    // The regression this file exists for: shipping a configuration under
    // which nothing is ever deleted.
    let config = Config::default();
    assert!(config.retention.enabled);
    assert!(!config.retention.observations.is_forever());
    assert!(config.retention.interval <= Duration::from_secs(24 * 3600));
}
