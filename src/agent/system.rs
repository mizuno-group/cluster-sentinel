//! What the agent can learn about the machine it runs on.
//!
//! Everything that touches the real system goes through [`SystemInspector`].
//! That indirection exists so capability discovery can be tested against a
//! fileserver, a GPU node and a bare login node on a laptop with none of
//! those — a test suite that can only assert what the developer's machine
//! happens to have is worth very little.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// One mounted filesystem.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MountInfo {
    /// What is mounted (`server:/export`, `/dev/sda1`).
    pub source: String,
    /// Where it is mounted.
    pub target: String,
    /// Filesystem type (`nfs4`, `ext4`, `zfs`).
    pub fstype: String,
    /// Mount options, as reported.
    pub options: String,
}

impl MountInfo {
    /// Whether this is a network filesystem of the NFS family.
    pub fn is_nfs(&self) -> bool {
        self.fstype == "nfs" || self.fstype == "nfs4" || self.fstype.starts_with("nfs")
    }

    /// The server component of an NFS source, if there is one.
    pub fn nfs_server(&self) -> Option<&str> {
        if !self.is_nfs() {
            return None;
        }
        // `server:/export`, and IPv6 as `[::1]:/export`.
        let (server, _) = self.source.rsplit_once(":/")?;
        Some(server.trim_start_matches('[').trim_end_matches(']'))
    }

    /// Whether the mount is read-only.
    pub fn is_read_only(&self) -> bool {
        self.options.split(',').any(|o| o == "ro")
    }
}

/// A summary of the machine's resources, for comparison against what a
/// scheduler has been configured to expect.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct HardwareSummary {
    /// Logical CPUs.
    pub cpus: Option<u32>,
    /// Usable memory in MiB.
    pub memory_mb: Option<u64>,
    /// GPUs found.
    pub gpus: Option<u32>,
    /// Kernel release.
    pub kernel: Option<String>,
}

/// One address, and the interface it is configured on.
///
/// The interface name matters as much as the address. A host reporting where
/// it can be reached must not offer an address on `lo`, and a virtual bridge
/// address is a worse answer than a physical interface's -- neither of which
/// can be told from the address alone.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InterfaceAddress {
    /// Interface name, as the kernel reports it.
    pub interface: String,
    /// The address configured on it.
    pub address: String,
}

impl InterfaceAddress {
    /// Build one.
    pub fn new(interface: impl Into<String>, address: impl Into<String>) -> Self {
        Self {
            interface: interface.into(),
            address: address.into(),
        }
    }
}

/// Interface name prefixes that belong to something virtual.
///
/// Not an exhaustive list and not meant to be: it demotes the ones that show
/// up on ordinary cluster nodes, so that a container bridge does not outrank
/// the network the cluster actually runs on. Anything unrecognised is treated
/// as real, because being wrong in that direction only costs ordering, while
/// the reverse would hide a genuine interface.
const VIRTUAL_INTERFACE_PREFIXES: &[&str] = &[
    "docker",
    "br-",
    "veth",
    "virbr",
    "vboxnet",
    "tailscale",
    "zt",
    "cni",
    "flannel",
    "podman",
    "lxc",
    "lxd",
    "tun",
    "tap",
    "wg",
    "kube",
];

/// Whether an interface looks like something a hypervisor or container runtime
/// created rather than something cabled to a switch.
pub fn is_virtual_interface_name(name: &str) -> bool {
    VIRTUAL_INTERFACE_PREFIXES.iter().any(|prefix| name.starts_with(prefix))
}

/// Whether an address tells another machine nothing about how to reach this one.
fn is_unreachable_address(address: &str) -> bool {
    match address.parse::<std::net::IpAddr>() {
        Ok(std::net::IpAddr::V4(ip)) => ip.is_loopback() || ip.is_link_local() || ip.is_unspecified(),
        // `is_unicast_link_local` is the fe80::/10 test; those are only
        // meaningful with a scope identifier this does not carry.
        Ok(std::net::IpAddr::V6(ip)) => {
            ip.is_loopback() || ip.is_unspecified() || (ip.segments()[0] & 0xffc0) == 0xfe80
        }
        Err(_) => true,
    }
}

