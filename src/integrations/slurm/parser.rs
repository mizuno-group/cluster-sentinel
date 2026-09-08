//! Parsers for `scontrol` output.
//!
//! Two properties matter more than completeness here (IMPLEMENTATION.md §78,
//! §155):
//!
//! * **Unknown fields must not break anything.** Slurm adds and reorders fields
//!   between versions. Every key is kept in a raw map; the typed fields are a
//!   convenience layered on top.
//! * **A malformed line must not lose the rest of the output.** One
//!   unparseable node must not cost visibility of the other fifteen.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

use super::hostlist;

/// Split one `-o` (one-record-per-line) row into its key/value pairs.
///
/// Values may contain spaces (`OS=Linux 5.15.0 #1 SMP ...`), so a plain split
/// on whitespace is wrong. A new field begins only where whitespace is followed
/// by an identifier and `=`.
///
/// `Reason` is special-cased to consume the rest of the line: `scontrol` emits
/// it last, and reason text is free-form operator prose that may itself contain
/// `=`.
pub fn parse_record(line: &str) -> BTreeMap<String, String> {
    let mut fields = BTreeMap::new();
    let bytes = line.as_bytes();
    let boundaries = field_boundaries(line);

    for (index, &(start, key_end)) in boundaries.iter().enumerate() {
        let key = &line[start..key_end];
        let value_start = key_end + 1; // skip '='

        if key == "Reason" {
            fields.insert(key.to_string(), line[value_start..].trim().to_string());
            break;
        }

        let value_end = boundaries.get(index + 1).map(|&(next, _)| next).unwrap_or(bytes.len());
        let value = line[value_start..value_end].trim();
        fields.insert(key.to_string(), value.to_string());
    }

    fields
}

/// Byte offsets of each `Key=` in the line, as `(key_start, key_end)`.
fn field_boundaries(line: &str) -> Vec<(usize, usize)> {
    let bytes = line.as_bytes();
    let mut boundaries = Vec::new();
    let mut index = 0usize;

    while index < bytes.len() {
        let at_field_start = index == 0 || bytes[index - 1].is_ascii_whitespace();
        if at_field_start && is_key_start(bytes[index]) {
            let mut end = index;
            while end < bytes.len() && is_key_char(bytes[end]) {
                end += 1;
            }
            if end < bytes.len() && bytes[end] == b'=' {
                boundaries.push((index, end));
                index = end + 1;
                continue;
            }
        }
        index += 1;
    }

    boundaries
}

fn is_key_start(byte: u8) -> bool {
    byte.is_ascii_alphabetic() || byte == b'_'
}

fn is_key_char(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || byte == b'_'
}

/// The base state of a Slurm node.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NodeBaseState {
    /// Idle and available.
    Idle,
    /// Fully allocated.
    Allocated,
    /// Partly allocated.
    Mixed,
    /// Marked down by the controller.
    Down,
    /// In an error state.
    Error,
    /// Configured but not yet present.
    Future,
    /// Unknown to the controller.
    Unknown,
    /// A state this build does not recognise, kept verbatim.
    Other(String),
}

impl NodeBaseState {
    /// Parse a base state name.
    pub fn parse(value: &str) -> Self {
        match value.to_ascii_uppercase().as_str() {
            "IDLE" => NodeBaseState::Idle,
            "ALLOCATED" | "ALLOC" => NodeBaseState::Allocated,
            "MIXED" | "MIX" => NodeBaseState::Mixed,
            "DOWN" => NodeBaseState::Down,
            "ERROR" | "ERR" => NodeBaseState::Error,
            "FUTURE" | "FUTR" => NodeBaseState::Future,
            "UNKNOWN" | "UNK" => NodeBaseState::Unknown,
            other => NodeBaseState::Other(other.to_string()),
        }
    }

    /// Whether the scheduler considers this state usable for work.
    pub fn is_schedulable(&self) -> bool {
        matches!(
            self,
            NodeBaseState::Idle | NodeBaseState::Allocated | NodeBaseState::Mixed
        )
    }
}

/// A state flag modifying the base state.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NodeStateFlag {
    /// Draining or drained: the scheduler will not place new work here.
    Drain,
    /// The controller is not hearing from `slurmd` (the `*` suffix).
    NotResponding,
    /// Jobs are finishing.
    Completing,
    /// Under a maintenance reservation (the `$` suffix).
    Maintenance,
    /// Powered down to save energy (the `~` suffix).
    PowerSave,
    /// Powering up (the `#` suffix).
    PoweringUp,
    /// Powering down (the `%` suffix).
    PoweringDown,
    /// Registered with resources that disagree with the configuration.
    InvalidRegistration,
    /// Reserved.
    Reserved,
    /// Scheduled to reboot.
    Reboot,
    /// Planned for a future allocation.
    Planned,
    /// A flag this build does not recognise, kept verbatim.
    Other(String),
}

