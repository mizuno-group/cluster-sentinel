//! NVIDIA GPU probe (SPEC.md §81, §82).
//!
//! Uses `nvidia-smi` rather than linking NVML: one portable binary that works
//! across driver versions is worth more here than the efficiency of the C API,
//! for the same reason `scontrol` is preferred to `libslurm` (SPEC.md §64).
//!
//! The important restraint is about absence. A host with no GPUs is not a
//! broken host, and `nvidia-smi` missing is not a fault unless something
//! expected GPUs to be there. Only a *disagreement* between what is expected
//! and what is present is worth reporting, and saying which is which is the
//! diagnosis engine's job, not this probe's.

use std::time::Duration;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::capability::well_known;
use crate::command::{Allowlist, CommandRunner};
use crate::entity::EntityType;
use crate::observation::{Observation, ProbeStatus};
use crate::probes::{Probe, ProbeContext, ProbeDefinition};

/// Probe id.
pub const PROBE_ID: &str = "gpu.nvidia";

/// The fields queried, in order. Naming them keeps the output stable across
/// driver versions, which change the default table layout freely.
const QUERY_FIELDS: &[&str] = &[
    "index",
    "uuid",
    "name",
    "temperature.gpu",
    "memory.total",
    "memory.used",
    "utilization.gpu",
    "power.draw",
];

/// One GPU as `nvidia-smi` reports it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Gpu {
    /// Device index.
    pub index: u32,
    /// Stable device UUID.
    pub uuid: String,
    /// Model name.
    pub name: String,
    /// Temperature in degrees Celsius.
    pub temperature_c: Option<f64>,
    /// Total memory in MiB.
    pub memory_total_mb: Option<u64>,
    /// Used memory in MiB.
    pub memory_used_mb: Option<u64>,
    /// Utilisation percentage.
    pub utilization_percent: Option<f64>,
    /// Power draw in watts.
    pub power_w: Option<f64>,
}

impl Gpu {
    /// Memory in use, as a fraction.
    pub fn memory_used_fraction(&self) -> Option<f64> {
        let total = self.memory_total_mb? as f64;
        if total <= 0.0 {
            return None;
        }
        Some((self.memory_used_mb? as f64 / total).clamp(0.0, 1.0))
    }
}

/// Parse `nvidia-smi --query-gpu=... --format=csv,noheader,nounits`.
///
/// Tolerant of the `[N/A]` and `[Not Supported]` placeholders the driver emits
/// for fields a particular card does not report: those become `None`, not
/// parse failures. A consumer GPU that cannot report power draw is not a
/// broken GPU.
pub fn parse_query(output: &str) -> Vec<Gpu> {
    output
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .filter_map(|line| {
            let fields: Vec<&str> = line.split(',').map(str::trim).collect();
            if fields.len() < 3 {
                return None;
            }

            let optional = |index: usize| -> Option<&str> {
                let value = fields.get(index)?.trim();
                if value.is_empty() || value.starts_with("[N/A") || value.starts_with("[Not") {
                    None
                } else {
                    Some(value)
                }
            };

            Some(Gpu {
                index: optional(0)?.parse().ok()?,
                uuid: optional(1)?.to_string(),
                name: optional(2)?.to_string(),
                temperature_c: optional(3).and_then(|v| v.parse().ok()),
                memory_total_mb: optional(4).and_then(|v| v.parse().ok()),
                memory_used_mb: optional(5).and_then(|v| v.parse().ok()),
                utilization_percent: optional(6).and_then(|v| v.parse().ok()),
                power_w: optional(7).and_then(|v| v.parse().ok()),
            })
        })
        .collect()
}

/// Thresholds above which a GPU is reported as degraded.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct GpuThresholds {
    /// Temperature above which the card is reported as degraded.
    pub temperature_c: f64,
}

impl Default for GpuThresholds {
    fn default() -> Self {
        // Well above a normal load temperature and below the thermal-slowdown
        // point of current cards. A GPU at 80C under load is working, not
        // failing, and reporting it would be noise on a busy cluster.
        Self { temperature_c: 90.0 }
    }
}

/// Reads GPU inventory and health.
#[derive(Debug, Clone)]
pub struct NvidiaGpuProbe {
    definition: ProbeDefinition,
    allowlist: Allowlist,
    thresholds: GpuThresholds,
}

impl Default for NvidiaGpuProbe {
    fn default() -> Self {
        Self::new()
    }
}

impl NvidiaGpuProbe {
    /// A probe with the default schedule.
    pub fn new() -> Self {
        Self {
            definition: ProbeDefinition::new(PROBE_ID)
                .requiring([well_known::GPU_NVIDIA])
                .targeting([EntityType::Host])
                .every(Duration::from_secs(15))
                .within(Duration::from_secs(10)),
            allowlist: Allowlist::builtin(),
            thresholds: GpuThresholds::default(),
        }
    }