/// Order addresses so the first is the one most likely to reach this host.
///
/// The first is what a peer will actually probe, so getting it wrong makes a
/// healthy host look unreachable. Three rules, in order:
///
/// 1. **Drop what cannot be reached from elsewhere.** Loopback, link-local,
///    and -- the case that motivated this -- anything on the loopback
///    *interface*. A non-loopback address on `lo` is a real configuration
///    (WSL and some routers do it) and is reachable by nobody.
/// 2. **Physical interfaces before virtual ones.** A node with Docker
///    installed has a bridge address that sorts ahead of the cluster network
///    on many sites.
/// 3. **IPv4 before IPv6**, then a stable order, so the answer does not move
///    between restarts.
///
/// This is a heuristic and says so. `[agent] address` and `[agent] interface`
/// exist because a heuristic is not good enough when it is wrong.
pub fn rank_addresses(addresses: Vec<InterfaceAddress>) -> Vec<String> {
    let mut usable: Vec<InterfaceAddress> = addresses
        .into_iter()
        .filter(|entry| entry.interface != "lo" && !is_unreachable_address(&entry.address))
        .collect();

    usable.sort_by(|a, b| {
        let rank = |entry: &InterfaceAddress| {
            (
                is_virtual_interface_name(&entry.interface),
                entry.address.contains(':'),
                entry.interface.clone(),
                entry.address.clone(),
            )
        };
        rank(a).cmp(&rank(b))
    });

    let mut out: Vec<String> = usable.into_iter().map(|entry| entry.address).collect();
    out.dedup();
    out
}

/// The agent's window onto the local system.
pub trait SystemInspector: Send + Sync {
    /// Short host name.
    fn hostname(&self) -> Option<String>;
    /// Fully qualified name, when resolvable.
    fn fqdn(&self) -> Option<String>;
    /// Linux boot id; changes on reboot.
    fn boot_id(&self) -> Option<String>;
    /// Every address configured, with the interface it is on.
    fn interface_addresses(&self) -> Vec<InterfaceAddress>;

    /// Addresses this host answers on, best first.
    ///
    /// The first is what peers will probe, so the ordering is part of the
    /// answer rather than a presentation detail.
    fn addresses(&self) -> Vec<String> {
        rank_addresses(self.interface_addresses())
    }
    /// Whether a path exists.
    fn path_exists(&self, path: &Path) -> bool;
    /// Locate an executable on `PATH`.
    fn which(&self, program: &str) -> Option<PathBuf>;
    /// Read a configuration file, if it is readable.
    ///
    /// For configuration only. Reading a file on a network filesystem could
    /// block indefinitely, so callers must stay on local paths (SPEC.md §75).
    fn read_file(&self, path: &Path) -> Option<String>;
    /// Currently mounted filesystems.
    fn mounts(&self) -> Vec<MountInfo>;
    /// Hardware summary.
    fn hardware(&self) -> HardwareSummary;
}

/// The host's mount table, as opposed to this process's view of it.
///
/// **Not `/proc/self/mounts`.** The generated systemd unit sets
/// `ProtectSystem=strict`, which gives the service its own mount namespace
/// with the whole hierarchy remounted read-only. Reading its own table there
/// reports every filesystem on the host as `ro` -- so an agent under its own
/// hardening declared a perfectly writable NFS share read-only, describing
/// its sandbox and calling it the machine.
///
/// PID 1 is in the host's namespace, and its table is world-readable. In a
/// container PID 1 is the container's init, which is the right answer there
/// too.
pub const HOST_MOUNTS_PATH: &str = "/proc/1/mounts";

/// Read [`HOST_MOUNTS_PATH`], falling back to this process's own table.
///
/// Reading either is a read of kernel state: it does not touch the
/// filesystems it lists, so it is safe even when an NFS mount is hung.
/// Calling `df` or `stat` here would not be (SPEC.md §75).
pub fn read_host_mounts() -> Vec<MountInfo> {
    std::fs::read_to_string(HOST_MOUNTS_PATH)
        .or_else(|_| std::fs::read_to_string("/proc/self/mounts"))
        .map(|text| parse_mounts(&text))
        .unwrap_or_default()
}

/// The real Linux implementation.
#[derive(Debug, Clone, Default)]
pub struct LinuxInspector;

impl LinuxInspector {
    /// An inspector reading the live system.
    pub fn new() -> Self {
        Self
    }
}

impl SystemInspector for LinuxInspector {
    fn hostname(&self) -> Option<String> {
        hostname::get().ok().and_then(|h| h.into_string().ok()).map(|h| {
            // A host that calls itself `node-a.example.org` is still `node-a`;
            // the canonical name must not drift with DNS configuration.
            h.split('.').next().unwrap_or(&h).to_string()
        })
    }

