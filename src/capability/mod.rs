//! Capabilities decide which probes apply to an entity.
//!
//! Probes are enabled by **capability**, never by role (SPEC.md §15). A role is
//! an operator-facing label used for grouping and for suggesting defaults; if
//! `role=fileserver` is deleted but `storage.nfs.server` is still present, NFS
//! monitoring must keep working (SPEC.md §179).

mod resolve;

pub use resolve::{resolve_capabilities, CapabilityOverride, DiscoveryOutcome, Resolution, ResolutionReason};

use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};

/// A namespaced capability name such as `storage.nfs.server`.
///
/// Capabilities are open-ended strings on purpose: adding a new storage or
/// interconnect technology must not require a change to a core enum
/// (SPEC.md §57).
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Capability(String);

impl Capability {
    /// Build a capability from a namespaced name.
    pub fn new(name: impl Into<String>) -> Self {
        Self(name.into())
    }

    /// The capability name.
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Whether this capability sits in the given namespace,
    /// e.g. `storage.nfs.client` is in `storage` and in `storage.nfs`.
    pub fn in_namespace(&self, namespace: &str) -> bool {
        self.0 == namespace
            || (self.0.len() > namespace.len()
                && self.0.starts_with(namespace)
                && self.0.as_bytes()[namespace.len()] == b'.')
    }
}

impl From<&str> for Capability {
    fn from(s: &str) -> Self {
        Capability::new(s)
    }
}

impl std::fmt::Display for Capability {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// The set of capabilities held by an entity.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct CapabilitySet(BTreeSet<Capability>);

impl CapabilitySet {
    /// An empty set.
    pub fn new() -> Self {
        Self(BTreeSet::new())
    }

    /// Add a capability.
    pub fn insert(&mut self, capability: impl Into<Capability>) -> bool {
        self.0.insert(capability.into())
    }

    /// Remove a capability.
    pub fn remove(&mut self, capability: &Capability) -> bool {
        self.0.remove(capability)
    }

    /// Whether the set holds this capability.
    pub fn contains(&self, capability: &Capability) -> bool {
        self.0.contains(capability)
    }

    /// Whether the set holds a capability given by name.
    pub fn has(&self, name: &str) -> bool {
        self.0.contains(&Capability::new(name))
    }

    /// Whether the set holds every one of `required`.
    pub fn has_all(&self, required: &[Capability]) -> bool {
        required.iter().all(|c| self.contains(c))
    }

    /// Whether any capability sits in the given namespace.
    pub fn any_in_namespace(&self, namespace: &str) -> bool {
        self.0.iter().any(|c| c.in_namespace(namespace))
    }

    /// Iterate over the capabilities in stable order.
    pub fn iter(&self) -> impl Iterator<Item = &Capability> {
        self.0.iter()
    }

    /// Number of capabilities.
    pub fn len(&self) -> usize {
        self.0.len()
    }

    /// Whether the set is empty.
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

impl<T: Into<Capability>> FromIterator<T> for CapabilitySet {
    fn from_iter<I: IntoIterator<Item = T>>(iter: I) -> Self {
        Self(iter.into_iter().map(Into::into).collect())
    }
}

impl<'a> IntoIterator for &'a CapabilitySet {
    type Item = &'a Capability;
    type IntoIter = std::collections::btree_set::Iter<'a, Capability>;

    fn into_iter(self) -> Self::IntoIter {
        self.0.iter()
    }
}

/// Well-known capability names.
///
/// These are *constants for convenience and typo-safety only*. The core never
/// enumerates them exhaustively and an integration may introduce any other
/// name (SPEC.md §181).
pub mod well_known {
    /// Host-level metrics: uptime, load, memory, pressure.
    pub const HOST_METRICS: &str = "host.metrics";
    /// Reachable by ICMP echo.
    pub const NETWORK_ICMP: &str = "network.icmp";
    /// Reachable by TCP.
    pub const NETWORK_TCP: &str = "network.tcp";
    /// Runs an SSH server.
    pub const SSH_SERVER: &str = "ssh.server";
    /// Managed by systemd.
    pub const SYSTEMD: &str = "systemd";
    /// Runs `slurmctld`.
    pub const SLURM_CONTROLLER: &str = "slurm.controller";
    /// Runs `slurmd`.
    pub const SLURM_COMPUTE: &str = "slurm.compute";
    /// Has NVIDIA GPUs.
    pub const GPU_NVIDIA: &str = "gpu.nvidia";
    /// Has local storage worth monitoring.
    pub const STORAGE_LOCAL: &str = "storage.local";
    /// Mounts NFS.
    pub const STORAGE_NFS_CLIENT: &str = "storage.nfs.client";
    /// Exports NFS.
    pub const STORAGE_NFS_SERVER: &str = "storage.nfs.server";
    /// Has ZFS pools.
    pub const STORAGE_ZFS: &str = "storage.zfs";
    /// Exposes SMART data.
    pub const STORAGE_SMART: &str = "storage.smart";
    /// Journal is readable.
    pub const JOURNAL_READ: &str = "journal.read";
    /// May observe other entities.
    pub const OBSERVER_PEER: &str = "observer.peer";
    /// May send notifications when the controller cannot.
    pub const NOTIFICATION_FALLBACK: &str = "notification.fallback";
    /// Runs a Sentinel agent exposing the health RPC.
    pub const SENTINEL_AGENT: &str = "sentinel.agent";
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn namespace_matching_respects_dot_boundaries() {
        let cap = Capability::new("storage.nfs.client");
        assert!(cap.in_namespace("storage"));
        assert!(cap.in_namespace("storage.nfs"));
        assert!(cap.in_namespace("storage.nfs.client"));
        assert!(!cap.in_namespace("stor"));
        assert!(!cap.in_namespace("storage.nfs.clientx"));
        assert!(!cap.in_namespace("storage.nf"));
    }

    #[test]
    fn set_queries_work_by_name_and_namespace() {
        let set: CapabilitySet = ["host.metrics", "storage.nfs.client", "slurm.compute"]
            .into_iter()
            .collect();
        assert!(set.has("slurm.compute"));
        assert!(!set.has("slurm.controller"));
        assert!(set.any_in_namespace("storage"));
        assert!(!set.any_in_namespace("gpu"));
        assert!(set.has_all(&[Capability::new("host.metrics"), Capability::new("slurm.compute")]));
        assert!(!set.has_all(&[Capability::new("host.metrics"), Capability::new("gpu.nvidia")]));
    }

    #[test]
    fn iteration_order_is_stable() {
        let set: CapabilitySet = ["z.b", "a.a", "m.c"].into_iter().collect();
        let names: Vec<_> = set.iter().map(|c| c.as_str().to_string()).collect();
        assert_eq!(names, ["a.a", "m.c", "z.b"]);
    }
}