impl NodeStateFlag {
    /// Parse a flag name.
    pub fn parse(value: &str) -> Self {
        match value.to_ascii_uppercase().as_str() {
            "DRAIN" | "DRAINING" | "DRAINED" | "DRNG" | "DRAIN+" => NodeStateFlag::Drain,
            "NOT_RESPONDING" | "NO_RESPOND" => NodeStateFlag::NotResponding,
            "COMPLETING" | "COMP" => NodeStateFlag::Completing,
            "MAINTENANCE" | "MAINT" => NodeStateFlag::Maintenance,
            "POWER_SAVE" | "POWERED_DOWN" | "POWER_DOWN" => NodeStateFlag::PowerSave,
            "POWERING_UP" | "POWER_UP" => NodeStateFlag::PoweringUp,
            "POWERING_DOWN" => NodeStateFlag::PoweringDown,
            "INVALID_REG" | "INVAL" => NodeStateFlag::InvalidRegistration,
            "RESERVED" | "RESV" => NodeStateFlag::Reserved,
            "REBOOT" | "REBOOT_REQUESTED" | "REBOOT_ISSUED" => NodeStateFlag::Reboot,
            "PLANNED" | "PLND" => NodeStateFlag::Planned,
            other => NodeStateFlag::Other(other.to_string()),
        }
    }
}

/// A parsed `State=` value: one base state plus any number of flags.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NodeState {
    /// The base state.
    pub base: NodeBaseState,
    /// Modifying flags.
    pub flags: BTreeSet<NodeStateFlag>,
    /// The original string, kept for evidence.
    pub raw: String,
}

impl NodeState {
    /// Parse a `State=` value such as `IDLE+DRAIN` or `DOWN*`.
    pub fn parse(value: &str) -> Self {
        let raw = value.trim().to_string();
        let mut flags = BTreeSet::new();

        // Suffix characters encode flags that are not spelled out.
        let mut body = raw.as_str();
        while let Some(last) = body.chars().next_back() {
            let flag = match last {
                '*' => NodeStateFlag::NotResponding,
                '~' => NodeStateFlag::PowerSave,
                '#' => NodeStateFlag::PoweringUp,
                '%' => NodeStateFlag::PoweringDown,
                '$' => NodeStateFlag::Maintenance,
                '@' => NodeStateFlag::Reboot,
                _ => break,
            };
            flags.insert(flag);
            body = &body[..body.len() - last.len_utf8()];
        }

        let mut parts = body.split('+').filter(|p| !p.is_empty());
        let base = parts.next().map(NodeBaseState::parse).unwrap_or(NodeBaseState::Unknown);
        for part in parts {
            flags.insert(NodeStateFlag::parse(part));
        }

        Self { base, flags, raw }
    }

    /// Whether the node carries a flag.
    pub fn has(&self, flag: &NodeStateFlag) -> bool {
        self.flags.contains(flag)
    }

    /// Whether the node is drained or draining.
    pub fn is_drained(&self) -> bool {
        self.has(&NodeStateFlag::Drain)
    }

    /// Whether the controller has lost contact with `slurmd`.
    pub fn is_not_responding(&self) -> bool {
        self.has(&NodeStateFlag::NotResponding)
    }

    /// Whether Slurm will schedule work here.
    ///
    /// This is a statement about the *scheduler's* willingness, not about the
    /// host's health. A perfectly healthy machine can be unschedulable.
    pub fn is_schedulable(&self) -> bool {
        self.base.is_schedulable() && !self.is_drained() && !self.is_not_responding()
    }
}

