//! Is everything that should be running actually running?
//!
//! Two commands already answer half of this each. `sentinel explain probes`
//! says what *would* run, from the catalogue and the capabilities. `sentinel
//! entity observations` says what *did* run, from the database. Nothing joined
//! them, and the gap between the two is invisible by construction: a probe
//! that never runs produces no observation, no failure, and no diagnosis. Every
//! entity reads healthy, because nothing ever said otherwise.
//!
//! That is not hypothetical. `nfs.server.exports` was defined, capability
//! gated, listed in the catalogue and consulted by a diagnosis rule, and
//! nothing scheduled it. It never ran anywhere, for months, behind a green
//! board. The rule's second branch was unreachable in production and the
//! storage domains it should have judged were decided on a port check alone.
//!
//! **A silent probe is itself a fault.** This module reports them.
//!
//! Two categories, because they mean different things:
//!
//! * **Never observed** — the probe applies to this entity and has produced
//!   nothing, ever. Either it is not wired up at all, or it has never been
//!   able to run here. This is the one that hides for months.
//! * **Silent** — it used to report and has stopped. Something changed: the
//!   agent died, a capability was withdrawn, a schedule was disabled.

use std::collections::{BTreeMap, BTreeSet};
use std::time::Duration;

use crate::config::Config;
use crate::controller::endpoint_for;
use crate::entity::{EntityId, ManagedEntity};
use crate::inventory::Inventory;
use crate::probes::catalog;
use crate::probes::ExecutionMode;
use crate::time::Timestamp;

/// Capability that means an agent is running on the entity.
const AGENT: &str = crate::capability::well_known::SENTINEL_AGENT;

/// How many of its own intervals a probe may miss before it is called silent.
///
/// Deliberately far more generous than the freshness bound diagnosis uses.
/// That one decides whether an observation is still evidence about now; this
/// one asks whether the probe is running at all, and a check that cries wolf
/// is a check people switch off.
const SILENT_AFTER_INTERVALS: u32 = 10;

/// No probe is called silent before this, whatever its interval.
///
/// Reachability runs every five seconds, so ten intervals is under a minute --
/// short enough that an ordinary controller restart would report half the
/// cluster.
const MINIMUM_SILENCE: Duration = Duration::from_secs(300);

/// Why a probe is expected to be producing observations for an entity.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Expectation {
    /// An agent on the entity runs it.
    Locally,
    /// Another host runs it against this entity.
    Remotely,
}

impl Expectation {
    /// Stable string form.
    pub fn as_str(&self) -> &'static str {
        match self {
            Expectation::Locally => "locally",
            Expectation::Remotely => "remotely",
        }
    }
}

/// A probe that should be reporting about an entity and is not.
#[derive(Debug, Clone, PartialEq)]
pub struct Finding {
    /// The probe.
    pub probe: String,
    /// The entity it should be reporting about.
    pub entity: EntityId,
    /// That entity's name, for reading.
    pub entity_name: String,
    /// Where it would run.
    pub expectation: Expectation,
    /// How often it is supposed to run.
    pub interval: Duration,
    /// When it last reported, if it ever has.
    pub last_seen: Option<Timestamp>,
}

impl Finding {
    /// Whether this probe has never reported at all.
    ///
    /// Worth separating: "stopped" is an incident on one host, "never" is a
    /// probe that is not wired up anywhere and has been lying dormant.
    pub fn never_observed(&self) -> bool {
        self.last_seen.is_none()
    }
}

