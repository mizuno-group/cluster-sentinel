//! Journal collection against the real journal on this machine.
//!
//! The pattern matching is unit-tested against recorded kernel messages; this
//! checks the part unit tests cannot — that the command actually runs, its real
//! output parses, and the bounds hold.
//!
//! Skips where there is no journal, which includes the Docker pseudo-cluster:
//! containers have no systemd, so `journal.read` is correctly not detected and
//! the probe correctly does not run there. Verifying this against a real
//! systemd host is a level 4 item (docs/VM_VALIDATION.md).

use std::time::Duration;

use sentinel::agent::system::{LinuxInspector, SystemInspector};
use sentinel::capability::CapabilitySet;
use sentinel::entity::{EntityKey, EntityType};
use sentinel::observation::ProbeStatus;
use sentinel::probes::journal::{JournalProbe, MAX_LINES, MAX_LOOKBACK};
use sentinel::probes::{Probe, ProbeContext};

/// Whether this machine has a journal to read.
fn journal_available() -> bool {
    let inspector = LinuxInspector::new();
    inspector.which("journalctl").is_some() && inspector.path_exists(std::path::Path::new("/run/systemd/system"))
}

macro_rules! require_journal {
    () => {
        if !journal_available() {
            eprintln!("skipping: no systemd journal on this machine");
            return;
        }
    };
}

fn context() -> ProbeContext {
    ProbeContext::local(
        EntityKey::new("lab", EntityType::Host, "node-a").entity_id(),
        CapabilitySet::from_iter(["journal.read"]),
    )
    .with_timeout(Duration::from_secs(10))
}

#[tokio::test]
async fn the_probe_reads_the_real_journal() {
    require_journal!();

    let observation = JournalProbe::new().collect(&context()).await;

    assert_ne!(
        observation.status,
        ProbeStatus::Unsupported,
        "journalctl is present, so the probe should have run: {:?}",
        observation.error_message
    );
    assert!(observation.payload["event_count"].is_number());
    assert!(observation.payload["window_seconds"].is_number());
}

#[tokio::test]
async fn a_healthy_machine_yields_no_serious_events() {
    // If this fails on a developer's laptop, the laptop has something to say.
    require_journal!();

    let observation = JournalProbe::new().collect(&context()).await;
    if observation.status == ProbeStatus::Unsupported {
        return;
    }

    let serious = observation.payload["serious_count"].as_u64().unwrap_or(0);
    assert_eq!(
        serious, 0,
        "this machine is logging serious kernel events: {:?}",
        observation.evidence
    );
    assert_eq!(observation.status, ProbeStatus::Ok);
}

#[tokio::test]
async fn the_scan_stays_within_its_bounds() {
    require_journal!();

    let probe = JournalProbe::new();

    // A window far longer than the cap must be clamped, not honoured. The
    // first scan after a long outage would otherwise read the whole journal.
    let events = probe.read_window(Duration::from_secs(86_400 * 30)).await;
    let events = match events {
        Ok(events) => events,
        Err(_) => return,
    };

    assert!(
        events.len() <= MAX_LINES,
        "returned {} events, more than the {MAX_LINES} line bound",
        events.len()
    );
}

#[tokio::test]
async fn the_probe_returns_promptly_even_over_a_long_window() {
    // The bounds exist so that a busy machine cannot make this slow.
    require_journal!();

    let started = std::time::Instant::now();
    let _ = JournalProbe::new().read_window(MAX_LOOKBACK).await;

    assert!(
        started.elapsed() < Duration::from_secs(15),
        "the journal scan took {:?}",
        started.elapsed()
    );
}

#[tokio::test]
async fn collected_events_are_carried_as_evidence_not_as_payload() {
    // The lines are what an operator reads after the machine has rebooted and
    // taken the journal with it, so they belong with the evidence.
    require_journal!();

    let observation = JournalProbe::new().collect(&context()).await;
    if observation.status == ProbeStatus::Unsupported {
        return;
    }

    assert!(
        observation.evidence.get("events").is_some(),
        "events must be attached as evidence: {:?}",
        observation.evidence
    );
}
