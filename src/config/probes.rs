//! Operator control over how often probes run.
//!
//! The cadences compiled into each probe are chosen for a cluster of a few
//! hundred nodes on a healthy network. They are not right everywhere: a
//! thousand-node cluster may want the five-second reachability probe slowed
//! down, a link with satellite latency needs longer timeouts, and a small
//! testbed may want everything faster so a scenario finishes in a minute.
//!
//! Overrides are per probe and additive. Anything not named here keeps its
//! compiled-in schedule, so a configuration file that mentions one probe does
//! not silently reset the other ten.
//!
//! `max_outstanding` is the exception: it may be lowered but never raised.
//! The NFS and journal probes pin it to one because a blocked syscall must
//! never be joined by a second one (SPEC.md §76, design principle 10), and a
//! configuration file is not the place to overturn that.

use std::collections::BTreeMap;
use std::time::Duration;

use serde::{Deserialize, Serialize};

/// Overrides for one probe.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProbeSchedule {
    /// How often the probe runs.
    #[serde(default, with = "humantime_serde::option")]
    pub interval: Option<Duration>,
    /// How long one execution may take.
    #[serde(default, with = "humantime_serde::option")]
    pub timeout: Option<Duration>,
    /// How many executions may be in flight per target.
    ///
    /// Only ever applied downward; see the module note.
    #[serde(default)]
    pub max_outstanding: Option<u32>,
    /// Whether the probe runs at all.
    ///
    /// Turning one off is a real operational need -- a site with no NFS, or
    /// one whose GPUs are managed by something else -- but it is a decision,
    /// so it is written down rather than inferred.
    #[serde(default)]
    pub enabled: Option<bool>,
}

/// Probe schedule overrides, keyed by probe id.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ProbeSchedules(pub BTreeMap<String, ProbeSchedule>);

impl ProbeSchedules {
    /// Whether anything is overridden.
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// The override for one probe, if any.
    pub fn get(&self, probe_id: &str) -> Option<&ProbeSchedule> {
        self.0.get(probe_id)
    }

    /// Whether a probe should run at all.
    pub fn is_enabled(&self, probe_id: &str) -> bool {
        self.get(probe_id).and_then(|s| s.enabled).unwrap_or(true)
    }

    /// The probe ids named here.
    pub fn ids(&self) -> impl Iterator<Item = &str> {
        self.0.keys().map(|k| k.as_str())
    }

    /// Apply the override for `definition.id`, if there is one.
    ///
    /// Takes the definition by reference so the probe's own copy is changed:
    /// several probes read their timeout to bound the command they run, and a
    /// schedule the runner enforced but the probe did not know about would be
    /// two different timeouts wearing one name.
    pub fn apply(&self, definition: &mut crate::probes::ProbeDefinition) {
        let Some(schedule) = self.0.get(definition.id.as_str()) else {
            return;
        };
        if let Some(interval) = schedule.interval {
            definition.interval = interval;
        }
        if let Some(timeout) = schedule.timeout {
            definition.timeout = timeout;
        }
        if let Some(max_outstanding) = schedule.max_outstanding {
            // Downward only. Raising the NFS or journal probe's limit would
            // let blocked syscalls accumulate, which is the one thing the
            // limit exists to prevent.
            definition.max_outstanding = definition.max_outstanding.min(max_outstanding.max(1));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::probes::ProbeDefinition;

    fn schedules(toml_text: &str) -> ProbeSchedules {
        toml::from_str(toml_text).expect("schedules")
    }

    #[test]
    fn nothing_configured_changes_nothing() {
        let mut definition = ProbeDefinition::new("network.tcp").every(Duration::from_secs(5));
        ProbeSchedules::default().apply(&mut definition);
        assert_eq!(definition.interval, Duration::from_secs(5));
    }

    #[test]
    fn an_interval_can_be_changed() {
        let schedules = schedules("\"network.tcp\" = { interval = \"30s\" }");
        let mut definition = ProbeDefinition::new("network.tcp").every(Duration::from_secs(5));
        schedules.apply(&mut definition);
        assert_eq!(definition.interval, Duration::from_secs(30));
    }

    #[test]
    fn naming_one_probe_leaves_the_others_alone() {
        // Otherwise a file that tunes one probe would silently reset ten.
        let schedules = schedules("\"network.tcp\" = { interval = \"30s\" }");
        let mut other = ProbeDefinition::new("host.metrics").every(Duration::from_secs(15));
        schedules.apply(&mut other);
        assert_eq!(other.interval, Duration::from_secs(15));
    }

    #[test]
    fn an_unset_field_keeps_the_compiled_value() {
        let schedules = schedules("\"host.metrics\" = { interval = \"60s\" }");
        let mut definition = ProbeDefinition::new("host.metrics")
            .every(Duration::from_secs(15))
            .within(Duration::from_secs(5));
        schedules.apply(&mut definition);
        assert_eq!(definition.interval, Duration::from_secs(60));
        assert_eq!(definition.timeout, Duration::from_secs(5));
    }

    #[test]
    fn concurrency_can_be_lowered() {
        let schedules = schedules("\"systemd.unit\" = { max_outstanding = 2 }");
        let mut definition = ProbeDefinition::new("systemd.unit").max_outstanding(8);
        schedules.apply(&mut definition);
        assert_eq!(definition.max_outstanding, 2);
    }

    #[test]
    fn concurrency_cannot_be_raised() {
        // The NFS and journal probes pin this to one because a blocked syscall
        // must never be joined by a second one. A config file does not get to
        // overturn that.
        let schedules = schedules("\"nfs.client.io\" = { max_outstanding = 64 }");
        let mut definition = ProbeDefinition::new("nfs.client.io").max_outstanding(1);
        schedules.apply(&mut definition);
        assert_eq!(definition.max_outstanding, 1);
    }

    #[test]
    fn concurrency_cannot_be_set_to_zero() {
        // Zero would mean "never run", expressed by accident. `enabled` is how
        // that gets said on purpose.
        let schedules = schedules("\"systemd.unit\" = { max_outstanding = 0 }");
        let mut definition = ProbeDefinition::new("systemd.unit").max_outstanding(8);
        schedules.apply(&mut definition);
        assert_eq!(definition.max_outstanding, 1);
    }

    #[test]
    fn a_probe_can_be_switched_off() {
        let schedules = schedules("\"gpu.nvidia\" = { enabled = false }");
        assert!(!schedules.is_enabled("gpu.nvidia"));
        assert!(schedules.is_enabled("host.metrics"));
    }

    #[test]
    fn a_misspelled_field_is_refused_rather_than_ignored() {
        // `intervall = "30s"` silently doing nothing is the worst outcome:
        // the operator believes they changed something.
        let error = toml::from_str::<ProbeSchedules>("\"network.tcp\" = { intervall = \"30s\" }")
            .expect_err("should be refused");
        assert!(error.to_string().contains("intervall"), "{error}");
    }
}
