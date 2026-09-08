//! NFS client-side probes.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use async_trait::async_trait;

use crate::agent::system::{parse_mounts, MountInfo};
use crate::capability::well_known;
use crate::entity::EntityType;
use crate::observation::{Observation, ProbeStatus};
use crate::probes::{Probe, ProbeContext, ProbeDefinition};

/// Probe id for the safe, `/proc`-only mount check.
pub const PROBE_CLIENT_MOUNT: &str = "nfs.client.mount";
/// Probe id for the active filesystem check.
pub const PROBE_CLIENT_IO: &str = "nfs.client.io";

/// How an NFS mount is behaving (SPEC.md §77).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NfsStatus {
    /// Responding normally.
    Ok,
    /// Responding, but slowly enough to matter.
    Slow,
    /// Did not respond within the probe's timeout.
    Timeout,
    /// A previous probe is still blocked in the kernel.
    ///
    /// Distinct from a timeout: a timeout means this attempt gave up, while
    /// stuck means an earlier attempt never came back and the thread is still
    /// gone. It is the strongest signal available that a mount is wedged.
    Stuck,
}

impl NfsStatus {
    /// The probe status this implies.
    pub fn probe_status(&self) -> ProbeStatus {
        match self {
            NfsStatus::Ok => ProbeStatus::Ok,
            NfsStatus::Slow => ProbeStatus::Degraded,
            NfsStatus::Timeout => ProbeStatus::Timeout,
            NfsStatus::Stuck => ProbeStatus::Stuck,
        }
    }

    /// Stable string form.
    pub fn as_str(&self) -> &'static str {
        match self {
            NfsStatus::Ok => "NFS_OK",
            NfsStatus::Slow => "NFS_SLOW",
            NfsStatus::Timeout => "NFS_TIMEOUT",
            NfsStatus::Stuck => "NFS_STUCK",
        }
    }
}

/// Judge a latency against a threshold.
pub fn classify_latency(latency: Duration, slow_after: Duration) -> NfsStatus {
    if latency >= slow_after {
        NfsStatus::Slow
    } else {
        NfsStatus::Ok
    }
}

/// Reports which NFS filesystems are mounted, and how.
///
/// Reads `/proc/self/mounts` only, which is a read of kernel state and does not
/// touch the filesystems it describes. Safe even when every mount is hung.
#[derive(Debug, Clone)]
pub struct NfsMountProbe {
    definition: ProbeDefinition,
    mounts_path: PathBuf,
}

impl Default for NfsMountProbe {
    fn default() -> Self {
        Self::new()
    }
}

impl NfsMountProbe {
    /// A probe reading the real `/proc/self/mounts`.
    pub fn new() -> Self {
        Self {
            definition: ProbeDefinition::new(PROBE_CLIENT_MOUNT)
                .requiring([well_known::STORAGE_NFS_CLIENT])
                .targeting([EntityType::Host])
                .every(Duration::from_secs(30))
                .within(Duration::from_secs(5)),
            mounts_path: PathBuf::from("/proc/self/mounts"),
        }
    }

    /// Builder: read mounts from somewhere else, for tests.
    pub fn with_mounts_path(mut self, path: impl Into<PathBuf>) -> Self {
        self.mounts_path = path.into();
        self
    }

    /// The NFS mounts currently present.
    pub fn nfs_mounts(&self) -> Vec<MountInfo> {
        std::fs::read_to_string(&self.mounts_path)
            .map(|text| parse_mounts(&text).into_iter().filter(|m| m.is_nfs()).collect())
            .unwrap_or_default()
    }
}

#[async_trait]
impl Probe for NfsMountProbe {
    fn definition(&self) -> &ProbeDefinition {
        &self.definition
    }

