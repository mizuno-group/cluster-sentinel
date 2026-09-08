//! Host-level metrics (SPEC.md §58).
//!
//! Reads `/proc`. Nothing here executes a command or touches a filesystem that
//! could block, so it stays safe on a host whose NFS mounts are hung.

use std::time::Duration;

use async_trait::async_trait;

use crate::capability::well_known;
use crate::entity::EntityType;
use crate::observation::{Observation, ProbeStatus};
use crate::probes::{Probe, ProbeContext, ProbeDefinition};

/// Probe id.
pub const PROBE_ID: &str = "host.metrics";

/// Load average and memory pressure, read from `/proc`.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct HostMetrics {
    /// One-, five- and fifteen-minute load averages.
    pub load: Option<[f64; 3]>,
    /// Total memory in MiB.
    pub memory_total_mb: Option<u64>,
    /// Available memory in MiB.
    pub memory_available_mb: Option<u64>,
    /// Seconds since boot.
    pub uptime_seconds: Option<u64>,
    /// Linux boot id.
    pub boot_id: Option<String>,
    /// Some-pressure average over ten seconds, if the kernel reports it.
    pub memory_pressure: Option<f64>,
    /// Number of logical CPUs, used to make load comparable across hosts.
    pub cpus: Option<u32>,
}

impl HostMetrics {
    /// Memory in use, as a fraction between 0 and 1.
    pub fn memory_used_fraction(&self) -> Option<f64> {
        let total = self.memory_total_mb? as f64;
        let available = self.memory_available_mb? as f64;
        if total <= 0.0 {
            return None;
        }
        Some(((total - available) / total).clamp(0.0, 1.0))
    }

    /// One-minute load per CPU.
    pub fn load_per_cpu(&self) -> Option<f64> {
        let load = self.load?[0];
        let cpus = self.cpus.filter(|c| *c > 0)? as f64;
        Some(load / cpus)
    }
}

/// Parse `/proc/loadavg`.
pub fn parse_loadavg(text: &str) -> Option<[f64; 3]> {
    let mut fields = text.split_whitespace();
    Some([
        fields.next()?.parse().ok()?,
        fields.next()?.parse().ok()?,
        fields.next()?.parse().ok()?,
    ])
}

/// Parse `/proc/meminfo`, returning `(total_mb, available_mb)`.
pub fn parse_meminfo(text: &str) -> (Option<u64>, Option<u64>) {
    let field = |name: &str| {
        text.lines()
            .find(|line| line.starts_with(name))
            .and_then(|line| line.split_whitespace().nth(1))
            .and_then(|kb| kb.parse::<u64>().ok())
            .map(|kb| kb / 1024)
    };
    (field("MemTotal:"), field("MemAvailable:"))
}

/// Parse `/proc/uptime`.
pub fn parse_uptime(text: &str) -> Option<u64> {
    text.split_whitespace().next()?.parse::<f64>().ok().map(|s| s as u64)
}

/// Parse the `some avg10` value out of a `/proc/pressure/*` file.
pub fn parse_pressure(text: &str) -> Option<f64> {
    let line = text.lines().find(|line| line.starts_with("some"))?;
    line.split_whitespace()
        .find_map(|field| field.strip_prefix("avg10="))
        .and_then(|value| value.parse().ok())
}

/// Thresholds above which the host is reported as degraded.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct HostThresholds {
    /// One-minute load per CPU above which the host is degraded.
    pub load_per_cpu: f64,
    /// Memory-used fraction above which the host is degraded.
    pub memory_used: f64,
    /// Memory `some avg10` pressure above which the host is degraded.
    pub memory_pressure: f64,
}

impl Default for HostThresholds {
    fn default() -> Self {
        // Deliberately loose. A busy compute node is doing its job, not
        // failing, and a monitoring system that cries wolf about load on a
        // cluster built to be loaded teaches operators to ignore it.
        Self {
            load_per_cpu: 4.0,
            memory_used: 0.95,
            memory_pressure: 60.0,
        }
    }
}

