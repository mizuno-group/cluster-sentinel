//! systemd service state (SPEC.md §62).
//!
//! Reads state through `systemctl show`, which is parseable, stable across
//! versions and read-only. Sentinel never starts, stops or restarts a unit
//! (SPEC.md §113).

use std::collections::BTreeMap;
use std::time::Duration;

use async_trait::async_trait;

use crate::capability::well_known;
use crate::command::{Allowlist, CommandRunner};
use crate::entity::EntityType;
use crate::observation::{Observation, ProbeStatus};
use crate::probes::{Probe, ProbeContext, ProbeDefinition};

/// Probe id.
pub const PROBE_ID: &str = "systemd.unit";

/// The fields worth asking for. Naming them keeps the output small and stable.
const FIELDS: &[&str] = &[
    "Id",
    "LoadState",
    "ActiveState",
    "SubState",
    "UnitFileState",
    "Result",
    "ExecMainStatus",
    "NRestarts",
    "ActiveEnterTimestamp",
];

/// A unit's state as systemd reports it.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct UnitState {
    /// `LoadState`: whether the unit file was found.
    pub load_state: String,
    /// `ActiveState`: active, inactive, failed, activating, deactivating.
    pub active_state: String,
    /// `SubState`: running, exited, dead, ...
    pub sub_state: String,
    /// `Result`: how it last exited.
    pub result: String,
    /// How many times systemd has restarted it.
    pub restarts: Option<u32>,
    /// Everything reported, including fields this build does not interpret.
    pub fields: BTreeMap<String, String>,
}

impl UnitState {
    /// Whether the unit is running normally.
    pub fn is_active(&self) -> bool {
        self.active_state == "active"
    }

    /// Whether systemd considers the unit failed.
    pub fn is_failed(&self) -> bool {
        self.active_state == "failed" || self.result == "exit-code" || self.result == "signal"
    }

    /// Whether systemd has never heard of this unit.
    ///
    /// Different from "stopped": a unit that is not installed is not a fault of
    /// the host, and reporting it as one would produce a permanent false alarm
    /// on every host that legitimately lacks it.
    pub fn is_not_found(&self) -> bool {
        self.load_state == "not-found" || self.load_state == "masked"
    }

    /// The status this unit state implies.
    pub fn status(&self) -> ProbeStatus {
        if self.is_not_found() {
            return ProbeStatus::NotApplicable;
        }
        if self.is_failed() {
            return ProbeStatus::Failed;
        }
        if self.is_active() {
            return ProbeStatus::Ok;
        }
        match self.active_state.as_str() {
            // Mid-transition: not healthy yet, but not a fault either.
            "activating" | "deactivating" | "reloading" => ProbeStatus::Degraded,
            // Deliberately stopped is still a stopped service, and something
            // that should be running and is not is a failure.
            _ => ProbeStatus::Failed,
        }
    }
}

/// Parse `systemctl show` output, which is `Key=Value` per line.
pub fn parse_show(output: &str) -> UnitState {
    let mut fields = BTreeMap::new();
    for line in output.lines() {
        if let Some((key, value)) = line.split_once('=') {
            fields.insert(key.trim().to_string(), value.trim().to_string());
        }
    }

    UnitState {
        load_state: fields.get("LoadState").cloned().unwrap_or_default(),
        active_state: fields.get("ActiveState").cloned().unwrap_or_default(),
        sub_state: fields.get("SubState").cloned().unwrap_or_default(),
        result: fields.get("Result").cloned().unwrap_or_default(),
        restarts: fields.get("NRestarts").and_then(|v| v.parse().ok()),
        fields,
    }
}

/// Checks a systemd unit's state.
#[derive(Debug, Clone)]
pub struct SystemdProbe {
    definition: ProbeDefinition,
    allowlist: Allowlist,
}

impl Default for SystemdProbe {
    fn default() -> Self {
        Self::new()
    }
}

impl SystemdProbe {
    /// A probe with the default schedule.
    pub fn new() -> Self {
        Self {
            definition: ProbeDefinition::new(PROBE_ID)
                .requiring([well_known::SYSTEMD])
                .targeting([EntityType::Service, EntityType::Host])
                .every(Duration::from_secs(10))
                .within(Duration::from_secs(5)),
            allowlist: Allowlist::builtin(),
        }
    }

    /// Ask systemd about one unit.
    pub async fn query(&self, unit: &str) -> Result<UnitState, String> {
        let output = CommandRunner::new("systemctl")
            .args(["show", unit, "--no-pager", &format!("--property={}", FIELDS.join(","))])
            .timeout(self.definition.timeout)
            // systemctl output for a handful of properties is tiny; a cap
            // this low turns a runaway into a truncation rather than memory.
            .output_limit(64 * 1024)
            .run(&self.allowlist)
            .await
            .map_err(|error| error.to_string())?;

        if !output.is_success() && output.stdout.trim().is_empty() {
            return Err(format!("systemctl failed: {}", output.stderr.trim()));
        }
        Ok(parse_show(&output.stdout))
    }
}

#[async_trait]
impl Probe for SystemdProbe {
    fn definition(&self) -> &ProbeDefinition {
        &self.definition
    }