    async fn collect(&self, context: &ProbeContext) -> Observation {
        let mounts = self.nfs_mounts();

        if mounts.is_empty() {
            return Observation::new(
                PROBE_CLIENT_MOUNT.into(),
                context.target_entity,
                ProbeStatus::NotApplicable,
            )
            .with_payload(serde_json::json!({"mounts": []}))
            .with_error("no_nfs_mounts", "this host has no NFS mounts");
        }

        // Recorded as a fact, not judged. `/proc/mounts` says a mount is `ro`;
        // it does not say why, and the two reasons are opposite in meaning. A
        // kernel that turned a filesystem read-only after errors is a fault. A
        // share exported or mounted read-only on purpose -- a dataset, a
        // reference tree -- is the normal state of many clusters, and calling
        // it degraded means a permanent false alarm on every node that has
        // one, which teaches people to ignore the storage component.
        //
        // The evidence that distinguishes them is in the kernel log, and the
        // journal probe already looks for it (`filesystem_readonly`). A
        // genuine remount is caught there, where there is something to see.
        let read_only: Vec<&MountInfo> = mounts.iter().filter(|m| m.is_read_only()).collect();

        let described: Vec<serde_json::Value> = mounts
            .iter()
            .map(|m| {
                serde_json::json!({
                    "source": m.source,
                    "target": m.target,
                    "fstype": m.fstype,
                    "server": m.nfs_server(),
                    "read_only": m.is_read_only(),
                })
            })
            .collect();

        let observation = Observation::new(PROBE_CLIENT_MOUNT.into(), context.target_entity, ProbeStatus::Ok)
            .with_payload(serde_json::json!({
                "mounts": described,
                "mount_count": mounts.len(),
                "read_only_count": read_only.len(),
                "servers": mounts.iter().filter_map(|m| m.nfs_server()).collect::<Vec<_>>(),
            }));

        if read_only.is_empty() {
            observation
        } else {
            // Stated, not diagnosed: "is" rather than "has been remounted",
            // because the probe cannot see which happened.
            observation.with_evidence(serde_json::json!({
                "read_only_mounts": read_only.iter().map(|m| m.target.clone()).collect::<Vec<_>>(),
                "note": format!(
                    "{} mount(s) are read-only: {}",
                    read_only.len(),
                    read_only
                        .iter()
                        .map(|m| m.target.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                ),
            }))
        }
    }
}

/// Actively touches one NFS mount to see whether it responds.
///
/// This is the dangerous one. It runs a single `stat()` on a blocking thread
/// with a timeout, and its definition sets `max_outstanding = 1` so the probe
/// runner refuses to start a second one while the first is still gone. If the
/// mount is wedged, the thread is lost — that is what a hard mount does — but
/// exactly one thread is lost, not one per interval.
#[derive(Debug, Clone)]
pub struct NfsClientIoProbe {
    definition: ProbeDefinition,
    slow_after: Duration,
}

impl Default for NfsClientIoProbe {
    fn default() -> Self {
        Self::new()
    }
}

impl NfsClientIoProbe {
    /// A probe with the default schedule and threshold.
    pub fn new() -> Self {
        Self {
            definition: ProbeDefinition::new(PROBE_CLIENT_IO)
                .requiring([well_known::STORAGE_NFS_CLIENT])
                .targeting([EntityType::Host, EntityType::Storage])
                .every(Duration::from_secs(30))
                .within(Duration::from_secs(10))
                // The whole point. See the module documentation.
                .max_outstanding(1),
            slow_after: Duration::from_secs(2),
        }
    }

    /// Builder: set the latency above which a mount is called slow.
    pub fn slow_after(mut self, slow_after: Duration) -> Self {
        self.slow_after = slow_after;
        self
    }

    /// `stat()` a path on a blocking thread, with a deadline.
    ///
    /// Returns `None` if the deadline passed, in which case the blocking thread
    /// may still be in the kernel and is deliberately not waited for.
    pub async fn stat_with_deadline(path: &Path, timeout: Duration) -> Option<Result<Duration, String>> {
        let path = path.to_path_buf();
        let started = Instant::now();

        // spawn_blocking, because this call can block for an unbounded time and
        // must never occupy an async worker thread.
        let handle = tokio::task::spawn_blocking(move || std::fs::metadata(&path).map(|_| ()));

        match tokio::time::timeout(timeout, handle).await {
            Ok(Ok(Ok(()))) => Some(Ok(started.elapsed())),
            Ok(Ok(Err(error))) => Some(Err(error.to_string())),
            Ok(Err(error)) => Some(Err(format!("probe task failed: {error}"))),
            Err(_) => None,
        }
    }
}

#[async_trait]
impl Probe for NfsClientIoProbe {
    fn definition(&self) -> &ProbeDefinition {
        &self.definition
    }

