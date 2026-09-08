//! Runtime capability discovery (SPEC.md §18, §39).
//!
//! The agent works out what this machine *can do*, and the answer decides which
//! probes run. Two properties matter:
//!
//! * Discovery reports both presence and **absence**. "I looked and there is no
//!   `slurmd`" is a different answer from "I did not look", and only the former
//!   may override a role hint.
//! * Nothing here executes anything. Detection reads the filesystem and
//!   `/proc`, which is safe even when an NFS mount is hung.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use crate::capability::{
    resolve_capabilities, well_known, Capability, CapabilityOverride, DiscoveryOutcome, Resolution,
};

use super::system::{HardwareSummary, SystemInspector};

/// What a role suggests, when nothing better is known.
///
/// These are *hints only*. A role never enables a probe on its own; see
/// `capability::resolve` for the precedence that enforces this.
pub fn role_hints(role: &str) -> BTreeSet<Capability> {
    let names: &[&str] = match role {
        "controller" => &[well_known::SLURM_CONTROLLER, well_known::OBSERVER_PEER],
        "compute" => &[well_known::SLURM_COMPUTE],
        "fileserver" => &[well_known::STORAGE_NFS_SERVER, well_known::OBSERVER_PEER],
        "observer" => &[well_known::OBSERVER_PEER],
        "login" => &[well_known::SSH_SERVER],
        _ => &[],
    };
    names.iter().map(|n| Capability::new(*n)).collect()
}

/// Where `sshd` keeps its configuration.
pub const SSHD_CONFIG_PATH: &str = "/etc/ssh/sshd_config";

/// Work out which port `sshd` listens on.
///
/// Read from the configuration rather than assumed, because a cluster that
/// moved SSH off 22 would otherwise have every host reported as SSH-down: the
/// probe would knock on a closed port and be told, correctly, that nothing is
/// there. An operator can still override this explicitly.
///
/// `ListenAddress host:port` also carries a port, and takes effect the same
/// way, so both spellings are honoured.
pub fn detect_ssh_port(inspector: &dyn SystemInspector) -> Option<u16> {
    let config = inspector.read_file(Path::new(SSHD_CONFIG_PATH))?;
    parse_sshd_port(&config)
}

/// Parse the effective port out of an `sshd_config`.
pub fn parse_sshd_port(config: &str) -> Option<u16> {
    let mut from_listen_address = None;

    for line in config.lines() {
        let line = line.split('#').next().unwrap_or("").trim();
        if line.is_empty() {
            continue;
        }

        let mut fields = line.split_whitespace();
        let Some(keyword) = fields.next() else {
            continue;
        };

        // `Port` wins outright; the first one is the one to report, since a
        // probe only needs one working way in.
        if keyword.eq_ignore_ascii_case("Port") {
            if let Some(port) = fields.next().and_then(|p| p.parse().ok()) {
                return Some(port);
            }
        }

        // `ListenAddress host:port` or `ListenAddress [v6]:port`.
        if keyword.eq_ignore_ascii_case("ListenAddress") && from_listen_address.is_none() {
            if let Some(value) = fields.next() {
                let tail = value.rsplit_once(':').map(|(_, port)| port);
                from_listen_address = tail.and_then(|p| p.parse().ok());
            }
        }
    }

    from_listen_address
}

