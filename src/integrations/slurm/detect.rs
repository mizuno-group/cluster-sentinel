//! Deciding whether a host actually participates in Slurm.
//!
//! The naive test — "is the `slurmd` binary installed?" — is wrong, and wrong
//! in a way that matters. Many distributions install the whole `slurm-wlm`
//! suite as one package, so every node ends up with `slurmctld` on disk. A host
//! that merely *has* the binary would claim `slurm.controller`, and control
//! plane probes would then run against every compute node in the cluster.
//!
//! So configuration is the evidence, not installation:
//!
//! * `slurm.controller` — this host is the configured `SlurmctldHost`.
//! * `slurm.compute`    — this host appears in a `NodeName` line.
//!
//! Configuration is also the *right* kind of evidence for a capability, because
//! it is stable while the daemon is not. A capability that vanished when a
//! service died would switch off the probe that detects the death.

use std::collections::BTreeMap;
use std::path::Path;

use crate::capability::{well_known, Capability, DiscoveryOutcome};

use super::hostlist;

/// Where `slurm.conf` normally lives.
pub const DEFAULT_CONFIG_PATHS: &[&str] = &["/etc/slurm/slurm.conf", "/etc/slurm-llnl/slurm.conf"];

/// The Slurm roles a host is configured for.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SlurmRoles {
    /// This host is the configured controller.
    pub controller: bool,
    /// This host is a configured compute node.
    pub compute: bool,
}

/// Read a `slurm.conf` and work out what `hostname` is configured to be.
///
/// Tolerant by design: unknown directives, comments and continuation lines must
/// not stop it finding the two things it is looking for.
pub fn roles_from_config(config: &str, hostname: &str) -> SlurmRoles {
    let mut roles = SlurmRoles::default();

    for line in config.lines() {
        let line = line.split('#').next().unwrap_or("").trim();
        if line.is_empty() {
            continue;
        }

        if let Some(value) = directive(line, "SlurmctldHost") {
            // `SlurmctldHost=name(address)` names a backup controller's address
            // in parentheses; only the name identifies the host.
            let name = value.split('(').next().unwrap_or(value).trim();
            if names_match(name, hostname) {
                roles.controller = true;
            }
            continue;
        }

        // Long superseded, but still present in older deployments.
        if let Some(value) = directive(line, "ControlMachine") {
            if names_match(value.trim(), hostname) {
                roles.controller = true;
            }
            continue;
        }

        if let Some(value) = directive(line, "NodeName") {
            let nodes = value.split_whitespace().next().unwrap_or("");
            if nodes == "DEFAULT" {
                continue;
            }
            if hostlist::expand(nodes).iter().any(|n| names_match(n, hostname)) {
                roles.compute = true;
            }
        }
    }

    roles
}

/// The value of `Key=` at the start of a line, if that is what this line is.
fn directive<'a>(line: &'a str, key: &str) -> Option<&'a str> {
    let rest = line.strip_prefix(key)?;
    let rest = rest.trim_start();
    rest.strip_prefix('=')
}

/// Compare host names, ignoring case and any domain suffix.
///
/// `slurm.conf` may name `node01` where the host calls itself
/// `node01.example.org`, and neither spelling is wrong.
fn names_match(a: &str, b: &str) -> bool {
    let short = |s: &str| s.split('.').next().unwrap_or(s).to_ascii_lowercase();
    short(a) == short(b)
}

/// Detect this host's Slurm capabilities.
///
/// With no readable `slurm.conf`, this claims **nothing**. That is deliberate:
/// a host genuinely participating in Slurm has a configuration, because
/// `slurmd` and `slurmctld` cannot start without one. Absence of the file is
/// therefore reasonable evidence that this host is not a Slurm node, whereas
/// presence of the binaries is not evidence that it is — distributions ship the
/// whole suite in one package, so a fileserver with the client tools installed
/// would otherwise claim to be a compute node and a control plane at once.
///
/// Nothing is lost by the conservative answer. A node the agent cannot classify
/// is still given `slurm.compute` by the Slurm inventory provider when
/// `scontrol` reports it, which also covers configless deployments, and an
/// operator can force the capability explicitly.
pub fn detect(
    hostname: &str,
    read_config: impl Fn(&Path) -> Option<String>,
    has_program: impl Fn(&str) -> bool,
) -> BTreeMap<Capability, DiscoveryOutcome> {
    let config = DEFAULT_CONFIG_PATHS
        .iter()
        .find_map(|path| read_config(Path::new(path)));

    let (controller, compute) = match config {
        Some(config) => {
            let roles = roles_from_config(&config, hostname);
            // Configuration says this host has the role; the binary must also
            // be there for it to actually do anything.
            (
                roles.controller && has_program("slurmctld"),
                roles.compute && has_program("slurmd"),
            )
        }
        None => (false, false),
    };

    let outcome = |present: bool| {
        if present {
            DiscoveryOutcome::Detected
        } else {
            DiscoveryOutcome::NotDetected
        }
    };

    BTreeMap::from([
        (Capability::new(well_known::SLURM_CONTROLLER), outcome(controller)),
        (Capability::new(well_known::SLURM_COMPUTE), outcome(compute)),
    ])
}

#[cfg(test)]
mod tests {
    use super::*;

    const CONFIG: &str = "\
# A testbed configuration.
ClusterName=example
SlurmctldHost=ctl-a
SlurmUser=slurm

NodeName=DEFAULT CPUs=1 State=UNKNOWN
NodeName=compute[01-03] RealMemory=256 State=UNKNOWN
NodeName=special-a RealMemory=512 State=UNKNOWN

PartitionName=compute Nodes=compute[01-03] Default=YES State=UP
";