/// A node as Slurm describes it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SlurmNode {
    /// Slurm's `NodeName`. Not assumed to equal the hostname (SPEC.md §38).
    pub node_name: String,
    /// `NodeHostName`, when Slurm reports one.
    pub node_host_name: Option<String>,
    /// `NodeAddr`, when Slurm reports one.
    pub node_addr: Option<String>,
    /// Parsed state.
    pub state: NodeState,
    /// `Reason`, when the node is down or drained.
    pub reason: Option<String>,
    /// `ReasonTime`, verbatim.
    pub reason_time: Option<String>,
    /// Partitions this node belongs to.
    pub partitions: Vec<String>,
    /// Configured CPUs.
    pub cpu_total: Option<u32>,
    /// Configured memory in MiB.
    pub real_memory_mb: Option<u64>,
    /// `Gres`, verbatim (`gpu:rtx3090:2`).
    pub gres: Option<String>,
    /// `CfgTRES`, verbatim.
    pub cfg_tres: Option<String>,
    /// `AllocTRES`, verbatim.
    pub alloc_tres: Option<String>,
    /// `BootTime`, verbatim.
    pub boot_time: Option<String>,
    /// Every field as reported, including ones this build does not know.
    pub raw: BTreeMap<String, String>,
}

impl SlurmNode {
    /// Parse one `scontrol show nodes -o` line, if it names a node.
    pub fn parse_line(line: &str) -> Option<Self> {
        let fields = parse_record(line);
        let node_name = fields.get("NodeName")?.clone();
        if node_name.is_empty() {
            return None;
        }

        Some(Self {
            state: NodeState::parse(fields.get("State").map(String::as_str).unwrap_or("UNKNOWN")),
            node_host_name: optional(&fields, "NodeHostName"),
            node_addr: optional(&fields, "NodeAddr"),
            reason: optional(&fields, "Reason").filter(|r| r != "none"),
            reason_time: optional(&fields, "ReasonTime"),
            partitions: fields
                .get("Partitions")
                .map(|p| hostlist::expand(p))
                .unwrap_or_default(),
            cpu_total: fields.get("CPUTot").and_then(|v| v.parse().ok()),
            real_memory_mb: fields.get("RealMemory").and_then(|v| v.parse().ok()),
            gres: optional(&fields, "Gres"),
            cfg_tres: optional(&fields, "CfgTRES"),
            alloc_tres: optional(&fields, "AllocTRES"),
            boot_time: optional(&fields, "BootTime"),
            node_name,
            raw: fields,
        })
    }

    /// The host name to reach this node at, falling back through
    /// `NodeHostName` to `NodeName`.
    ///
    /// The fallback is deliberate and documented: Slurm omits `NodeHostName`
    /// when it equals `NodeName`, and treating the two as interchangeable
    /// everywhere else would bake in an assumption SPEC.md §38 forbids.
    pub fn host_name(&self) -> &str {
        self.node_host_name.as_deref().unwrap_or(&self.node_name)
    }

    /// Number of GPUs Slurm has been configured to expect, from `Gres`.
    pub fn configured_gpu_count(&self) -> Option<u32> {
        let gres = self.gres.as_deref()?;
        if gres == "(null)" {
            return Some(0);
        }
        // Drop any `(IDX:0-7)` annotation first: it contains both colons and
        // digits, and would otherwise be mistaken for the count.
        let gres = strip_parenthesised(gres);

        let mut total = 0u32;
        let mut found = false;
        for item in gres.split(',') {
            let mut parts = item.split(':');
            if parts.next() != Some("gpu") {
                continue;
            }
            // `gpu:2` or `gpu:model:2`; the count is the trailing number, and a
            // bare `gpu` with no count means one.
            let count = parts.next_back().and_then(|p| p.parse::<u32>().ok()).unwrap_or(1);
            total += count;
            found = true;
        }
        found.then_some(total)
    }
}

/// Parse the full output of `scontrol show nodes -o`.
///
/// Lines that do not describe a node are skipped rather than treated as an
/// error.
pub fn parse_nodes(output: &str) -> Vec<SlurmNode> {
    output.lines().filter_map(SlurmNode::parse_line).collect()
}

/// A partition as Slurm describes it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SlurmPartition {
    /// Partition name.
    pub name: String,
    /// Expanded node membership.
    pub nodes: Vec<String>,
    /// `State`, verbatim (`UP`, `DOWN`, `DRAIN`, `INACTIVE`).
    pub state: Option<String>,
    /// Whether this is the default partition.
    pub is_default: bool,
    /// Every field as reported.
    pub raw: BTreeMap<String, String>,
}

impl SlurmPartition {
    /// Parse one `scontrol show partitions -o` line, if it names a partition.
    pub fn parse_line(line: &str) -> Option<Self> {
        let fields = parse_record(line);
        let name = fields.get("PartitionName")?.clone();
        if name.is_empty() {
            return None;
        }
        Some(Self {
            nodes: fields.get("Nodes").map(|n| hostlist::expand(n)).unwrap_or_default(),
            state: optional(&fields, "State"),
            is_default: fields
                .get("Default")
                .map(|d| d.eq_ignore_ascii_case("YES"))
                .unwrap_or(false),
            name,
            raw: fields,
        })
    }
}