/// Judge metrics against thresholds.
pub fn assess(metrics: &HostMetrics, thresholds: &HostThresholds) -> (ProbeStatus, Vec<String>) {
    let mut reasons = Vec::new();

    if let Some(load) = metrics.load_per_cpu() {
        if load > thresholds.load_per_cpu {
            reasons.push(format!("load per cpu {load:.2} above {:.2}", thresholds.load_per_cpu));
        }
    }
    if let Some(used) = metrics.memory_used_fraction() {
        if used > thresholds.memory_used {
            reasons.push(format!("memory {:.0}% used", used * 100.0));
        }
    }
    if let Some(pressure) = metrics.memory_pressure {
        if pressure > thresholds.memory_pressure {
            reasons.push(format!("memory pressure {pressure:.1}"));
        }
    }

    // Degraded, never failed: a loaded host is still a working host, and only
    // a probe that cannot read anything at all is evidence of a fault.
    let status = if reasons.is_empty() {
        ProbeStatus::Ok
    } else {
        ProbeStatus::Degraded
    };
    (status, reasons)
}

/// Reads host metrics from `/proc`.
#[derive(Debug, Clone)]
pub struct HostMetricsProbe {
    definition: ProbeDefinition,
    thresholds: HostThresholds,
    proc_root: std::path::PathBuf,
}

impl Default for HostMetricsProbe {
    fn default() -> Self {
        Self::new()
    }
}

impl HostMetricsProbe {
    /// A probe reading the real `/proc`.
    pub fn new() -> Self {
        Self {
            definition: ProbeDefinition::new(PROBE_ID)
                .requiring([well_known::HOST_METRICS])
                .targeting([EntityType::Host])
                .every(Duration::from_secs(15))
                .within(Duration::from_secs(5)),
            thresholds: HostThresholds::default(),
            proc_root: std::path::PathBuf::from("/proc"),
        }
    }

    /// Builder: read from somewhere other than `/proc`, for tests.
    pub fn with_proc_root(mut self, root: impl Into<std::path::PathBuf>) -> Self {
        self.proc_root = root.into();
        self
    }

    /// Builder: set the thresholds.
    pub fn with_thresholds(mut self, thresholds: HostThresholds) -> Self {
        self.thresholds = thresholds;
        self
    }

    /// Read the metrics.
    pub fn read(&self) -> HostMetrics {
        let read = |path: &str| std::fs::read_to_string(self.proc_root.join(path)).ok();
        let (memory_total_mb, memory_available_mb) = read("meminfo").map(|t| parse_meminfo(&t)).unwrap_or((None, None));

        HostMetrics {
            load: read("loadavg").and_then(|t| parse_loadavg(&t)),
            memory_total_mb,
            memory_available_mb,
            uptime_seconds: read("uptime").and_then(|t| parse_uptime(&t)),
            boot_id: read("sys/kernel/random/boot_id").map(|id| id.trim().to_string()),
            memory_pressure: read("pressure/memory").and_then(|t| parse_pressure(&t)),
            cpus: std::thread::available_parallelism().ok().map(|n| n.get() as u32),
        }
    }
}

#[async_trait]
impl Probe for HostMetricsProbe {
    fn definition(&self) -> &ProbeDefinition {
        &self.definition
    }

    async fn collect(&self, context: &ProbeContext) -> Observation {
        let metrics = self.read();

        // Nothing readable at all means the probe cannot do its job here, which
        // is not the same as the host being unhealthy.
        if metrics.load.is_none() && metrics.memory_total_mb.is_none() && metrics.uptime_seconds.is_none() {
            return Observation::new(PROBE_ID.into(), context.target_entity, ProbeStatus::Unsupported)
                .with_error("proc_unreadable", "cannot read /proc on this host");
        }

        let (status, reasons) = assess(&metrics, &self.thresholds);

        Observation::new(PROBE_ID.into(), context.target_entity, status).with_payload(serde_json::json!({
            "load": metrics.load,
            "load_per_cpu": metrics.load_per_cpu(),
            "cpus": metrics.cpus,
            "memory_total_mb": metrics.memory_total_mb,
            "memory_available_mb": metrics.memory_available_mb,
            "memory_used_fraction": metrics.memory_used_fraction(),
            "memory_pressure": metrics.memory_pressure,
            "uptime_seconds": metrics.uptime_seconds,
            "boot_id": metrics.boot_id,
            "reasons": reasons,
        }))
    }
}

