//! What each capability means, and how it is decided.
//!
//! A capability gates probes, so "why is this host not being monitored the way
//! I expected" is nearly always "why does it not have that capability". The
//! answer is a specific test against the machine -- a file, a program on
//! `PATH`, a line in a configuration -- and until now that test lived only in
//! the detection code.
//!
//! Written beside the detection rather than derived from it, which means the
//! two can drift; a test asserts that every capability the agent can report
//! appears here, so at least nothing is missing.

use crate::capability::well_known;

/// One capability, as an operator needs to understand it.
pub struct CapabilityEntry {
    /// The capability name.
    pub name: &'static str,
    /// What having it means.
    pub meaning: &'static str,
    /// The test the agent performs on the host.
    pub detection: &'static str,
}

/// Every capability the agent can report, with how it decides.
pub fn catalog() -> Vec<CapabilityEntry> {
    use CapabilityEntry as C;
    vec![
        C {
            name: well_known::HOST_METRICS,
            meaning: "load, memory, filesystem usage and uptime can be read here",
            detection: "always, on any host running an agent",
        },
        C {
            name: well_known::SENTINEL_AGENT,
            meaning: "an agent is running and answering on its health port",
            detection: "always, on any host running an agent",
        },
        C {
            name: well_known::NETWORK_TCP,
            meaning: "this host can open TCP connections to others",
            detection: "always, on any host running an agent",
        },
        C {
            name: well_known::SYSTEMD,
            meaning: "systemd units can be inspected here",
            detection: "the directory /run/systemd/system exists",
        },
        C {
            name: well_known::JOURNAL_READ,
            meaning: "kernel and service events can be collected here",
            detection: "systemd is present AND journalctl is on PATH",
        },
        C {
            name: well_known::SSH_SERVER,
            meaning: "this host is expected to answer SSH",
            detection: "/etc/ssh/sshd_config exists OR sshd is on PATH",
        },
        C {
            name: well_known::GPU_NVIDIA,
            meaning: "NVIDIA GPUs can be queried here",
            detection: "nvidia-smi is on PATH",
        },
        C {
            name: well_known::STORAGE_LOCAL,
            meaning: "local filesystems can be inspected here",
            detection: "always, on any host running an agent",
        },
        C {
            name: well_known::STORAGE_NFS_CLIENT,
            meaning: "this host mounts NFS and those mounts are worth watching",
            detection: "the host's mount table lists at least one NFS mount",
        },
        C {
            name: well_known::STORAGE_NFS_SERVER,
            meaning: "this host is expected to serve NFS",
            detection: "/etc/exports exists OR exportfs is on PATH",
        },
        C {
            name: well_known::STORAGE_ZFS,
            meaning: "ZFS pools exist here",
            detection: "zpool is on PATH",
        },
        C {
            name: well_known::STORAGE_SMART,
            meaning: "disk health can be queried here",
            detection: "smartctl is on PATH",
        },
        C {
            name: well_known::SLURM_CONTROLLER,
            meaning: "this host is Slurm's control plane",
            detection: "slurm.conf names it as SlurmctldHost AND slurmctld is on PATH",
        },
        C {
            name: well_known::SLURM_COMPUTE,
            meaning: "this host is a Slurm compute node",
            detection: "slurm.conf names it as a NodeName AND slurmd is on PATH",
        },
        C {
            name: well_known::OBSERVER_PEER,
            meaning: "this host watches others on the controller's behalf",
            detection: "never detected: a policy decision, set by a role hint or [capabilities]",
        },
        C {
            name: well_known::NETWORK_ICMP,
            meaning: "ICMP may be used from here",
            detection: "never detected: set by [capabilities] if a site permits it",
        },
        C {
            name: well_known::NOTIFICATION_FALLBACK,
            meaning: "this host may send notifications if the controller cannot",
            detection: "never detected: set by [capabilities]",
        },
    ]
}

/// The entry for one capability, if it is one Sentinel knows.
pub fn describe(name: &str) -> Option<CapabilityEntry> {
    catalog().into_iter().find(|entry| entry.name == name)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_well_known_capability_is_described() {
        // The list is written beside the detection code rather than derived
        // from it, so this is what stops the two drifting apart.
        let described: Vec<&str> = catalog().iter().map(|e| e.name).collect();
        for name in [
            well_known::HOST_METRICS,
            well_known::NETWORK_ICMP,
            well_known::NETWORK_TCP,
            well_known::SSH_SERVER,
            well_known::SYSTEMD,
            well_known::SLURM_CONTROLLER,
            well_known::SLURM_COMPUTE,
            well_known::GPU_NVIDIA,
            well_known::STORAGE_LOCAL,
            well_known::STORAGE_NFS_CLIENT,
            well_known::STORAGE_NFS_SERVER,
            well_known::STORAGE_ZFS,
            well_known::STORAGE_SMART,
            well_known::JOURNAL_READ,
            well_known::OBSERVER_PEER,
            well_known::NOTIFICATION_FALLBACK,
            well_known::SENTINEL_AGENT,
        ] {
            assert!(described.contains(&name), "{name} has no description");
        }
    }

    #[test]
    fn a_capability_that_is_never_detected_says_so() {
        // Otherwise an operator reads "not present on this host" and goes
        // looking for a missing program that was never the point.
        for name in [well_known::OBSERVER_PEER, well_known::NETWORK_ICMP] {
            let entry = describe(name).expect("described");
            assert!(entry.detection.contains("never detected"), "{}", entry.detection);
        }
    }

    #[test]
    fn an_unknown_capability_has_no_description() {
        assert!(describe("something.invented").is_none());
    }
}