    async fn collect(&self, context: &ProbeContext) -> Observation {
        let Some(mount_point) = context.parameter_str("mount_point") else {
            return Observation::new(
                PROBE_CLIENT_IO.into(),
                context.target_entity,
                ProbeStatus::NotApplicable,
            )
            .with_error("no_mount_point", "no mount point is configured for this entity");
        };

        let outcome = Self::stat_with_deadline(Path::new(mount_point), context.timeout).await;

        let (status, latency, detail) = match outcome {
            Some(Ok(latency)) => (classify_latency(latency, self.slow_after), Some(latency), None),
            Some(Err(error)) => {
                // The mount answered, and the answer was an error. That is a
                // fault, but it is emphatically not a hang.
                return Observation::new(PROBE_CLIENT_IO.into(), context.target_entity, ProbeStatus::Failed)
                    .with_payload(serde_json::json!({
                        "mount_point": mount_point,
                        "nfs_status": "NFS_ERROR",
                        "responded": true,
                    }))
                    .with_error("stat_failed", error);
            }
            None => (
                NfsStatus::Timeout,
                None,
                Some("the mount did not respond within the timeout".to_string()),
            ),
        };

        let observation = Observation::new(PROBE_CLIENT_IO.into(), context.target_entity, status.probe_status())
            .with_payload(serde_json::json!({
                "mount_point": mount_point,
                "nfs_status": status.as_str(),
                "latency_ms": latency.map(|l| l.as_millis() as u64),
                "responded": latency.is_some(),
            }));

        match detail {
            Some(detail) => observation.with_error("timed_out", detail),
            None => observation,
        }
    }
}

// Operators may retune this probe's schedule in [probes].
crate::probes::configurable_probe!(NfsMountProbe, NfsClientIoProbe);

#[cfg(test)]
mod tests {
    use super::*;
    use crate::capability::CapabilitySet;
    use crate::entity::{EntityKey, EntityType};

    fn entity() -> crate::entity::EntityId {
        EntityKey::new("lab", EntityType::Host, "node-a").entity_id()
    }

    fn context(parameters: serde_json::Value) -> ProbeContext {
        ProbeContext::local(entity(), CapabilitySet::new())
            .with_parameters(parameters)
            .with_timeout(Duration::from_millis(500))
    }

    fn mounts_file(contents: &str) -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("mounts");
        std::fs::write(&path, contents).expect("write");
        (dir, path)
    }

    #[test]
    fn nfs_statuses_map_to_probe_statuses() {
        assert_eq!(NfsStatus::Ok.probe_status(), ProbeStatus::Ok);
        assert_eq!(NfsStatus::Slow.probe_status(), ProbeStatus::Degraded);
        assert_eq!(NfsStatus::Timeout.probe_status(), ProbeStatus::Timeout);
        assert_eq!(NfsStatus::Stuck.probe_status(), ProbeStatus::Stuck);
    }

    #[test]
    fn a_slow_mount_is_degraded_not_failed() {
        // Storage that answers eventually is still storage that answers.
        assert_eq!(
            classify_latency(Duration::from_millis(10), Duration::from_secs(2)),
            NfsStatus::Ok
        );
        assert_eq!(
            classify_latency(Duration::from_secs(5), Duration::from_secs(2)),
            NfsStatus::Slow
        );
        assert_eq!(
            classify_latency(Duration::from_secs(2), Duration::from_secs(2)),
            NfsStatus::Slow
        );
    }

    #[tokio::test]
    async fn nfs_mounts_are_found_and_local_ones_ignored() {
        let (_dir, path) = mounts_file(
            "/dev/sda1 / ext4 rw,relatime 0 0\n\
             fs-a:/export/home /home nfs4 rw,relatime,vers=4.2 0 0\n\
             fs-b:/export/scratch /scratch nfs rw 0 0\n",
        );

        let probe = NfsMountProbe::new().with_mounts_path(&path);
        let observation = probe.collect(&context(serde_json::Value::Null)).await;

        assert_eq!(observation.status, ProbeStatus::Ok);
        assert_eq!(observation.payload["mount_count"], 2);
        let servers = observation.payload["servers"].as_array().expect("servers");
        assert_eq!(servers.len(), 2);
        assert!(servers.iter().any(|s| s == "fs-a"));
    }

    #[tokio::test]
    async fn a_host_with_no_nfs_mounts_is_not_applicable() {
        // Not a fault: most hosts legitimately mount nothing over NFS.
        let (_dir, path) = mounts_file("/dev/sda1 / ext4 rw 0 0\n");
        let observation = NfsMountProbe::new()
            .with_mounts_path(&path)
            .collect(&context(serde_json::Value::Null))
            .await;

        assert_eq!(observation.status, ProbeStatus::NotApplicable);
        assert!(!observation.status.is_bad());
    }