// Operators may retune this probe's schedule in [probes].
crate::probes::configurable_probe!(HostMetricsProbe);

#[cfg(test)]
mod tests {
    use super::*;
    use crate::capability::CapabilitySet;
    use crate::entity::{EntityKey, EntityType};

    fn metrics() -> HostMetrics {
        HostMetrics {
            load: Some([1.0, 1.0, 1.0]),
            memory_total_mb: Some(1000),
            memory_available_mb: Some(800),
            uptime_seconds: Some(3600),
            boot_id: Some("boot-1".into()),
            memory_pressure: Some(0.0),
            cpus: Some(4),
        }
    }

    #[test]
    fn loadavg_parses() {
        assert_eq!(parse_loadavg("0.52 0.31 0.20 1/234 5678"), Some([0.52, 0.31, 0.20]));
        assert_eq!(parse_loadavg(""), None);
        assert_eq!(parse_loadavg("not numbers here"), None);
    }

    #[test]
    fn meminfo_parses_into_mebibytes() {
        let text = "MemTotal:       16316108 kB\nMemFree:         1000000 kB\nMemAvailable:   12000000 kB\n";
        let (total, available) = parse_meminfo(text);
        assert_eq!(total, Some(15933));
        assert_eq!(available, Some(11718));
    }

    #[test]
    fn a_meminfo_without_memavailable_still_yields_the_total() {
        let (total, available) = parse_meminfo("MemTotal: 1048576 kB\n");
        assert_eq!(total, Some(1024));
        assert_eq!(available, None);
    }

    #[test]
    fn uptime_parses() {
        assert_eq!(parse_uptime("12345.67 98765.43"), Some(12345));
        assert_eq!(parse_uptime("nonsense"), None);
    }

    #[test]
    fn pressure_parses_the_ten_second_average() {
        let text = "some avg10=1.23 avg60=4.56 avg300=7.89 total=123\nfull avg10=0.00 avg60=0.00 total=0\n";
        assert_eq!(parse_pressure(text), Some(1.23));
        assert_eq!(parse_pressure("full avg10=1.00 total=0\n"), None);
    }

    #[test]
    fn derived_values_are_computed_from_the_raw_readings() {
        let metrics = metrics();
        assert_eq!(metrics.memory_used_fraction(), Some(0.2));
        assert_eq!(metrics.load_per_cpu(), Some(0.25));
    }

    #[test]
    fn derived_values_are_absent_when_their_inputs_are() {
        let empty = HostMetrics::default();
        assert_eq!(empty.memory_used_fraction(), None);
        assert_eq!(empty.load_per_cpu(), None);

        let no_cpus = HostMetrics {
            cpus: Some(0),
            ..metrics()
        };
        assert_eq!(
            no_cpus.load_per_cpu(),
            None,
            "dividing by zero CPUs must not produce infinity"
        );
    }

    #[test]
    fn a_healthy_host_assesses_as_ok() {
        let (status, reasons) = assess(&metrics(), &HostThresholds::default());
        assert_eq!(status, ProbeStatus::Ok);
        assert!(reasons.is_empty());
    }

    #[test]
    fn a_busy_compute_node_is_not_reported_as_a_problem() {
        // A cluster node running at load 3 per CPU is doing exactly what it
        // was bought for.
        let busy = HostMetrics {
            load: Some([12.0, 12.0, 12.0]),
            cpus: Some(4),
            ..metrics()
        };
        assert_eq!(assess(&busy, &HostThresholds::default()).0, ProbeStatus::Ok);
    }