/// Parse the full output of `scontrol show partitions -o`.
pub fn parse_partitions(output: &str) -> Vec<SlurmPartition> {
    output.lines().filter_map(SlurmPartition::parse_line).collect()
}

/// One controller's reachability, from `scontrol ping`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ControllerPing {
    /// The host `scontrol` named.
    pub host: String,
    /// Whether it answered.
    pub up: bool,
    /// `primary`, `backup`, or whatever Slurm reported.
    pub role: String,
}

/// Parse `scontrol ping`.
///
/// Two output shapes exist across Slurm versions:
///
/// ```text
/// Slurmctld(primary) at ctl-a is UP
/// Slurmctld(primary/backup) at ctl-a/ctl-b are UP/DOWN
/// ```
pub fn parse_ping(output: &str) -> Vec<ControllerPing> {
    let mut pings = Vec::new();

    for line in output.lines() {
        let line = line.trim();
        if !line.starts_with("Slurmctld") {
            continue;
        }
        let Some(roles) = line
            .split_once('(')
            .and_then(|(_, rest)| rest.split_once(')'))
            .map(|(r, _)| r)
        else {
            continue;
        };
        let Some(after_at) = line.split(" at ").nth(1) else {
            continue;
        };
        // ` is UP` / ` are UP/DOWN`
        let (hosts, statuses) = match after_at.split_once(" is ") {
            Some(parts) => parts,
            None => match after_at.split_once(" are ") {
                Some(parts) => parts,
                None => continue,
            },
        };

        let roles: Vec<&str> = roles.split('/').collect();
        let hosts: Vec<&str> = hosts.trim().split('/').collect();
        let statuses: Vec<&str> = statuses.trim().split('/').collect();

        for (index, host) in hosts.iter().enumerate() {
            let host = host.trim();
            // Slurm writes an unconfigured backup as `(NULL)`.
            if host.is_empty() || host.eq_ignore_ascii_case("(null)") {
                continue;
            }
            pings.push(ControllerPing {
                host: host.to_string(),
                up: statuses.get(index).is_some_and(|s| s.trim().eq_ignore_ascii_case("UP")),
                role: roles.get(index).unwrap_or(&"unknown").trim().to_string(),
            });
        }
    }

    pings
}

/// Remove `(...)` sections, which Slurm uses for index annotations.
fn strip_parenthesised(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    let mut depth = 0usize;
    for ch in value.chars() {
        match ch {
            '(' => depth += 1,
            ')' => depth = depth.saturating_sub(1),
            _ if depth == 0 => out.push(ch),
            _ => {}
        }
    }
    out
}