    /// Builder: set the thresholds.
    pub fn with_thresholds(mut self, thresholds: GpuThresholds) -> Self {
        self.thresholds = thresholds;
        self
    }

    /// Run `nvidia-smi` and parse its output.
    pub async fn query(&self) -> Result<Vec<Gpu>, String> {
        let output = CommandRunner::new("nvidia-smi")
            .args([
                &format!("--query-gpu={}", QUERY_FIELDS.join(",")),
                "--format=csv,noheader,nounits",
            ])
            .timeout(self.definition.timeout)
            // A wedged driver can make nvidia-smi produce a great deal of
            // diagnostic output; this turns that into a truncation.
            .output_limit(256 * 1024)
            .run(&self.allowlist)
            .await
            .map_err(|error| error.to_string())?;

        if !output.is_success() {
            return Err(format!("nvidia-smi failed: {}", output.stderr.trim()));
        }
        Ok(parse_query(&output.stdout))
    }
}

#[async_trait]
impl Probe for NvidiaGpuProbe {
    fn definition(&self) -> &ProbeDefinition {
        &self.definition
    }

    async fn collect(&self, context: &ProbeContext) -> Observation {
        // How many GPUs something expects to find here, if anything does.
        // Supplied as a parameter so this probe never has to know about Slurm.
        let expected = context.parameter_u64("expected_gpu_count");

        let gpus = match self.query().await {
            Ok(gpus) => gpus,
            Err(detail) => {
                // No `nvidia-smi` and nothing expecting GPUs is an ordinary
                // host, not a fault.
                let status = match expected {
                    Some(count) if count > 0 => ProbeStatus::Failed,
                    _ => ProbeStatus::Unsupported,
                };
                return Observation::new(PROBE_ID.into(), context.target_entity, status)
                    .with_payload(serde_json::json!({
                        "gpu_count": 0,
                        "expected_gpu_count": expected,
                    }))
                    .with_error("nvidia_smi_unavailable", detail);
            }
        };

        let hot: Vec<&Gpu> = gpus
            .iter()
            .filter(|g| g.temperature_c.is_some_and(|t| t > self.thresholds.temperature_c))
            .collect();

        // A count disagreement is reported as a fact here; whether it is a
        // configuration error or a missing card is for a rule to decide.
        let count_mismatch = expected.is_some_and(|count| count != gpus.len() as u64);

        let status = if count_mismatch || !hot.is_empty() {
            ProbeStatus::Degraded
        } else {
            ProbeStatus::Ok
        };

        let observation =
            Observation::new(PROBE_ID.into(), context.target_entity, status).with_payload(serde_json::json!({
                "gpu_count": gpus.len(),
                "expected_gpu_count": expected,
                "count_matches_expectation": expected.map(|c| c == gpus.len() as u64),
                "gpus": gpus,
                "hot_gpu_count": hot.len(),
                "temperature_threshold_c": self.thresholds.temperature_c,
            }));

        if count_mismatch {
            observation.with_error(
                "gpu_count_mismatch",
                format!("{} GPU(s) expected, {} present", expected.unwrap_or(0), gpus.len()),
            )
        } else if !hot.is_empty() {
            observation.with_error(
                "gpu_temperature",
                format!("{} GPU(s) above {}C", hot.len(), self.thresholds.temperature_c),
            )
        } else {
            observation
        }
    }
}

// Operators may retune this probe's schedule in [probes].
crate::probes::configurable_probe!(NvidiaGpuProbe);

#[cfg(test)]
mod tests {
    use super::*;
    use crate::capability::CapabilitySet;
    use crate::entity::{EntityKey, EntityType};

    fn context(parameters: serde_json::Value) -> ProbeContext {
        ProbeContext::local(
            EntityKey::new("lab", EntityType::Host, "node-a").entity_id(),
            CapabilitySet::new(),
        )
        .with_parameters(parameters)
        .with_timeout(Duration::from_millis(500))
    }

    const TWO_GPUS: &str = "\
0, GPU-11111111-2222-3333-4444-555555555555, NVIDIA GeForce RTX 3090, 45, 24576, 1024, 12, 120.50
1, GPU-66666666-7777-8888-9999-000000000000, NVIDIA GeForce RTX 3090, 47, 24576, 2048, 35, 210.25
";

    #[test]
    fn a_two_gpu_output_parses_completely() {
        let gpus = parse_query(TWO_GPUS);
        assert_eq!(gpus.len(), 2);

        assert_eq!(gpus[0].index, 0);
        assert_eq!(gpus[0].name, "NVIDIA GeForce RTX 3090");
        assert_eq!(gpus[0].temperature_c, Some(45.0));
        assert_eq!(gpus[0].memory_total_mb, Some(24576));
        assert_eq!(gpus[0].power_w, Some(120.50));
        assert_eq!(gpus[1].index, 1);
    }