    #[test]
    fn the_configured_controller_is_recognised() {
        assert_eq!(
            roles_from_config(CONFIG, "ctl-a"),
            SlurmRoles {
                controller: true,
                compute: false
            }
        );
    }

    #[test]
    fn a_node_inside_a_compressed_hostlist_is_recognised() {
        for hostname in ["compute01", "compute02", "compute03"] {
            assert_eq!(
                roles_from_config(CONFIG, hostname),
                SlurmRoles {
                    controller: false,
                    compute: true
                },
                "{hostname}"
            );
        }
    }

    #[test]
    fn a_node_named_individually_is_recognised() {
        assert_eq!(
            roles_from_config(CONFIG, "special-a"),
            SlurmRoles {
                controller: false,
                compute: true
            }
        );
    }

    #[test]
    fn a_host_the_configuration_does_not_mention_gets_no_role() {
        assert_eq!(roles_from_config(CONFIG, "fileserver-a"), SlurmRoles::default());
        assert_eq!(roles_from_config(CONFIG, "compute04"), SlurmRoles::default());
    }

    #[test]
    fn the_default_node_template_is_not_taken_for_a_host_name() {
        assert_eq!(roles_from_config(CONFIG, "DEFAULT"), SlurmRoles::default());
    }

    #[test]
    fn a_domain_suffix_does_not_prevent_a_match() {
        // slurm.conf says `ctl-a`; the host calls itself `ctl-a.example.org`.
        assert!(roles_from_config(CONFIG, "ctl-a.example.org").controller);
        assert!(
            roles_from_config(CONFIG, "COMPUTE01").compute,
            "and case must not matter"
        );
    }

    #[test]
    fn a_backup_controllers_address_annotation_is_ignored() {
        let config = "SlurmctldHost=ctl-a(10.0.0.1)\nSlurmctldHost=ctl-b(10.0.0.2)\n";
        assert!(roles_from_config(config, "ctl-a").controller);
        assert!(
            roles_from_config(config, "ctl-b").controller,
            "an HA pair has two controllers"
        );
        assert!(
            !roles_from_config(config, "10.0.0.1").controller,
            "an address is not a host name"
        );
    }

    #[test]
    fn the_superseded_control_machine_directive_still_works() {
        assert!(roles_from_config("ControlMachine=old-ctl\n", "old-ctl").controller);
    }

    #[test]
    fn comments_and_whitespace_do_not_confuse_the_parser() {
        let config = "  SlurmctldHost = ctl-a   # the controller\n#SlurmctldHost=decoy\n";
        assert!(roles_from_config(config, "ctl-a").controller);
        assert!(!roles_from_config(config, "decoy").controller);
    }

    #[test]
    fn one_host_can_be_both_controller_and_compute() {
        let config = "SlurmctldHost=all-in-one\nNodeName=all-in-one CPUs=1\n";
        assert_eq!(
            roles_from_config(config, "all-in-one"),
            SlurmRoles {
                controller: true,
                compute: true
            }
        );
    }

    #[test]
    fn installing_the_package_everywhere_does_not_make_every_node_a_controller() {
        // The bug this module exists to prevent: `slurm-wlm` ships every
        // daemon, so binary presence would give a compute node the controller
        // capability and start control plane probes against it.
        let found = detect("compute01", |_| Some(CONFIG.to_string()), |_| true);

        assert_eq!(
            found[&Capability::new(well_known::SLURM_COMPUTE)],
            DiscoveryOutcome::Detected
        );
        assert_eq!(
            found[&Capability::new(well_known::SLURM_CONTROLLER)],
            DiscoveryOutcome::NotDetected,
            "having slurmctld on disk does not make this the control plane"
        );
    }

    #[test]
    fn the_real_controller_is_still_recognised() {
        let found = detect("ctl-a", |_| Some(CONFIG.to_string()), |_| true);
        assert_eq!(
            found[&Capability::new(well_known::SLURM_CONTROLLER)],
            DiscoveryOutcome::Detected
        );
        assert_eq!(
            found[&Capability::new(well_known::SLURM_COMPUTE)],
            DiscoveryOutcome::NotDetected
        );
    }

    #[test]
    fn a_configured_role_without_the_binary_is_not_claimed() {
        let found = detect("ctl-a", |_| Some(CONFIG.to_string()), |_| false);
        assert_eq!(
            found[&Capability::new(well_known::SLURM_CONTROLLER)],
            DiscoveryOutcome::NotDetected
        );
    }

    #[test]
    fn without_a_readable_config_nothing_is_claimed() {
        // A fileserver with the Slurm client tools installed must not report
        // itself as a compute node, let alone as a control plane. Distributions
        // ship the whole suite in one package, so binary presence says nothing.
        // Nothing is lost: the Slurm inventory provider still supplies the
        // capability for nodes scontrol actually reports, which also covers
        // configless deployments, and an operator can force it explicitly.
        let found = detect("fileserver-a", |_| None, |_| true);
        assert_eq!(
            found[&Capability::new(well_known::SLURM_COMPUTE)],
            DiscoveryOutcome::NotDetected
        );
        assert_eq!(
            found[&Capability::new(well_known::SLURM_CONTROLLER)],
            DiscoveryOutcome::NotDetected
        );
    }

    #[test]
    fn a_host_with_no_slurm_at_all_claims_nothing() {
        let found = detect("fileserver-a", |_| None, |_| false);
        assert!(found.values().all(|o| *o == DiscoveryOutcome::NotDetected));
    }
}