    #[test]
    fn an_overloaded_host_is_degraded_and_says_why() {
        let overloaded = HostMetrics {
            load: Some([100.0, 100.0, 100.0]),
            cpus: Some(4),
            ..metrics()
        };
        let (status, reasons) = assess(&overloaded, &HostThresholds::default());
        assert_eq!(status, ProbeStatus::Degraded);
        assert!(reasons[0].contains("load per cpu"), "{reasons:?}");
    }

    #[test]
    fn memory_exhaustion_is_degraded_never_failed() {
        // The host is still working; it is in trouble. Reporting a failure
        // here would be a claim the probe cannot support.
        let starved = HostMetrics {
            memory_available_mb: Some(10),
            ..metrics()
        };
        let (status, reasons) = assess(&starved, &HostThresholds::default());
        assert_eq!(status, ProbeStatus::Degraded);
        assert!(reasons.iter().any(|r| r.contains("memory")), "{reasons:?}");
    }

    #[test]
    fn every_breached_threshold_is_reported_not_just_the_first() {
        let bad = HostMetrics {
            load: Some([100.0, 100.0, 100.0]),
            memory_available_mb: Some(1),
            memory_pressure: Some(90.0),
            ..metrics()
        };
        assert_eq!(assess(&bad, &HostThresholds::default()).1.len(), 3);
    }

    #[tokio::test]
    async fn the_probe_reads_a_fake_proc_and_reports_its_findings() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(dir.path().join("loadavg"), "0.50 0.40 0.30 1/2 3").expect("write");
        std::fs::write(
            dir.path().join("meminfo"),
            "MemTotal: 1048576 kB\nMemAvailable: 524288 kB\n",
        )
        .expect("write");
        std::fs::write(dir.path().join("uptime"), "7200.00 1000.00").expect("write");

        let probe = HostMetricsProbe::new().with_proc_root(dir.path());
        let entity = EntityKey::new("lab", EntityType::Host, "node-a").entity_id();
        let observation = probe.collect(&ProbeContext::local(entity, CapabilitySet::new())).await;

        assert_eq!(observation.status, ProbeStatus::Ok);
        assert_eq!(observation.payload["uptime_seconds"], 7200);
        assert_eq!(observation.payload["memory_total_mb"], 1024);
        assert_eq!(observation.payload["memory_used_fraction"], 0.5);
    }

    #[tokio::test]
    async fn an_unreadable_proc_is_unsupported_not_a_host_failure() {
        // "I cannot see" is not "it is broken".
        let probe = HostMetricsProbe::new().with_proc_root("/nonexistent/proc");
        let entity = EntityKey::new("lab", EntityType::Host, "node-a").entity_id();
        let observation = probe.collect(&ProbeContext::local(entity, CapabilitySet::new())).await;

        assert_eq!(observation.status, ProbeStatus::Unsupported);
        assert!(!observation.status.is_bad());
    }

    #[test]
    fn the_probe_is_gated_on_the_host_metrics_capability() {
        let definition = HostMetricsProbe::new().definition().clone();
        assert!(definition.applies_to(EntityType::Host, &CapabilitySet::from_iter(["host.metrics"])));
        assert!(!definition.applies_to(EntityType::Host, &CapabilitySet::new()));
        assert!(!definition.applies_to(EntityType::Storage, &CapabilitySet::from_iter(["host.metrics"])));
    }

    #[tokio::test]
    async fn the_probe_works_against_the_real_proc_on_this_machine() {
        let probe = HostMetricsProbe::new();
        let entity = EntityKey::new("lab", EntityType::Host, "node-a").entity_id();
        let observation = probe.collect(&ProbeContext::local(entity, CapabilitySet::new())).await;
        assert_ne!(
            observation.status,
            ProbeStatus::Unsupported,
            "/proc should be readable here"
        );
    }
}