    #[test]
    fn unsupported_fields_become_absent_not_errors() {
        // A consumer card that cannot report power draw is not a broken card.
        let output = "0, GPU-abc, NVIDIA T400, 40, 4096, 100, 0, [N/A]\n";
        let gpus = parse_query(output);

        assert_eq!(gpus.len(), 1);
        assert_eq!(gpus[0].power_w, None);
        assert_eq!(gpus[0].temperature_c, Some(40.0), "the rest still parses");
    }

    #[test]
    fn the_not_supported_placeholder_is_handled_too() {
        let output = "0, GPU-abc, Tesla K80, [Not Supported], 11441, 0, 0, [Not Supported]\n";
        let gpus = parse_query(output);
        assert_eq!(gpus[0].temperature_c, None);
        assert_eq!(gpus[0].memory_total_mb, Some(11441));
    }

    #[test]
    fn empty_output_means_no_gpus_rather_than_a_parse_failure() {
        assert!(parse_query("").is_empty());
        assert!(parse_query("\n\n").is_empty());
    }

    #[test]
    fn a_malformed_line_is_skipped_without_losing_the_others() {
        let output = format!("garbage\n{TWO_GPUS}");
        assert_eq!(parse_query(&output).len(), 2);
    }

    #[test]
    fn memory_usage_is_derived_safely() {
        let gpus = parse_query(TWO_GPUS);
        assert!((gpus[1].memory_used_fraction().expect("fraction") - 0.0833).abs() < 0.001);

        let no_memory = Gpu {
            memory_total_mb: Some(0),
            ..gpus[0].clone()
        };
        assert_eq!(
            no_memory.memory_used_fraction(),
            None,
            "dividing by zero must not produce infinity"
        );
    }

    #[tokio::test]
    async fn a_host_without_nvidia_smi_and_without_expectations_is_unsupported() {
        // The overwhelmingly common case: an ordinary host with no GPUs.
        // Reporting a fault here would mean a permanent false alarm on every
        // non-GPU machine in the cluster.
        let observation = NvidiaGpuProbe::new().collect(&context(serde_json::Value::Null)).await;

        assert_eq!(observation.status, ProbeStatus::Unsupported);
        assert!(!observation.status.is_bad());
    }

    #[tokio::test]
    async fn a_host_that_should_have_gpus_but_has_no_driver_is_a_failure() {
        // Something expected GPUs here and there is no way to see them. That
        // is worth reporting.
        let observation = NvidiaGpuProbe::new()
            .collect(&context(serde_json::json!({"expected_gpu_count": 4})))
            .await;

        assert_eq!(observation.status, ProbeStatus::Failed);
        assert_eq!(observation.payload["expected_gpu_count"], 4);
        assert_eq!(observation.payload["gpu_count"], 0);
    }

    #[tokio::test]
    async fn expecting_zero_gpus_is_not_an_expectation_of_gpus() {
        let observation = NvidiaGpuProbe::new()
            .collect(&context(serde_json::json!({"expected_gpu_count": 0})))
            .await;
        assert_eq!(observation.status, ProbeStatus::Unsupported);
    }

    #[test]
    fn the_probe_is_gated_on_the_gpu_capability() {
        let definition = NvidiaGpuProbe::new().definition().clone();
        assert!(definition.applies_to(EntityType::Host, &CapabilitySet::from_iter(["gpu.nvidia"])));
        assert!(!definition.applies_to(EntityType::Host, &CapabilitySet::new()));
    }

    #[test]
    fn a_busy_gpu_is_not_reported_as_a_problem() {
        // A card at 80C under full load is doing its job.
        let thresholds = GpuThresholds::default();
        let gpu = Gpu {
            index: 0,
            uuid: "GPU-abc".into(),
            name: "NVIDIA A100".into(),
            temperature_c: Some(80.0),
            memory_total_mb: Some(40960),
            memory_used_mb: Some(40000),
            utilization_percent: Some(100.0),
            power_w: Some(400.0),
        };
        assert!(gpu.temperature_c.unwrap() < thresholds.temperature_c);
    }

    #[test]
    fn the_probe_never_changes_a_gpus_state() {
        // nvidia-smi can set persistence mode, clocks and power limits. None of
        // those may ever appear here (SPEC.md §113).
        let implementation = include_str!("gpu.rs")
            .split("#[cfg(test)]")
            .next()
            .expect("implementation");
        for forbidden in [
            "-pm",
            "--persistence-mode",
            "-r",
            "--gpu-reset",
            "-ac",
            "-pl",
            "--applications-clocks",
        ] {
            assert!(
                !implementation.contains(forbidden),
                "the GPU probe must never pass {forbidden}"
            );
        }
    }
}