/// Everything that should be reporting and is not.
///
/// Pure: the caller supplies the inventory, the configuration, which entities
/// have an observer assigned, and the last-seen times, so this can be tested
/// without a cluster or a clock.
///
/// `observed` is what makes remote probes answerable. A host nobody watches
/// cannot have its reachability checked, and saying "this probe is silent"
/// about it would name the wrong problem -- `explain paths` reports the
/// missing observer, which is the thing to fix.
pub fn silent_probes(
    inventory: &Inventory,
    config: &Config,
    observed: &BTreeSet<EntityId>,
    last_seen: &BTreeMap<(EntityId, String), Timestamp>,
    now: Timestamp,
) -> Vec<Finding> {
    let mut findings = Vec::new();

    for entity in inventory.entities() {
        if entity.lifecycle_state != crate::entity::LifecycleState::Active {
            continue;
        }

        for entry in catalog::catalog() {
            let mut definition = entry.definition.clone();
            config.probes.apply(&mut definition);
            let probe = definition.id.as_str().to_string();

            // Switched off on purpose is not silence.
            if !config.probes.is_enabled(&probe) {
                continue;
            }
            // `reports_on_type` rather than the definition's targeting: the
            // question is what an observation from this probe would name, not
            // what the probe could in principle measure. `systemd.unit` can
            // target a host and is scheduled per service, and auditing it
            // against hosts reported every host in the cluster.
            if !entry.reports_on_type(entity.entity_type) {
                continue;
            }
            if !definition.applies_to(entity.entity_type, &entity.capabilities) {
                continue;
            }
            let Some(expectation) = expectation_for(definition.execution_mode, entity, observed.contains(&entity.id))
            else {
                continue;
            };

            let seen = last_seen.get(&(entity.id, probe.clone())).copied();
            let horizon = silence_horizon(definition.interval);
            let overdue = match seen {
                None => true,
                Some(at) => {
                    now.signed_duration_since(at)
                        > chrono::Duration::from_std(horizon).unwrap_or_else(|_| chrono::Duration::hours(1))
                }
            };

            if overdue {
                findings.push(Finding {
                    probe,
                    entity: entity.id,
                    entity_name: entity.canonical_name.clone(),
                    expectation,
                    interval: definition.interval,
                    last_seen: seen,
                });
            }
        }
    }

    // Grouped by probe when rendered, so sort that way: a probe missing
    // everywhere is a different discovery from one host having gone quiet, and
    // the sort is what makes the first one obvious.
    findings.sort_by(|a, b| a.probe.cmp(&b.probe).then(a.entity_name.cmp(&b.entity_name)));
    findings
}

/// Whether anything could run this probe against this entity at all.
///
/// Returning `None` is what keeps the report honest. A local probe on a host
/// with no agent is not silent -- there is nothing there to run it, which is a
/// different problem and one that `explain paths` already states. Reporting it
/// here would bury the real findings under every host that has no agent, and a
/// report that is mostly noise is a report nobody reads.
///
/// `Either` counts as remote, not local. Those probes are run **at** the host
/// from elsewhere, which is a different claim from the host checking itself:
/// an agent asking whether it answers can only say yes. `explain paths`
/// classifies them the same way, and this has to agree with it or the two
/// commands describe different clusters.
fn expectation_for(mode: ExecutionMode, entity: &ManagedEntity, watched: bool) -> Option<Expectation> {
    match mode {
        ExecutionMode::Local => entity.capabilities.has(AGENT).then_some(Expectation::Locally),
        ExecutionMode::Remote | ExecutionMode::Either => {
            (watched && endpoint_for(entity).is_some()).then_some(Expectation::Remotely)
        }
    }
}

/// Which entities something is actually watching.
///
/// Peers that have been assigned a target, plus every host when the controller
/// observes: the controller is an observer in its own right, so a cluster with
/// no peer observers at all is still watched from one place.
///
/// A host nobody watches is not a host with silent probes -- it is a host with
/// no observer, which `explain paths` reports as such. Naming it here would
/// point at the wrong thing to fix.
pub fn observed_entities(inventory: &Inventory, config: &Config) -> BTreeSet<EntityId> {
    use crate::entity::{EntityType, LifecycleState};

    let hosts: Vec<EntityId> = inventory
        .entities()
        .filter(|e| e.entity_type == EntityType::Host && e.lifecycle_state == LifecycleState::Active)
        .map(|e| e.id)
        .collect();

    if config.controller.observe {
        return hosts.into_iter().collect();
    }

    let candidates = crate::controller::observer_candidates_for_test(inventory.entities());
    let plan = crate::controller::assign(&hosts, &candidates, inventory.graph(), config.peer_monitoring.degree, 0);

    plan.assignments
        .iter()
        .filter(|assignment| !assignment.observers.is_empty())
        .map(|assignment| assignment.target)
        .collect()
}

/// How long a probe may be quiet before it is reported.
pub fn silence_horizon(interval: Duration) -> Duration {
    (interval * SILENT_AFTER_INTERVALS).max(MINIMUM_SILENCE)
}