    fn fqdn(&self) -> Option<String> {
        let full = hostname::get().ok().and_then(|h| h.into_string().ok())?;
        full.contains('.').then_some(full)
    }

    fn boot_id(&self) -> Option<String> {
        std::fs::read_to_string("/proc/sys/kernel/random/boot_id")
            .ok()
            .map(|id| id.trim().to_string())
            .filter(|id| !id.is_empty())
    }

    fn interface_addresses(&self) -> Vec<InterfaceAddress> {
        let Ok(interfaces) = if_addrs::get_if_addrs() else {
            return Vec::new();
        };
        // Everything is reported; `rank_addresses` decides what is usable.
        // Filtering here would throw away the interface name, which is the
        // only thing that tells a bridge from a cable.
        interfaces
            .into_iter()
            .map(|i| InterfaceAddress::new(i.name, i.addr.ip().to_string()))
            .collect()
    }

    fn path_exists(&self, path: &Path) -> bool {
        path.exists()
    }

    fn which(&self, program: &str) -> Option<PathBuf> {
        let path = std::env::var_os("PATH")?;
        std::env::split_paths(&path)
            .map(|dir| dir.join(program))
            .find(|candidate| candidate.is_file())
    }

    fn read_file(&self, path: &Path) -> Option<String> {
        std::fs::read_to_string(path).ok()
    }

    fn mounts(&self) -> Vec<MountInfo> {
        read_host_mounts()
    }

    fn hardware(&self) -> HardwareSummary {
        HardwareSummary {
            cpus: Some(
                std::thread::available_parallelism()
                    .map(|n| n.get() as u32)
                    .unwrap_or(0),
            )
            .filter(|n| *n > 0),
            memory_mb: read_meminfo_total_mb(),
            gpus: None,
            kernel: std::fs::read_to_string("/proc/sys/kernel/osrelease")
                .ok()
                .map(|k| k.trim().to_string()),
        }
    }
}

/// Parse a `/proc/*/mounts` table.
pub fn parse_mounts(text: &str) -> Vec<MountInfo> {
    text.lines()
        .filter_map(|line| {
            let mut fields = line.split_whitespace();
            let source = fields.next()?;
            let target = fields.next()?;
            let fstype = fields.next()?;
            let options = fields.next().unwrap_or("");
            Some(MountInfo {
                // The kernel escapes spaces and tabs in these fields.
                source: unescape_mount_field(source),
                target: unescape_mount_field(target),
                fstype: fstype.to_string(),
                options: options.to_string(),
            })
        })
        .collect()
}

/// Decode the kernel's octal escapes in a mount field.
fn unescape_mount_field(value: &str) -> String {
    if !value.contains('\\') {
        return value.to_string();
    }
    let bytes: Vec<char> = value.chars().collect();
    let mut out = String::with_capacity(value.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == '\\' && index + 3 < bytes.len() {
            let octal: String = bytes[index + 1..index + 4].iter().collect();
            if let Ok(byte) = u8::from_str_radix(&octal, 8) {
                out.push(byte as char);
                index += 4;
                continue;
            }
        }
        out.push(bytes[index]);
        index += 1;
    }
    out
}

fn read_meminfo_total_mb() -> Option<u64> {
    let text = std::fs::read_to_string("/proc/meminfo").ok()?;
    let line = text.lines().find(|l| l.starts_with("MemTotal:"))?;
    let kb: u64 = line.split_whitespace().nth(1)?.parse().ok()?;
    Some(kb / 1024)
}

/// An inspector that reports whatever a test says, for exercising discovery
/// against machines the test machine is not.
#[derive(Debug, Clone, Default)]
pub struct FakeInspector {
    /// Host name to report.
    pub hostname: Option<String>,
    /// FQDN to report.
    pub fqdn: Option<String>,
    /// Boot id to report.
    pub boot_id: Option<String>,
    /// Addresses to report, as if all on one physical interface.
    pub addresses: Vec<String>,
    /// Addresses with their interfaces, when a test cares which is which.
    pub interface_addresses: Vec<InterfaceAddress>,
    /// Paths that exist.
    pub paths: BTreeMap<String, bool>,
    /// Executables on `PATH`.
    pub programs: BTreeMap<String, PathBuf>,
    /// Mounted filesystems.
    pub mounts: Vec<MountInfo>,
    /// Readable files, by path.
    pub files: BTreeMap<String, String>,
    /// Hardware summary.
    pub hardware: HardwareSummary,
}