    #[tokio::test]
    async fn a_read_only_mount_is_recorded_but_not_called_a_fault() {
        // `/proc/mounts` says a mount is read-only; it does not say why, and
        // the two reasons are opposite in meaning. A share exported read-only
        // on purpose is the normal state of many clusters, and degrading on it
        // is a permanent false alarm. A kernel that flipped one after errors
        // is caught by the journal probe, where there is evidence to see.
        let (_dir, path) = mounts_file("fs1:/data /data nfs4 ro,vers=4.2 0 0\n");
        let observation = NfsMountProbe::new()
            .with_mounts_path(&path)
            .collect(&context(serde_json::Value::Null))
            .await;

        assert_eq!(observation.status, ProbeStatus::Ok);
        assert_eq!(observation.payload["read_only_count"], 1);
        assert!(
            observation.evidence["note"]
                .as_str()
                .unwrap_or_default()
                .contains("/data"),
            "the fact must still be recorded: {:?}",
            observation.evidence
        );
    }

    #[tokio::test]
    async fn the_mount_probe_never_touches_the_filesystem_it_describes() {
        // Reading /proc must work even when every mount named in it is hung.
        // The path here does not exist at all, which stands in for that.
        let (_dir, path) = mounts_file("fs-a:/export /nonexistent/hung/mount nfs4 rw 0 0\n");
        let observation = NfsMountProbe::new()
            .with_mounts_path(&path)
            .collect(&context(serde_json::Value::Null))
            .await;

        assert_eq!(
            observation.status,
            ProbeStatus::Ok,
            "describing a mount must not require touching it"
        );
        assert_eq!(observation.payload["mount_count"], 1);
    }

    #[test]
    fn the_mount_probe_reads_only_proc() {
        let implementation = include_str!("client.rs")
            .split("#[cfg(test)]")
            .next()
            .expect("implementation");
        // `metadata` is the one filesystem call in this file, and it belongs to
        // the active probe, which is bounded. Nothing may call `read_dir`,
        // `df` or anything else that walks a filesystem.
        for forbidden in ["read_dir", "Command::new", "walk", "\"df\"", "\"ls\""] {
            assert!(
                !implementation.contains(forbidden),
                "NFS client probes must not use {forbidden}"
            );
        }
    }

    #[test]
    fn the_active_probe_allows_exactly_one_outstanding_execution() {
        // SPEC.md §76. If this ever changes, a hung mount takes the agent down
        // one blocked thread at a time.
        assert_eq!(NfsClientIoProbe::new().definition().max_outstanding, 1);
    }

    #[test]
    fn the_safe_probe_is_not_needlessly_limited() {
        // Reading /proc cannot block, so limiting it would only add latency.
        assert!(NfsMountProbe::new().definition().max_outstanding > 1);
    }

    #[tokio::test]
    async fn the_active_probe_reports_a_responsive_mount() {
        let dir = tempfile::tempdir().expect("tempdir");
        let observation = NfsClientIoProbe::new()
            .collect(&context(
                serde_json::json!({"mount_point": dir.path().display().to_string()}),
            ))
            .await;

        assert_eq!(observation.status, ProbeStatus::Ok);
        assert_eq!(observation.payload["nfs_status"], "NFS_OK");
        assert_eq!(observation.payload["responded"], true);
        assert!(observation.payload["latency_ms"].is_number());
    }

    #[tokio::test]
    async fn a_missing_mount_point_is_a_failure_not_a_hang() {
        // The mount answered, and the answer was ENOENT. That is a fault, but
        // it is not the fault a hang would be, and the two need different
        // responses.
        let observation = NfsClientIoProbe::new()
            .collect(&context(serde_json::json!({"mount_point": "/nonexistent/mount/point"})))
            .await;

        assert_eq!(observation.status, ProbeStatus::Failed);
        assert_eq!(observation.payload["responded"], true);
        assert_eq!(observation.error_code.as_deref(), Some("stat_failed"));
    }

    #[tokio::test]
    async fn an_entity_with_no_mount_point_is_not_applicable() {
        let observation = NfsClientIoProbe::new().collect(&context(serde_json::Value::Null)).await;
        assert_eq!(observation.status, ProbeStatus::NotApplicable);
    }

    #[tokio::test]
    async fn a_stat_that_exceeds_its_deadline_returns_rather_than_waiting() {
        // The behaviour that keeps the agent alive during an outage: the probe
        // gives up, even though the blocking thread may still be in the kernel.
        let started = Instant::now();
        let outcome =
            NfsClientIoProbe::stat_with_deadline(Path::new("/proc/self/status"), Duration::from_nanos(1)).await;

        assert!(
            started.elapsed() < Duration::from_secs(2),
            "the deadline must be honoured"
        );
        // Either it finished within the nanosecond (unlikely) or it timed out;
        // both are correct, and neither may hang.
        assert!(outcome.is_none() || outcome.unwrap().is_ok());
    }
}