/// A one-line summary, or `None` when nothing is wrong.
///
/// Used by `status`, because a check nobody runs is the same as no check --
/// and nobody thought to look for the probe that had never run.
pub fn summary(findings: &[Finding]) -> Option<String> {
    if findings.is_empty() {
        return None;
    }

    let never: BTreeSet<&str> = findings
        .iter()
        .filter(|f| f.never_observed())
        .map(|f| f.probe.as_str())
        .collect();
    let probes: BTreeSet<&str> = findings.iter().map(|f| f.probe.as_str()).collect();

    let detail = if never.is_empty() {
        format!("{} probe(s) have gone quiet", probes.len())
    } else if never.len() == probes.len() {
        format!("{} probe(s) have never reported at all", never.len())
    } else {
        format!(
            "{} probe(s) are not reporting, {} of them never have",
            probes.len(),
            never.len()
        )
    };

    Some(format!("{detail}; run `sentinel audit` for which"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::capability::CapabilitySet;
    use crate::entity::{DiscoverySource, EntityType};
    use crate::inventory::InventorySnapshot;
    use crate::probes::nfs::{PROBE_CLIENT_MOUNT, PROBE_SERVER_EXPORTS};

    const ENV: &str = "lab";

    fn inventory_with(entities: Vec<ManagedEntity>) -> Inventory {
        let mut inventory = Inventory::new();
        let mut snapshot = InventorySnapshot::new(DiscoverySource::StaticConfig);
        for entity in entities {
            snapshot.add_entity(entity);
        }
        inventory.merge(&snapshot);
        inventory
    }

    /// A fileserver with an agent: exports and mounts both apply.
    fn fileserver(name: &str) -> ManagedEntity {
        ManagedEntity::new(ENV, EntityType::Host, name).with_capabilities(
            ["sentinel.agent", "storage.nfs.server", "storage.nfs.client"]
                .into_iter()
                .collect::<CapabilitySet>(),
        )
    }

    fn config() -> Config {
        Config {
            config_version: 1,
            environment: ENV.to_string(),
            ..Config::default()
        }
    }

    fn now() -> Timestamp {
        crate::time::now()
    }

    fn findings_for(inventory: &Inventory, seen: &BTreeMap<(EntityId, String), Timestamp>) -> Vec<Finding> {
        let config = config();
        let observed = observed_entities(inventory, &config);
        silent_probes(inventory, &config, &observed, seen, now())
    }

    #[test]
    fn a_probe_that_never_ran_anywhere_is_reported_for_every_host_it_applies_to() {
        // The case this module exists for. `nfs.server.exports` applied to
        // five hosts and nothing scheduled it, so it produced nothing, so
        // nothing failed, so nothing was diagnosed, and every host read
        // healthy for months.
        let inventory = inventory_with(vec![fileserver("fs1"), fileserver("fs2")]);
        let findings = findings_for(&inventory, &BTreeMap::new());

        let exports: Vec<&Finding> = findings.iter().filter(|f| f.probe == PROBE_SERVER_EXPORTS).collect();
        assert_eq!(exports.len(), 2, "{findings:#?}");
        assert!(exports.iter().all(|f| f.never_observed()));
        assert!(exports.iter().all(|f| f.expectation == Expectation::Locally));
    }

    #[test]
    fn a_probe_reporting_normally_is_not_reported() {
        let inventory = inventory_with(vec![fileserver("fs1")]);
        let fs1 = inventory.entities().next().expect("fs1").id;

        let mut seen = BTreeMap::new();
        for entry in catalog::catalog() {
            seen.insert((fs1, entry.id().to_string()), now());
        }

        assert!(findings_for(&inventory, &seen).is_empty());
    }

    #[test]
    fn a_probe_that_has_stopped_is_reported_but_not_as_never() {
        let inventory = inventory_with(vec![fileserver("fs1")]);
        let fs1 = inventory.entities().next().expect("fs1").id;

        let mut seen = BTreeMap::new();
        for entry in catalog::catalog() {
            seen.insert((fs1, entry.id().to_string()), now());
        }
        // One of them stopped an hour ago.
        seen.insert(
            (fs1, PROBE_CLIENT_MOUNT.to_string()),
            now() - chrono::Duration::hours(1),
        );

        let findings = findings_for(&inventory, &seen);
        assert_eq!(findings.len(), 1, "{findings:#?}");
        assert_eq!(findings[0].probe, PROBE_CLIENT_MOUNT);
        assert!(!findings[0].never_observed(), "it used to report; that is different");
    }

    #[test]
    fn a_recent_gap_is_not_silence() {
        // A probe is not called silent for missing a round. This check has to
        // be quiet enough that people leave it on.
        let inventory = inventory_with(vec![fileserver("fs1")]);
        let fs1 = inventory.entities().next().expect("fs1").id;

        let mut seen = BTreeMap::new();
        for entry in catalog::catalog() {
            seen.insert((fs1, entry.id().to_string()), now() - chrono::Duration::seconds(60));
        }

        assert!(
            findings_for(&inventory, &seen).is_empty(),
            "a minute is not silence for any probe"
        );
    }

    #[test]
    fn a_local_probe_on_a_host_with_no_agent_is_not_called_silent() {
        // Nothing there can run it. That is a different problem, and one
        // `explain paths` already states; reporting it here would bury the
        // real findings under every agentless host.
        let declared = ManagedEntity::new(ENV, EntityType::Host, "fs-no-agent")
            .with_capabilities(["storage.nfs.server"].into_iter().collect::<CapabilitySet>());
        let inventory = inventory_with(vec![declared]);

        let findings = findings_for(&inventory, &BTreeMap::new());
        assert!(
            !findings.iter().any(|f| f.probe == PROBE_SERVER_EXPORTS),
            "{findings:#?}"
        );
    }

    #[test]
    fn a_probe_scheduled_per_service_is_not_audited_against_hosts() {
        // `systemd.unit` can target a host and is scheduled once per service,
        // with each observation naming that service. Auditing it against hosts
        // reported all sixteen hosts of a real cluster as having a silent
        // probe -- and a report that is mostly wrong is a report nobody reads.
        let with_systemd = ManagedEntity::new(ENV, EntityType::Host, "node01")
            .with_capabilities(["sentinel.agent", "systemd"].into_iter().collect::<CapabilitySet>());
        let inventory = inventory_with(vec![with_systemd]);

        let findings = findings_for(&inventory, &BTreeMap::new());
        assert!(
            !findings.iter().any(|f| f.probe == crate::probes::systemd::PROBE_ID),
            "{findings:#?}"
        );
    }

    #[test]
    fn a_probe_switched_off_on_purpose_is_not_silence() {
        let inventory = inventory_with(vec![fileserver("fs1")]);
        let mut config = config();
        config.probes =
            toml::from_str(&format!("[\"{PROBE_SERVER_EXPORTS}\"]\nenabled = false\n")).expect("probe override");

        let observed = observed_entities(&inventory, &config);
        let findings = silent_probes(&inventory, &config, &observed, &BTreeMap::new(), now());
        assert!(
            !findings.iter().any(|f| f.probe == PROBE_SERVER_EXPORTS),
            "{findings:#?}"
        );
    }

    #[test]
    fn a_longer_configured_interval_widens_the_horizon() {
        // The horizon is the probe's own cadence, so retuning a probe must not
        // make it look silent.
        assert_eq!(silence_horizon(Duration::from_secs(5)), MINIMUM_SILENCE);
        assert_eq!(silence_horizon(Duration::from_secs(600)), Duration::from_secs(6000));
    }

    #[test]
    fn a_stale_entity_is_not_audited() {
        // A host that left the cluster should not be reported as unmonitored
        // for the rest of time.
        let mut entity = fileserver("gone");
        entity.lifecycle_state = crate::entity::LifecycleState::Stale;
        let inventory = inventory_with(vec![entity]);

        assert!(findings_for(&inventory, &BTreeMap::new()).is_empty());
    }

    #[test]
    fn the_summary_distinguishes_never_from_stopped() {
        let inventory = inventory_with(vec![fileserver("fs1")]);
        let never = findings_for(&inventory, &BTreeMap::new());
        let text = summary(&never).expect("something is wrong");
        assert!(text.contains("never"), "{text}");
        assert!(text.contains("sentinel audit"), "{text}");

        assert_eq!(summary(&[]), None, "silence about silence when there is none");
    }
}