/// Detect what this machine can do.
///
/// Returns an outcome per capability considered — including the negatives,
/// which is what lets a real look override a role's guess.
pub fn detect(inspector: &dyn SystemInspector) -> BTreeMap<Capability, DiscoveryOutcome> {
    let mut found = BTreeMap::new();
    let mut record = |name: &str, present: bool| {
        found.insert(
            Capability::new(name),
            if present {
                DiscoveryOutcome::Detected
            } else {
                DiscoveryOutcome::NotDetected
            },
        );
    };

    // An agent that is running can always report on its own host.
    record(well_known::HOST_METRICS, true);
    record(well_known::SENTINEL_AGENT, true);
    record(well_known::NETWORK_TCP, true);

    let systemd = inspector.path_exists(Path::new("/run/systemd/system"));
    record(well_known::SYSTEMD, systemd);
    record(
        well_known::JOURNAL_READ,
        systemd && inspector.which("journalctl").is_some(),
    );

    record(
        well_known::SSH_SERVER,
        inspector.path_exists(Path::new("/etc/ssh/sshd_config")) || inspector.which("sshd").is_some(),
    );

    record(well_known::GPU_NVIDIA, inspector.which("nvidia-smi").is_some());

    let mounts = inspector.mounts();
    record(well_known::STORAGE_NFS_CLIENT, mounts.iter().any(|m| m.is_nfs()));
    record(
        well_known::STORAGE_NFS_SERVER,
        inspector.path_exists(Path::new("/etc/exports")) || inspector.which("exportfs").is_some(),
    );
    record(well_known::STORAGE_ZFS, inspector.which("zpool").is_some());
    record(well_known::STORAGE_SMART, inspector.which("smartctl").is_some());
    record(well_known::STORAGE_LOCAL, true);

    // Slurm decides its own capabilities: "the binary is installed" is not the
    // same as "this host is configured to run it", and only the integration
    // knows how to tell the difference.
    found.extend(crate::integrations::slurm::detect::detect(
        &inspector.hostname().unwrap_or_default(),
        |path| inspector.read_file(path),
        |program| inspector.which(program).is_some(),
    ));

    found
}

/// Detect, then apply the operator's overrides and any role hints.
pub fn resolve(
    inspector: &dyn SystemInspector,
    overrides: &BTreeMap<String, CapabilityOverride>,
    roles: &[String],
) -> Resolution {
    let detected = detect(inspector);
    let overrides: BTreeMap<Capability, CapabilityOverride> = overrides
        .iter()
        .map(|(name, value)| (Capability::new(name), *value))
        .collect();
    let hints: BTreeSet<Capability> = roles.iter().flat_map(|role| role_hints(role)).collect();

    resolve_capabilities(&detected, &overrides, &hints)
}