impl FakeInspector {
    /// An inspector reporting a bare host with nothing installed.
    pub fn bare() -> Self {
        Self {
            hostname: Some("test-host".into()),
            boot_id: Some("boot-1".into()),
            addresses: vec!["192.0.2.1".into()],
            hardware: HardwareSummary {
                cpus: Some(8),
                memory_mb: Some(16384),
                gpus: None,
                kernel: None,
            },
            ..Self::default()
        }
    }

    /// Builder: pretend a program is installed.
    pub fn with_program(mut self, program: &str) -> Self {
        self.programs
            .insert(program.to_string(), PathBuf::from(format!("/usr/bin/{program}")));
        self
    }

    /// Builder: pretend a path exists.
    pub fn with_path(mut self, path: &str) -> Self {
        self.paths.insert(path.to_string(), true);
        self
    }

    /// Builder: make a file readable with the given contents.
    pub fn with_file(mut self, path: &str, contents: &str) -> Self {
        self.paths.insert(path.to_string(), true);
        self.files.insert(path.to_string(), contents.to_string());
        self
    }

    /// Builder: add a mount.
    pub fn with_mount(mut self, source: &str, target: &str, fstype: &str) -> Self {
        self.mounts.push(MountInfo {
            source: source.into(),
            target: target.into(),
            fstype: fstype.into(),
            options: "rw".into(),
        });
        self
    }

    /// Builder: drop the default addresses, so only interfaces given here count.
    pub fn without_addresses(mut self) -> Self {
        self.addresses.clear();
        self
    }

    /// Builder: add an address on a named interface.
    pub fn with_interface_address(mut self, interface: &str, address: &str) -> Self {
        self.interface_addresses.push(InterfaceAddress::new(interface, address));
        self
    }

    /// Builder: set the host name.
    pub fn with_hostname(mut self, hostname: &str) -> Self {
        self.hostname = Some(hostname.to_string());
        self
    }

    /// Builder: set the boot id.
    pub fn with_boot_id(mut self, boot_id: &str) -> Self {
        self.boot_id = Some(boot_id.to_string());
        self
    }
}

impl SystemInspector for FakeInspector {
    fn hostname(&self) -> Option<String> {
        self.hostname.clone()
    }

    fn fqdn(&self) -> Option<String> {
        self.fqdn.clone()
    }

    fn boot_id(&self) -> Option<String> {
        self.boot_id.clone()
    }

    fn interface_addresses(&self) -> Vec<InterfaceAddress> {
        if !self.interface_addresses.is_empty() {
            return self.interface_addresses.clone();
        }
        // Addresses given without an interface are taken at face value: a test
        // that says "this host answers here" should not have its answer
        // reordered by a heuristic it did not ask for.
        self.addresses
            .iter()
            .map(|address| InterfaceAddress::new("eth0", address))
            .collect()
    }

    fn path_exists(&self, path: &Path) -> bool {
        self.paths.get(&path.display().to_string()).copied().unwrap_or(false)
    }

    fn which(&self, program: &str) -> Option<PathBuf> {
        self.programs.get(program).cloned()
    }

    fn read_file(&self, path: &Path) -> Option<String> {
        self.files.get(&path.display().to_string()).cloned()
    }

    fn mounts(&self) -> Vec<MountInfo> {
        self.mounts.clone()
    }