    async fn collect(&self, context: &ProbeContext) -> Observation {
        let Some(unit) = context.parameter_str("unit") else {
            return Observation::new(PROBE_ID.into(), context.target_entity, ProbeStatus::NotApplicable)
                .with_error("no_unit", "no systemd unit is configured for this entity");
        };

        match self.query(unit).await {
            Ok(state) => Observation::new(PROBE_ID.into(), context.target_entity, state.status()).with_payload(
                serde_json::json!({
                    "unit": unit,
                    "load_state": state.load_state,
                    "active_state": state.active_state,
                    "sub_state": state.sub_state,
                    "result": state.result,
                    "restarts": state.restarts,
                }),
            ),
            // systemd being unreachable is a limitation of the observer, not a
            // verdict on the service.
            Err(detail) => Observation::new(PROBE_ID.into(), context.target_entity, ProbeStatus::Unsupported)
                .with_payload(serde_json::json!({"unit": unit}))
                .with_error("systemctl_unavailable", detail),
        }
    }
}

// Operators may retune this probe's schedule in [probes].
crate::probes::configurable_probe!(SystemdProbe);

#[cfg(test)]
mod tests {
    use super::*;
    use crate::capability::CapabilitySet;
    use crate::entity::{EntityKey, EntityType};

    fn state(output: &str) -> UnitState {
        parse_show(output)
    }

    #[test]
    fn a_running_unit_parses_as_active() {
        let unit = state(
            "Id=sshd.service\nLoadState=loaded\nActiveState=active\nSubState=running\nResult=success\nNRestarts=0\n",
        );
        assert!(unit.is_active());
        assert!(!unit.is_failed());
        assert_eq!(unit.status(), ProbeStatus::Ok);
        assert_eq!(unit.restarts, Some(0));
    }

    #[test]
    fn a_failed_unit_parses_as_failed() {
        let unit =
            state("Id=slurmd.service\nLoadState=loaded\nActiveState=failed\nSubState=failed\nResult=exit-code\n");
        assert!(unit.is_failed());
        assert_eq!(unit.status(), ProbeStatus::Failed);
    }

    #[test]
    fn a_stopped_unit_is_a_failure_because_it_should_be_running() {
        let unit = state("Id=slurmd.service\nLoadState=loaded\nActiveState=inactive\nSubState=dead\nResult=success\n");
        assert!(!unit.is_active());
        assert_eq!(unit.status(), ProbeStatus::Failed);
    }

    #[test]
    fn a_unit_that_does_not_exist_here_is_not_applicable() {
        // Otherwise every host without slurmd would report a permanent fault.
        let unit = state("Id=nope.service\nLoadState=not-found\nActiveState=inactive\nSubState=dead\n");
        assert!(unit.is_not_found());
        assert_eq!(unit.status(), ProbeStatus::NotApplicable);
        assert!(!unit.status().is_bad());
    }

    #[test]
    fn a_masked_unit_is_also_not_applicable() {
        let unit = state("Id=x.service\nLoadState=masked\nActiveState=inactive\n");
        assert_eq!(unit.status(), ProbeStatus::NotApplicable);
    }

    #[test]
    fn a_unit_mid_transition_is_degraded_not_failed() {
        for active_state in ["activating", "deactivating", "reloading"] {
            let unit = state(&format!(
                "LoadState=loaded\nActiveState={active_state}\nSubState=start\n"
            ));
            assert_eq!(unit.status(), ProbeStatus::Degraded, "{active_state}");
        }
    }

    #[test]
    fn a_unit_killed_by_a_signal_counts_as_failed() {
        let unit = state("LoadState=loaded\nActiveState=inactive\nSubState=dead\nResult=signal\n");
        assert!(unit.is_failed());
    }

    #[test]
    fn unknown_properties_are_kept_rather_than_dropped() {
        let unit = state("LoadState=loaded\nActiveState=active\nSomeFutureProperty=42\n");
        assert_eq!(unit.fields.get("SomeFutureProperty").map(String::as_str), Some("42"));
    }

    #[test]
    fn a_value_containing_an_equals_sign_survives() {
        let unit = state("Environment=FOO=bar BAZ=qux\nActiveState=active\nLoadState=loaded\n");
        assert_eq!(
            unit.fields.get("Environment").map(String::as_str),
            Some("FOO=bar BAZ=qux")
        );
    }

    #[test]
    fn empty_output_yields_an_empty_state_rather_than_panicking() {
        let unit = state("");
        assert_eq!(unit, UnitState::default());
        assert_eq!(unit.status(), ProbeStatus::Failed, "no evidence of a running service");
    }

    #[tokio::test]
    async fn an_entity_with_no_unit_is_not_applicable() {
        let observation = SystemdProbe::new()
            .collect(&ProbeContext::local(
                EntityKey::new("lab", EntityType::Service, "x").entity_id(),
                CapabilitySet::new(),
            ))
            .await;
        assert_eq!(observation.status, ProbeStatus::NotApplicable);
    }

    #[test]
    fn the_probe_is_gated_on_systemd_and_targets_services() {
        let definition = SystemdProbe::new().definition().clone();
        let with = CapabilitySet::from_iter(["systemd"]);
        assert!(definition.applies_to(EntityType::Service, &with));
        assert!(definition.applies_to(EntityType::Host, &with));
        assert!(!definition.applies_to(EntityType::Service, &CapabilitySet::new()));
        assert!(!definition.applies_to(EntityType::Storage, &with));
    }

    #[test]
    fn the_probe_only_ever_reads() {
        // SPEC.md §113: no start, stop, restart or enable, ever.
        let implementation = include_str!("systemd.rs")
            .split("#[cfg(test)]")
            .next()
            .expect("implementation");
        for forbidden in [
            "\"start\"",
            "\"stop\"",
            "\"restart\"",
            "\"enable\"",
            "\"disable\"",
            "\"kill\"",
        ] {
            assert!(
                !implementation.contains(forbidden),
                "systemd probe must never {forbidden}"
            );
        }
    }
}
