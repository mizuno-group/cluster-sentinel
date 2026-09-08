//! Every probe Sentinel knows about, with its compiled-in schedule.
//!
//! One list, derived from the probes themselves rather than written out beside
//! them, so it cannot drift. It is what `sentinel config init` writes into a
//! generated file, what `sentinel doctor` prints, and what config validation
//! checks a probe id against before telling an operator they have made a typo.

use std::sync::Arc;

use super::{Probe, ProbeDefinition};

/// One probe, as an operator sees it.
pub struct CatalogEntry {
    /// The probe's compiled-in definition.
    pub definition: ProbeDefinition,
    /// What it measures, in one line.
    pub description: &'static str,
    /// Anything an operator should know before changing its schedule.
    pub caution: Option<&'static str>,
}

impl CatalogEntry {
    /// The probe id.
    pub fn id(&self) -> &str {
        self.definition.id.as_str()
    }
}

/// Build one entry from a probe instance, so the schedule comes from the probe.
fn entry(probe: Arc<dyn Probe>, description: &'static str, caution: Option<&'static str>) -> CatalogEntry {
    CatalogEntry {
        definition: probe.definition().clone(),
        description,
        caution,
    }
}

/// Every probe, in the order an operator is likely to think about them:
/// cheapest and most frequent first.
pub fn catalog() -> Vec<CatalogEntry> {
    use super::{gpu, host, journal, network, nfs, sentinel_rpc, ssh, systemd};

    vec![
        entry(
            Arc::new(network::TcpProbe::reachability()),
            "TCP 到達性。応答拒否も「パケットが返った」証拠として扱う",
            Some("到達性診断の土台。長くすると host 障害の検出全体が遅くなる"),
        ),
        entry(
            Arc::new(sentinel_rpc::SentinelAgentProbe::new()),
            "agent の health endpoint。remote からのみ実行",
            Some("「agent だけ落ちた」と「host が落ちた」の区別に使う"),
        ),
        entry(Arc::new(systemd::SystemdProbe::new()), "systemd unit の状態", None),
        entry(
            Arc::new(host::HostMetricsProbe::new()),
            "load / memory / filesystem / uptime",
            None,
        ),
        entry(Arc::new(ssh::SshProbe::new()), "sshd の応答", None),
        entry(Arc::new(gpu::NvidiaGpuProbe::new()), "GPU の枚数・温度・メモリ", None),
        entry(Arc::new(nfs::NfsPortProbe::new()), "NFS server の port 応答", None),
        entry(
            Arc::new(nfs::NfsMountProbe::new()),
            "mount の一覧と状態。/proc を読むだけで、hang 中でも安全",
            None,
        ),
        entry(
            Arc::new(nfs::NfsClientIoProbe::new()),
            "mount 先への実 I/O",
            Some("同時実行は 1 に固定。blocking syscall を積み上げないため引き上げ不可"),
        ),
        entry(
            Arc::new(journal::JournalProbe::new()),
            "kernel / service event（OOM・I/O error・hung task 等）",
            Some("同時実行は 1 に固定。引き上げ不可"),
        ),
        entry(Arc::new(nfs::NfsExportsProbe::new()), "NFS server の export 一覧", None),
    ]
}

/// Every known probe id.
pub fn ids() -> Vec<String> {
    catalog().into_iter().map(|e| e.definition.id.to_string()).collect()
}

/// Whether a probe id is one Sentinel knows.
pub fn is_known(probe_id: &str) -> bool {
    catalog().iter().any(|e| e.id() == probe_id)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_probe_in_the_tree_is_listed() {
        // The catalog is what an operator is shown and what validation checks
        // against. A probe missing from it is one nobody can configure and
        // whose id would be reported as a typo.
        let ids = ids();
        for expected in [
            super::super::network::PROBE_ID,
            super::super::sentinel_rpc::PROBE_ID,
            super::super::systemd::PROBE_ID,
            super::super::host::PROBE_ID,
            super::super::ssh::PROBE_ID,
            super::super::gpu::PROBE_ID,
            super::super::journal::PROBE_ID,
            super::super::nfs::PROBE_SERVER_PORT,
            super::super::nfs::PROBE_SERVER_EXPORTS,
            super::super::nfs::PROBE_CLIENT_MOUNT,
            super::super::nfs::PROBE_CLIENT_IO,
        ] {
            assert!(ids.iter().any(|id| id == expected), "{expected} is not in the catalog");
        }
    }

    #[test]
    fn the_catalog_has_no_duplicates() {
        let mut ids = ids();
        let before = ids.len();
        ids.sort();
        ids.dedup();
        assert_eq!(ids.len(), before);
    }

    #[test]
    fn the_schedules_come_from_the_probes_themselves() {
        // Not transcribed beside them, where they could drift.
        let entry = catalog()
            .into_iter()
            .find(|e| e.id() == super::super::journal::PROBE_ID)
            .expect("journal probe");
        let probe = super::super::journal::JournalProbe::new();
        assert_eq!(entry.definition, *probe.definition());
    }

    #[test]
    fn probes_that_pin_their_concurrency_say_why() {
        for entry in catalog() {
            if entry.definition.max_outstanding == 1 {
                assert!(
                    entry.caution.is_some(),
                    "{} pins concurrency without explaining it",
                    entry.id()
                );
            }
        }
    }

    #[test]
    fn an_unknown_probe_id_is_not_known() {
        assert!(is_known("network.tcp"));
        assert!(!is_known("network.tpc"));
    }
}