    fn hardware(&self) -> HardwareSummary {
        self.hardware.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn proc_mounts_parses_including_escaped_spaces() {
        let text = "\
/dev/sda1 / ext4 rw,relatime 0 0
fileserver:/export/home /home nfs4 rw,relatime,vers=4.2 0 0
/dev/sdb1 /mnt/with\\040space ext4 ro 0 0
";
        let mounts = parse_mounts(text);
        assert_eq!(mounts.len(), 3);
        assert_eq!(mounts[0].fstype, "ext4");
        assert_eq!(mounts[1].target, "/home");
        assert_eq!(
            mounts[2].target, "/mnt/with space",
            "the kernel escapes spaces as \\040"
        );
    }

    #[test]
    fn a_malformed_mount_line_is_skipped_not_fatal() {
        let mounts = parse_mounts("garbage\n/dev/sda1 / ext4 rw 0 0\n\n");
        assert_eq!(mounts.len(), 1);
    }

    #[test]
    fn nfs_mounts_are_recognised_across_versions() {
        for fstype in ["nfs", "nfs4"] {
            let mount = MountInfo {
                source: "fileserver:/export".into(),
                target: "/home".into(),
                fstype: fstype.into(),
                options: "rw".into(),
            };
            assert!(mount.is_nfs());
            assert_eq!(mount.nfs_server(), Some("fileserver"));
        }
    }

    #[test]
    fn a_local_filesystem_is_not_an_nfs_mount() {
        let mount = MountInfo {
            source: "/dev/sda1".into(),
            target: "/".into(),
            fstype: "ext4".into(),
            options: "rw".into(),
        };
        assert!(!mount.is_nfs());
        assert_eq!(mount.nfs_server(), None);
    }

    #[test]
    fn an_ipv6_nfs_server_is_extracted_without_its_brackets() {
        let mount = MountInfo {
            source: "[2001:db8::1]:/export".into(),
            target: "/home".into(),
            fstype: "nfs4".into(),
            options: "rw".into(),
        };
        assert_eq!(mount.nfs_server(), Some("2001:db8::1"));
    }

    #[test]
    fn a_read_only_mount_is_recognised_without_matching_a_substring() {
        let read_only = MountInfo {
            source: "s:/e".into(),
            target: "/h".into(),
            fstype: "nfs4".into(),
            options: "ro,relatime".into(),
        };
        assert!(read_only.is_read_only());

        let read_write = MountInfo {
            options: "rw,relatime".into(),
            ..read_only.clone()
        };
        assert!(!read_write.is_read_only());

        // `root` and `rootcontext` contain "ro" but do not mean read-only.
        let tricky = MountInfo {
            options: "rw,rootcontext=x".into(),
            ..read_only
        };
        assert!(!tricky.is_read_only());
    }

    #[test]
    fn the_real_inspector_reports_something_plausible_about_this_machine() {
        // Deliberately weak: this runs on developer laptops and CI containers
        // alike, so it asserts only what must be true anywhere.
        let inspector = LinuxInspector::new();
        assert!(inspector.hostname().is_some_and(|h| !h.is_empty()));
        assert!(
            !inspector.hostname().unwrap().contains('.'),
            "the short name must be short"
        );
        assert!(inspector.hardware().cpus.unwrap_or(0) > 0);
    }

    #[test]
    fn the_fake_inspector_reports_exactly_what_it_was_told() {
        let inspector = FakeInspector::bare()
            .with_hostname("fileserver-a")
            .with_program("nvidia-smi")
            .with_path("/etc/exports")
            .with_mount("fileserver:/export", "/home", "nfs4");

        assert_eq!(inspector.hostname().as_deref(), Some("fileserver-a"));
        assert!(inspector.which("nvidia-smi").is_some());
        assert!(inspector.which("zpool").is_none());
        assert!(inspector.path_exists(Path::new("/etc/exports")));
        assert!(!inspector.path_exists(Path::new("/etc/elsewhere")));
        assert_eq!(inspector.mounts().len(), 1);
    }

    #[test]
    fn a_faked_file_is_readable_and_counts_as_existing() {
        let inspector = FakeInspector::bare().with_file("/etc/slurm/slurm.conf", "SlurmctldHost=ctl-a\n");
        assert_eq!(
            inspector.read_file(Path::new("/etc/slurm/slurm.conf")).as_deref(),
            Some("SlurmctldHost=ctl-a\n")
        );
        assert!(inspector.path_exists(Path::new("/etc/slurm/slurm.conf")));
        assert_eq!(inspector.read_file(Path::new("/etc/nothing")), None);
    }
}

#[cfg(test)]
mod host_mounts_tests {
    use super::*;

    #[test]
    fn the_host_mount_table_is_read_not_this_process_s() {
        // ProtectSystem=strict gives the service a namespace with everything
        // remounted read-only; reading its own table there describes the
        // sandbox and calls it the host.
        assert_eq!(HOST_MOUNTS_PATH, "/proc/1/mounts");
    }

    #[test]
    fn a_writable_nfs_mount_is_not_read_only() {
        let line = "192.0.2.52:/tank /workspace/fs nfs4 rw,noatime,vers=4.2,hard,proto=tcp,sec=sys,addr=192.0.2.52 0 0";
        let mounts = parse_mounts(line);
        assert_eq!(mounts.len(), 1);
        assert!(mounts[0].is_nfs());
        assert!(!mounts[0].is_read_only(), "rw was read as read-only");
    }
}