fn optional(fields: &BTreeMap<String, String>, key: &str) -> Option<String> {
    fields
        .get(key)
        .map(|v| v.trim())
        .filter(|v| !v.is_empty() && *v != "(null)" && *v != "N/A" && *v != "None")
        .map(str::to_string)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_record_splits_on_key_boundaries_not_on_whitespace() {
        let fields = parse_record("NodeName=n1 CPUTot=64 OS=Linux 5.15.0 #1 SMP Tue Nov 14 UTC 2023 RealMemory=257000");
        assert_eq!(fields.get("NodeName").unwrap(), "n1");
        assert_eq!(fields.get("CPUTot").unwrap(), "64");
        assert_eq!(fields.get("OS").unwrap(), "Linux 5.15.0 #1 SMP Tue Nov 14 UTC 2023");
        assert_eq!(fields.get("RealMemory").unwrap(), "257000");
    }

    #[test]
    fn an_equals_sign_inside_a_value_does_not_start_a_new_field() {
        let fields = parse_record("NodeName=n1 CfgTRES=cpu=64,mem=257000M,gres/gpu=2 AllocTRES= Weight=1");
        assert_eq!(fields.get("CfgTRES").unwrap(), "cpu=64,mem=257000M,gres/gpu=2");
        assert_eq!(fields.get("AllocTRES").unwrap(), "");
        assert_eq!(fields.get("Weight").unwrap(), "1");
    }

    #[test]
    fn a_free_form_reason_survives_intact() {
        let fields = parse_record("NodeName=n1 State=DOWN Reason=Not responding [slurm@2026-09-01T10:00:00]");
        assert_eq!(
            fields.get("Reason").unwrap(),
            "Not responding [slurm@2026-09-01T10:00:00]"
        );
    }

    #[test]
    fn a_reason_containing_an_equals_sign_is_not_split() {
        let fields = parse_record("NodeName=n1 Reason=NHC: check failed opt=value more text");
        assert_eq!(fields.get("Reason").unwrap(), "NHC: check failed opt=value more text");
    }

    #[test]
    fn unknown_fields_are_kept_rather_than_dropped() {
        let node = SlurmNode::parse_line("NodeName=n1 State=IDLE SomeFutureSlurmField=42").expect("parse");
        assert_eq!(node.raw.get("SomeFutureSlurmField").unwrap(), "42");
    }

    #[test]
    fn a_line_without_a_node_name_is_skipped_not_fatal() {
        assert!(SlurmNode::parse_line("").is_none());
        assert!(SlurmNode::parse_line("this is not a record").is_none());
        assert!(SlurmNode::parse_line("PartitionName=debug Nodes=n1").is_none());
    }

    #[test]
    fn one_malformed_line_does_not_lose_the_others() {
        let output = "NodeName=n1 State=IDLE\ngarbage\n\nNodeName=n2 State=DOWN\n";
        let nodes = parse_nodes(output);
        assert_eq!(nodes.len(), 2);
        assert_eq!(nodes[1].node_name, "n2");
    }

    #[test]
    fn base_states_parse_including_short_forms() {
        assert_eq!(NodeState::parse("IDLE").base, NodeBaseState::Idle);
        assert_eq!(NodeState::parse("ALLOCATED").base, NodeBaseState::Allocated);
        assert_eq!(NodeState::parse("MIX").base, NodeBaseState::Mixed);
        assert_eq!(NodeState::parse("DOWN").base, NodeBaseState::Down);
    }

    #[test]
    fn an_unknown_state_is_preserved_instead_of_failing() {
        let state = NodeState::parse("SOME_NEW_STATE");
        assert_eq!(state.base, NodeBaseState::Other("SOME_NEW_STATE".into()));
        assert!(!state.is_schedulable());
    }

    #[test]
    fn plus_separated_flags_parse() {
        let state = NodeState::parse("IDLE+DRAIN");
        assert_eq!(state.base, NodeBaseState::Idle);
        assert!(state.is_drained());
        assert!(
            !state.is_schedulable(),
            "a drained node is not schedulable even though it is idle"
        );
    }

    #[test]
    fn suffix_flags_parse() {
        let state = NodeState::parse("DOWN*");
        assert_eq!(state.base, NodeBaseState::Down);
        assert!(state.is_not_responding());

        assert!(NodeState::parse("IDLE~").has(&NodeStateFlag::PowerSave));
        assert!(NodeState::parse("IDLE#").has(&NodeStateFlag::PoweringUp));
        assert!(NodeState::parse("MIXED$").has(&NodeStateFlag::Maintenance));
        assert!(NodeState::parse("IDLE%").has(&NodeStateFlag::PoweringDown));
    }

    #[test]
    fn combined_flags_and_suffixes_parse_together() {
        let state = NodeState::parse("DOWN+DRAIN+INVALID_REG*");
        assert_eq!(state.base, NodeBaseState::Down);
        assert!(state.is_drained());
        assert!(state.is_not_responding());
        assert!(state.has(&NodeStateFlag::InvalidRegistration));
        assert_eq!(state.raw, "DOWN+DRAIN+INVALID_REG*");
    }

    #[test]
    fn an_idle_node_is_schedulable_and_a_drained_one_is_not() {
        assert!(NodeState::parse("IDLE").is_schedulable());
        assert!(NodeState::parse("MIXED").is_schedulable());
        assert!(NodeState::parse("ALLOCATED").is_schedulable());
        assert!(!NodeState::parse("IDLE+DRAIN").is_schedulable());
        assert!(!NodeState::parse("DOWN").is_schedulable());
        assert!(!NodeState::parse("IDLE*").is_schedulable());
    }

    #[test]
    fn a_node_name_that_differs_from_the_host_name_is_kept_separate() {
        // SPEC.md §38: these must never be assumed equal.
        let node =
            SlurmNode::parse_line("NodeName=n1 NodeHostName=physical-a NodeAddr=192.0.2.5 State=IDLE").expect("parse");
        assert_eq!(node.node_name, "n1");
        assert_eq!(node.host_name(), "physical-a");
        assert_eq!(node.node_addr.as_deref(), Some("192.0.2.5"));
    }

    #[test]
    fn the_host_name_falls_back_to_the_node_name_when_slurm_omits_it() {
        let node = SlurmNode::parse_line("NodeName=n1 State=IDLE").expect("parse");
        assert_eq!(node.host_name(), "n1");
        assert_eq!(node.node_host_name, None);
    }

    #[test]
    fn placeholder_values_become_absent_rather_than_literal_strings() {
        let node = SlurmNode::parse_line("NodeName=n1 State=IDLE Gres=(null) AllocTRES= Reason=none").expect("parse");
        assert_eq!(node.gres, None);
        assert_eq!(node.reason, None);
        assert_eq!(node.alloc_tres, None);
    }

    #[test]
    fn gpu_counts_come_out_of_the_gres_field() {
        let with_model = SlurmNode::parse_line("NodeName=n1 State=IDLE Gres=gpu:rtx3090:2").expect("parse");
        assert_eq!(with_model.configured_gpu_count(), Some(2));

        let bare = SlurmNode::parse_line("NodeName=n1 State=IDLE Gres=gpu:4").expect("parse");
        assert_eq!(bare.configured_gpu_count(), Some(4));

        let indexed = SlurmNode::parse_line("NodeName=n1 State=IDLE Gres=gpu:a100:8(IDX:0-7)").expect("parse");
        assert_eq!(indexed.configured_gpu_count(), Some(8));

        let mixed = SlurmNode::parse_line("NodeName=n1 State=IDLE Gres=gpu:2,mps:100").expect("parse");
        assert_eq!(mixed.configured_gpu_count(), Some(2));
    }

    #[test]
    fn a_node_without_gres_reports_no_gpu_expectation_at_all() {
        // Distinct from "expects zero": a node with no Gres field tells us
        // nothing, and must not produce a mismatch diagnosis.
        let node = SlurmNode::parse_line("NodeName=n1 State=IDLE").expect("parse");
        assert_eq!(node.configured_gpu_count(), None);

        let explicit_none = SlurmNode::parse_line("NodeName=n1 State=IDLE Gres=(null)").expect("parse");
        assert_eq!(
            explicit_none.configured_gpu_count(),
            None,
            "(null) is normalised away before the count"
        );
    }

    #[test]
    fn partition_membership_expands_a_hostlist() {
        let partition =
            SlurmPartition::parse_line("PartitionName=compute Nodes=n[01-03] State=UP Default=YES").expect("parse");
        assert_eq!(partition.name, "compute");
        assert_eq!(partition.nodes, ["n01", "n02", "n03"]);
        assert_eq!(partition.state.as_deref(), Some("UP"));
        assert!(partition.is_default);
    }

    #[test]
    fn a_node_belonging_to_several_partitions_lists_them_all() {
        let node = SlurmNode::parse_line("NodeName=n1 State=IDLE Partitions=compute,debug").expect("parse");
        assert_eq!(node.partitions, ["compute", "debug"]);
    }

    #[test]
    fn a_single_controller_ping_parses() {
        let pings = parse_ping("Slurmctld(primary) at ctl-a is UP");
        assert_eq!(pings.len(), 1);
        assert_eq!(
            pings[0],
            ControllerPing {
                host: "ctl-a".into(),
                up: true,
                role: "primary".into()
            }
        );
    }

    #[test]
    fn a_primary_and_backup_ping_parses_both_sides() {
        let pings = parse_ping("Slurmctld(primary/backup) at ctl-a/ctl-b are UP/DOWN");
        assert_eq!(pings.len(), 2);
        assert!(pings[0].up);
        assert!(!pings[1].up);
        assert_eq!(pings[1].host, "ctl-b");
        assert_eq!(pings[1].role, "backup");
    }

    #[test]
    fn an_unconfigured_backup_is_omitted_rather_than_reported_down() {
        let pings = parse_ping("Slurmctld(primary/backup) at ctl-a/(NULL) are UP/DOWN");
        assert_eq!(pings.len(), 1);
        assert_eq!(pings[0].host, "ctl-a");
    }

    #[test]
    fn a_down_controller_parses_as_down() {
        let pings = parse_ping("Slurmctld(primary) at ctl-a is DOWN");
        assert_eq!(pings.len(), 1);
        assert!(!pings[0].up);
    }

    #[test]
    fn unrecognised_ping_output_yields_nothing_rather_than_a_false_positive() {
        assert!(parse_ping("").is_empty());
        assert!(parse_ping("slurm_load_ctl_conf error: Connection refused").is_empty());
        assert!(parse_ping("Slurmctld nonsense").is_empty());
    }
}