/// Hardware summary, with the GPU count filled in from what was detected.
pub fn hardware(inspector: &dyn SystemInspector, gpu_count: Option<u32>) -> HardwareSummary {
    HardwareSummary {
        gpus: gpu_count,
        ..inspector.hardware()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::system::FakeInspector;

    fn outcome(found: &BTreeMap<Capability, DiscoveryOutcome>, name: &str) -> Option<DiscoveryOutcome> {
        found.get(&Capability::new(name)).copied()
    }

    #[test]
    fn a_standard_sshd_config_yields_no_explicit_port() {
        // Most hosts say nothing, and the default applies.
        assert_eq!(parse_sshd_port("PermitRootLogin no\nX11Forwarding yes\n"), None);
        assert_eq!(parse_sshd_port(""), None);
    }

    #[test]
    fn a_non_standard_port_is_read_from_the_configuration() {
        // A cluster that moved SSH off 22 would otherwise have every host
        // reported as SSH-down.
        assert_eq!(parse_sshd_port("Port 2222\n"), Some(2222));
        assert_eq!(
            parse_sshd_port("  port  2222  \n"),
            Some(2222),
            "the keyword is case-insensitive"
        );
    }

    #[test]
    fn a_commented_out_port_is_ignored() {
        // Distributions ship `#Port 22`, and reading it would be reporting a
        // setting nobody made.
        assert_eq!(parse_sshd_port("#Port 2222\nPermitRootLogin no\n"), None);
        assert_eq!(parse_sshd_port("Port 2222 # moved for the audit\n"), Some(2222));
    }

    #[test]
    fn the_first_port_wins_when_several_are_configured() {
        // sshd listens on all of them; a probe only needs one way in.
        assert_eq!(parse_sshd_port("Port 2222\nPort 2223\n"), Some(2222));
    }

    #[test]
    fn a_port_from_listen_address_is_honoured() {
        assert_eq!(parse_sshd_port("ListenAddress 10.0.0.1:2222\n"), Some(2222));
        assert_eq!(parse_sshd_port("ListenAddress [2001:db8::1]:2222\n"), Some(2222));
    }

    #[test]
    fn a_listen_address_without_a_port_yields_nothing() {
        assert_eq!(parse_sshd_port("ListenAddress 10.0.0.1\n"), None);
    }

    #[test]
    fn an_explicit_port_beats_a_listen_address() {
        assert_eq!(parse_sshd_port("ListenAddress 10.0.0.1:2223\nPort 2222\n"), Some(2222));
    }

    #[test]
    fn the_port_is_read_from_the_hosts_own_sshd_config() {
        let inspector = FakeInspector::bare().with_file(SSHD_CONFIG_PATH, "Port 2222\n");
        assert_eq!(detect_ssh_port(&inspector), Some(2222));

        assert_eq!(detect_ssh_port(&FakeInspector::bare()), None, "no config, no claim");
    }

    #[test]
    fn a_bare_host_reports_only_what_any_running_agent_can_do() {
        let found = detect(&FakeInspector::bare());

        assert_eq!(
            outcome(&found, well_known::HOST_METRICS),
            Some(DiscoveryOutcome::Detected)
        );
        assert_eq!(
            outcome(&found, well_known::SENTINEL_AGENT),
            Some(DiscoveryOutcome::Detected)
        );
        assert_eq!(
            outcome(&found, well_known::SLURM_COMPUTE),
            Some(DiscoveryOutcome::NotDetected)
        );
        assert_eq!(
            outcome(&found, well_known::GPU_NVIDIA),
            Some(DiscoveryOutcome::NotDetected)
        );
    }

    #[test]
    fn absence_is_reported_explicitly_rather_than_omitted() {
        // The difference between "looked, not there" and "did not look" is what
        // lets discovery overrule a role hint.
        let found = detect(&FakeInspector::bare());
        assert_eq!(
            outcome(&found, well_known::STORAGE_NFS_SERVER),
            Some(DiscoveryOutcome::NotDetected),
            "a negative must be recorded, not left out"
        );
    }

    #[test]
    fn a_compute_node_is_recognised_by_what_is_configured() {
        let inspector = FakeInspector::bare()
            .with_hostname("compute01")
            .with_program("slurmd")
            .with_program("nvidia-smi")
            .with_file(
                "/etc/slurm/slurm.conf",
                "SlurmctldHost=ctl-a\nNodeName=compute[01-03] CPUs=1\n",
            );

        let found = detect(&inspector);
        assert_eq!(
            outcome(&found, well_known::SLURM_COMPUTE),
            Some(DiscoveryOutcome::Detected)
        );
        assert_eq!(
            outcome(&found, well_known::GPU_NVIDIA),
            Some(DiscoveryOutcome::Detected)
        );
        assert_eq!(
            outcome(&found, well_known::SLURM_CONTROLLER),
            Some(DiscoveryOutcome::NotDetected),
            "this host is a node, not the control plane"
        );
    }

    #[test]
    fn installed_slurm_binaries_alone_do_not_make_a_host_a_slurm_node() {
        // A fileserver with the client tools installed and no slurm.conf.
        // Claiming a Slurm role here would start scheduler probes against a
        // machine that has nothing to do with the scheduler.
        let inspector = FakeInspector::bare()
            .with_hostname("fileserver-a")
            .with_program("slurmd")
            .with_program("slurmctld");

        let found = detect(&inspector);
        assert_eq!(
            outcome(&found, well_known::SLURM_COMPUTE),
            Some(DiscoveryOutcome::NotDetected)
        );
        assert_eq!(
            outcome(&found, well_known::SLURM_CONTROLLER),
            Some(DiscoveryOutcome::NotDetected)
        );
    }

    #[test]
    fn a_fileserver_is_recognised_by_its_exports() {
        let found = detect(&FakeInspector::bare().with_path("/etc/exports").with_program("zpool"));
        assert_eq!(
            outcome(&found, well_known::STORAGE_NFS_SERVER),
            Some(DiscoveryOutcome::Detected)
        );
        assert_eq!(
            outcome(&found, well_known::STORAGE_ZFS),
            Some(DiscoveryOutcome::Detected)
        );
    }

    #[test]
    fn an_nfs_client_is_recognised_by_its_mounts() {
        let found = detect(&FakeInspector::bare().with_mount("fileserver:/export", "/home", "nfs4"));
        assert_eq!(
            outcome(&found, well_known::STORAGE_NFS_CLIENT),
            Some(DiscoveryOutcome::Detected)
        );
        assert_eq!(
            outcome(&found, well_known::STORAGE_NFS_SERVER),
            Some(DiscoveryOutcome::NotDetected),
            "mounting NFS does not make a host a server"
        );
    }

    #[test]
    fn a_local_only_host_is_not_taken_for_an_nfs_client() {
        let found = detect(&FakeInspector::bare().with_mount("/dev/sda1", "/", "ext4"));
        assert_eq!(
            outcome(&found, well_known::STORAGE_NFS_CLIENT),
            Some(DiscoveryOutcome::NotDetected)
        );
    }

    #[test]
    fn journal_access_needs_both_systemd_and_journalctl() {
        let neither = detect(&FakeInspector::bare());
        assert_eq!(
            outcome(&neither, well_known::JOURNAL_READ),
            Some(DiscoveryOutcome::NotDetected)
        );

        let only_binary = detect(&FakeInspector::bare().with_program("journalctl"));
        assert_eq!(
            outcome(&only_binary, well_known::JOURNAL_READ),
            Some(DiscoveryOutcome::NotDetected)
        );

        let both = detect(
            &FakeInspector::bare()
                .with_path("/run/systemd/system")
                .with_program("journalctl"),
        );
        assert_eq!(
            outcome(&both, well_known::JOURNAL_READ),
            Some(DiscoveryOutcome::Detected)
        );
    }

    #[test]
    fn a_role_alone_never_enables_a_probe_discovery_ruled_out() {
        // SPEC.md §15 and §179, at the point where it actually matters.
        let resolution = resolve(&FakeInspector::bare(), &BTreeMap::new(), &["fileserver".into()]);
        assert!(
            !resolution.enabled.has(well_known::STORAGE_NFS_SERVER),
            "this host exports nothing; calling it a fileserver must not start NFS probes"
        );
    }

    #[test]
    fn a_capability_survives_the_role_label_being_removed() {
        // The converse of the previous test, and the one SPEC.md §179 names.
        let inspector = FakeInspector::bare().with_path("/etc/exports");
        let resolution = resolve(&inspector, &BTreeMap::new(), &[]);
        assert!(resolution.enabled.has(well_known::STORAGE_NFS_SERVER));
    }

    #[test]
    fn an_operator_can_force_a_capability_discovery_missed() {
        let overrides = BTreeMap::from([(well_known::STORAGE_NFS_SERVER.to_string(), CapabilityOverride::Force)]);
        let resolution = resolve(&FakeInspector::bare(), &overrides, &[]);
        assert!(resolution.enabled.has(well_known::STORAGE_NFS_SERVER));
    }

    #[test]
    fn an_operator_can_disable_a_capability_discovery_found() {
        let overrides = BTreeMap::from([(well_known::GPU_NVIDIA.to_string(), CapabilityOverride::Disable)]);
        let resolution = resolve(&FakeInspector::bare().with_program("nvidia-smi"), &overrides, &[]);
        assert!(!resolution.enabled.has(well_known::GPU_NVIDIA));
    }

    #[test]
    fn a_role_hint_applies_where_discovery_cannot_look() {
        // `observer.peer` is a policy decision, not a property of the machine,
        // so nothing detects it and the role hint is the only signal.
        let resolution = resolve(&FakeInspector::bare(), &BTreeMap::new(), &["observer".into()]);
        assert!(resolution.enabled.has(well_known::OBSERVER_PEER));
    }

    #[test]
    fn an_unknown_role_contributes_nothing_rather_than_failing() {
        assert!(role_hints("wharf-master").is_empty());
        let resolution = resolve(&FakeInspector::bare(), &BTreeMap::new(), &["wharf-master".into()]);
        assert!(
            resolution.enabled.has(well_known::HOST_METRICS),
            "discovery still works"
        );
    }

    #[test]
    fn the_gpu_count_is_carried_into_the_hardware_summary() {
        let summary = hardware(&FakeInspector::bare(), Some(4));
        assert_eq!(summary.gpus, Some(4));
        assert_eq!(summary.cpus, Some(8), "the rest of the summary is preserved");
    }
}
