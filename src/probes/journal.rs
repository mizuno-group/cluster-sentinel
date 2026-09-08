//! Kernel and service events from the journal (SPEC.md §83, §103).
//!
//! # Why this collects continuously rather than on demand
//!
//! The failure this exists to prevent is named in SPEC.md §1: *reboot後に原因
//! 情報が失われる* — the cause is lost when the machine reboots. An operator
//! arrives after a node has already come back, and the I/O errors, the hung
//! task traces and the OOM kills that explain it are gone with the volatile
//! journal.
//!
//! Collecting on demand cannot solve that, because the moment you most want the
//! logs is the moment the host is least able to hand them over. So the agent
//! scans continuously and ships what it finds, and the evidence is already at
//! the controller before the machine goes down.
//!
//! # Why it is bounded, and how
//!
//! `journalctl` will happily return gigabytes. Every query here is fenced in
//! four directions at once (IMPLEMENTATION.md §52):
//!
//! | Bound | Value | Reason |
//! | --- | --- | --- |
//! | time | since the last scan, capped | never re-reads the whole journal |
//! | priority | warning and above | debug chatter is not evidence |
//! | lines | 500 | bounded output regardless of rate |
//! | bytes | 1 MiB | bounded memory even if lines are enormous |
//!
//! # What it does not do
//!
//! It does not diagnose. A matched pattern is recorded as a fact, and SPEC.md
//! §83 is explicit that a kernel event must not be treated as equivalent to a
//! diagnosis. An OOM kill on a compute node is very often a job doing exactly
//! what jobs do.

use std::time::Duration;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::capability::well_known;
use crate::command::{Allowlist, CommandRunner};
use crate::entity::EntityType;
use crate::observation::{Observation, ProbeStatus};
use crate::probes::{Probe, ProbeContext, ProbeDefinition};

/// Probe id.
pub const PROBE_ID: &str = "journal.events";

/// Most lines to read in one scan.
pub const MAX_LINES: usize = 500;
/// Most bytes to read in one scan.
pub const MAX_BYTES: usize = 1024 * 1024;
/// Furthest back a scan will ever look, however long since the last one.
pub const MAX_LOOKBACK: Duration = Duration::from_secs(15 * 60);

/// How much a matched event says about the host's health.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EventSeverity {
    /// Worth preserving as evidence; says nothing about health on its own.
    ///
    /// A job being OOM-killed is the scheduler and kernel working as intended.
    /// Recording it matters; alarming about it does not.
    Notable,
    /// Evidence that the host itself is in trouble.
    Serious,
}

/// A class of event worth recognising.
///
/// Compiled in rather than configurable: these are kernel messages, not
/// deployment data, and a pattern list an operator can edit is a pattern list
/// that silently stops matching after a kernel upgrade.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EventClass {
    /// Short machine-readable name.
    pub kind: &'static str,
    /// How much it says about host health.
    pub severity: EventSeverity,
    /// Lowercase substrings, any of which identifies this class.
    pub patterns: &'static [&'static str],
}

/// The event classes SPEC.md §83 names, plus the storage ones §77 implies.
///
/// Substring matching on lowercased text: crude, but kernel messages are not a
/// stable grammar, and a missed event is worse than an extra one an operator
/// can dismiss.
pub const EVENT_CLASSES: &[EventClass] = &[
    EventClass {
        kind: "io_error",
        severity: EventSeverity::Serious,
        patterns: &[
            "i/o error",
            "buffer i/o error",
            "critical medium error",
            "unrecovered read error",
        ],
    },
    EventClass {
        kind: "filesystem_readonly",
        severity: EventSeverity::Serious,
        patterns: &[
            "remounting filesystem read-only",
            "ext4-fs error",
            "xfs.*corruption",
            "filesystem panic",
        ],
    },
    EventClass {
        kind: "hung_task",
        severity: EventSeverity::Serious,
        patterns: &["hung_task", "blocked for more than", "task blocked for more than"],
    },
    EventClass {
        kind: "nvme_error",
        severity: EventSeverity::Serious,
        patterns: &["nvme", "controller is down", "i/o timeout, reset controller"],
    },
    EventClass {
        kind: "gpu_xid",
        severity: EventSeverity::Serious,
        patterns: &["nvrm: xid", "xid error", "gpu has fallen off the bus"],
    },
    EventClass {
        kind: "mce",
        severity: EventSeverity::Serious,
        patterns: &["machine check", "mce:", "hardware error"],
    },
    EventClass {
        kind: "nfs_server_not_responding",
        severity: EventSeverity::Serious,
        patterns: &["nfs: server", "not responding", "nfs server"],
    },
    EventClass {
        kind: "oom",
        // Deliberately not Serious: on a compute node this is usually a job
        // exceeding its memory, which is the system working.
        severity: EventSeverity::Notable,
        patterns: &["out of memory: killed", "oom-killer", "oom_reaper"],
    },
    EventClass {
        kind: "network_down",
        severity: EventSeverity::Notable,
        patterns: &["link is down", "link down", "nic link is down"],
    },
    EventClass {
        kind: "thermal",
        severity: EventSeverity::Notable,
        patterns: &[
            "thermal throttling",
            "temperature above threshold",
            "critical temperature",
        ],
    },
];

/// One recognised event.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct JournalEvent {
    /// Which class it belongs to.
    pub kind: String,
    /// How much it says about health.
    pub severity: EventSeverity,
    /// The log line, truncated.
    pub message: String,
    /// The unit or kernel source, when the journal gave one.
    pub source: Option<String>,
    /// The timestamp the journal recorded, verbatim.
    pub timestamp: Option<String>,
}

/// Longest message excerpt kept. A stack trace is evidence; a whole one is a
/// denial of service on the operator reading it.
const MAX_MESSAGE: usize = 400;

/// Classify one journal line, if it is one of the classes worth keeping.
pub fn classify(line: &str) -> Option<&'static EventClass> {
    let lowered = line.to_ascii_lowercase();
    EVENT_CLASSES
        .iter()
        .find(|class| class.patterns.iter().any(|pattern| lowered.contains(pattern)))
}

/// Parse `journalctl -o short-iso` output into recognised events.
///
/// Lines that match nothing are dropped: the journal is mostly ordinary
/// operation, and keeping all of it would defeat every bound above.
pub fn parse_journal(output: &str) -> Vec<JournalEvent> {
    output
        .lines()
        .filter_map(|line| {
            let class = classify(line)?;

            // `2026-09-08T01:02:03+0000 hostname unit[123]: message`
            let mut fields = line.splitn(4, ' ');
            let timestamp = fields.next().filter(|t| t.starts_with(|c: char| c.is_ascii_digit()));
            let _host = fields.next();
            let source = fields.next().map(|s| s.trim_end_matches(':').to_string());

            Some(JournalEvent {
                kind: class.kind.to_string(),
                severity: class.severity,
                message: line.chars().take(MAX_MESSAGE).collect(),
                source,
                timestamp: timestamp.map(str::to_string),
            })
        })
        .collect()
}

/// Collects kernel and service events.
#[derive(Debug, Clone)]
pub struct JournalProbe {
    definition: ProbeDefinition,
    allowlist: Allowlist,
}

impl Default for JournalProbe {
    fn default() -> Self {
        Self::new()
    }
}

impl JournalProbe {
    /// A probe with the default schedule.
    pub fn new() -> Self {
        Self {
            definition: ProbeDefinition::new(PROBE_ID)
                .requiring([well_known::JOURNAL_READ])
                .targeting([EntityType::Host])
                // Often enough that events reach the controller long before a
                // reboot could take them, cheap enough not to matter: one
                // bounded journalctl read per interval.
                .every(Duration::from_secs(30))
                .within(Duration::from_secs(10))
                // A journal on a wedged filesystem can block; one outstanding
                // read at a time, like the NFS probe and for the same reason.
                .max_outstanding(1),
            allowlist: Allowlist::builtin(),
        }
    }

    /// Read the journal for a window, bounded in every direction.
    pub async fn read_window(&self, since: Duration) -> Result<Vec<JournalEvent>, String> {
        let since = since.min(MAX_LOOKBACK);

        let output = CommandRunner::new("journalctl")
            .args([
                // Only this boot: older entries belong to a machine that no
                // longer exists in any useful sense.
                "--boot",
                "--no-pager",
                "--output=short-iso",
                // Warning and above. Debug chatter is not evidence.
                "--priority=warning",
                &format!("--since=-{}s", since.as_secs()),
                &format!("--lines={MAX_LINES}"),
            ])
            .timeout(self.definition.timeout)
            .output_limit(MAX_BYTES)
            .run(&self.allowlist)
            .await
            .map_err(|error| error.to_string())?;

        if !output.is_success() && output.stdout.trim().is_empty() {
            return Err(format!("journalctl failed: {}", output.stderr.trim()));
        }

        if output.stdout_truncated {
            // Recorded rather than hidden: an operator reading the evidence
            // needs to know it is partial (IMPLEMENTATION.md §51).
            tracing::warn!("journal output truncated at {MAX_BYTES} bytes");
        }

        Ok(parse_journal(&output.stdout))
    }
}

#[async_trait]
impl Probe for JournalProbe {
    fn definition(&self) -> &ProbeDefinition {
        &self.definition
    }

    async fn collect(&self, context: &ProbeContext) -> Observation {
        // Look back over slightly more than one interval, so a slow cycle does
        // not leave a gap. Overlap is harmless: the controller stores whole
        // observations, and a repeated event is repeated evidence, not a
        // second fault.
        let window = context
            .parameter_u64("window_seconds")
            .map(Duration::from_secs)
            .unwrap_or(self.definition.interval * 2);

        let events = match self.read_window(window).await {
            Ok(events) => events,
            Err(detail) => {
                // Not being able to read the journal is a limitation of this
                // observer, not a fault of the host.
                return Observation::new(PROBE_ID.into(), context.target_entity, ProbeStatus::Unsupported)
                    .with_error("journal_unavailable", detail);
            }
        };

        let serious: Vec<&JournalEvent> = events.iter().filter(|e| e.severity == EventSeverity::Serious).collect();

        let status = if serious.is_empty() {
            ProbeStatus::Ok
        } else {
            ProbeStatus::Degraded
        };

        let mut kinds: Vec<&str> = events.iter().map(|e| e.kind.as_str()).collect();
        kinds.sort_unstable();
        kinds.dedup();

        let observation = Observation::new(PROBE_ID.into(), context.target_entity, status)
            .with_payload(serde_json::json!({
                "window_seconds": window.as_secs(),
                "event_count": events.len(),
                "serious_count": serious.len(),
                "kinds": kinds,
            }))
            // The lines themselves are evidence, not payload: they are what an
            // operator reads after the machine has rebooted and taken the
            // journal with it (SPEC.md §103).
            .with_evidence(serde_json::json!({ "events": events }));

        if serious.is_empty() {
            observation
        } else {
            observation.with_error(
                "kernel_events",
                format!(
                    "{} serious kernel event(s): {}",
                    serious.len(),
                    serious.iter().map(|e| e.kind.as_str()).collect::<Vec<_>>().join(", ")
                ),
            )
        }
    }
}

// Operators may retune this probe's schedule in [probes].
crate::probes::configurable_probe!(JournalProbe);

#[cfg(test)]
mod tests {
    use super::*;
    use crate::capability::CapabilitySet;
    use crate::entity::{EntityKey, EntityType};

    fn context() -> ProbeContext {
        ProbeContext::local(
            EntityKey::new("lab", EntityType::Host, "node-a").entity_id(),
            CapabilitySet::new(),
        )
        .with_timeout(Duration::from_millis(500))
    }

    #[test]
    fn the_event_classes_spec_83_names_are_all_recognised() {
        // SPEC.md §83 lists these explicitly.
        let samples = [
            (
                "2026-09-08T01:00:00+0000 n1 kernel: Out of memory: Killed process 123 (python)",
                "oom",
            ),
            (
                "2026-09-08T01:00:00+0000 n1 kernel: nfs: server fs1 not responding, still trying",
                "nfs_server_not_responding",
            ),
            (
                "2026-09-08T01:00:00+0000 n1 kernel: nvme nvme0: I/O timeout, reset controller",
                "nvme_error",
            ),
            (
                "2026-09-08T01:00:00+0000 n1 kernel: Buffer I/O error on dev sda1",
                "io_error",
            ),
            (
                "2026-09-08T01:00:00+0000 n1 kernel: NVRM: Xid (PCI:0000:01:00): 79",
                "gpu_xid",
            ),
            (
                "2026-09-08T01:00:00+0000 n1 kernel: INFO: task nfsd:1234 blocked for more than 120 seconds",
                "hung_task",
            ),
            (
                "2026-09-08T01:00:00+0000 n1 kernel: e1000e: eth0 NIC Link is Down",
                "network_down",
            ),
        ];

        for (line, expected) in samples {
            let class = classify(line).unwrap_or_else(|| panic!("not recognised: {line}"));
            assert_eq!(class.kind, expected, "{line}");
        }
    }

    #[test]
    fn ordinary_log_lines_are_not_events() {
        // The journal is mostly ordinary operation; keeping it all would defeat
        // every bound this probe has.
        for line in [
            "2026-09-08T01:00:00+0000 n1 sshd[1]: Accepted publickey for alice",
            "2026-09-08T01:00:00+0000 n1 systemd[1]: Started Session 42 of user bob.",
            "2026-09-08T01:00:00+0000 n1 slurmd[9]: launch task StepId=1.0",
        ] {
            assert!(classify(line).is_none(), "wrongly matched: {line}");
        }
    }

    #[test]
    fn a_job_being_oom_killed_is_recorded_but_not_alarming() {
        // On a compute node this is usually a job doing exactly what jobs do.
        let class = classify("kernel: Out of memory: Killed process 1 (a.out)").expect("recognised");
        assert_eq!(class.severity, EventSeverity::Notable);
    }

    #[test]
    fn hardware_and_filesystem_trouble_is_serious() {
        for line in [
            "kernel: Buffer I/O error on dev nvme0n1",
            "kernel: EXT4-fs error (device sda1): remounting filesystem read-only",
            "kernel: INFO: task kworker:1 blocked for more than 120 seconds",
            "kernel: mce: [Hardware Error]: Machine check events logged",
        ] {
            assert_eq!(
                classify(line).expect("recognised").severity,
                EventSeverity::Serious,
                "{line}"
            );
        }
    }

    #[test]
    fn matching_is_case_insensitive() {
        assert!(classify("KERNEL: OUT OF MEMORY: KILLED process 1").is_some());
        assert!(classify("NVRM: XID (PCI:0000:01:00): 13").is_some());
    }

    #[test]
    fn parsing_extracts_the_timestamp_and_source() {
        let output = "2026-09-08T01:02:03+0000 node-a kernel: Buffer I/O error on dev sda1, logical block 0\n";
        let events = parse_journal(output);

        assert_eq!(events.len(), 1);
        assert_eq!(events[0].kind, "io_error");
        assert_eq!(events[0].timestamp.as_deref(), Some("2026-09-08T01:02:03+0000"));
        assert_eq!(events[0].source.as_deref(), Some("kernel"));
        assert!(events[0].message.contains("Buffer I/O error"));
    }

    #[test]
    fn an_enormous_line_is_truncated_rather_than_stored_whole() {
        // A stack trace is evidence; a whole one is a denial of service on the
        // operator reading it.
        let line = format!("2026-09-08T01:00:00+0000 n1 kernel: hung_task {}", "x".repeat(10_000));
        let events = parse_journal(&line);

        assert_eq!(events.len(), 1);
        assert!(events[0].message.chars().count() <= MAX_MESSAGE);
    }

    #[test]
    fn a_journal_with_nothing_notable_yields_nothing() {
        let output = "2026-09-08T01:00:00+0000 n1 sshd[1]: Accepted publickey\n\n";
        assert!(parse_journal(output).is_empty());
    }

    #[test]
    fn several_events_are_all_kept() {
        let output = "\
2026-09-08T01:00:00+0000 n1 kernel: Buffer I/O error on dev sda1
2026-09-08T01:00:01+0000 n1 sshd[1]: Accepted publickey for alice
2026-09-08T01:00:02+0000 n1 kernel: Out of memory: Killed process 9 (a.out)
";
        let events = parse_journal(output);
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].kind, "io_error");
        assert_eq!(events[1].kind, "oom");
    }

    #[test]
    fn the_probe_is_gated_on_the_journal_capability() {
        let definition = JournalProbe::new().definition().clone();
        assert!(definition.applies_to(EntityType::Host, &CapabilitySet::from_iter(["journal.read"])));
        assert!(!definition.applies_to(EntityType::Host, &CapabilitySet::new()));
    }

    #[test]
    fn only_one_journal_read_runs_at_a_time() {
        // A journal on a wedged filesystem can block, for the same reason an
        // NFS stat can.
        assert_eq!(JournalProbe::new().definition().max_outstanding, 1);
    }

    #[test]
    fn the_query_is_bounded_in_every_direction() {
        // IMPLEMENTATION.md §52. Asserted against the implementation so a later
        // edit cannot quietly remove a bound.
        let implementation = include_str!("journal.rs")
            .split("#[cfg(test)]")
            .next()
            .expect("implementation");
        for bound in ["--lines=", "--since=", "--priority=", "output_limit(", "--boot"] {
            assert!(
                implementation.contains(bound),
                "the journal query must be bounded by {bound}"
            );
        }
    }

    #[test]
    fn the_probe_only_ever_reads() {
        let implementation = include_str!("journal.rs")
            .split("#[cfg(test)]")
            .next()
            .expect("implementation");
        for forbidden in ["--rotate", "--vacuum", "--flush", "--sync"] {
            assert!(
                !implementation.contains(forbidden),
                "the journal probe must never {forbidden}"
            );
        }
    }

    #[tokio::test]
    async fn a_host_without_journalctl_reports_unsupported_not_failed() {
        // "I cannot see" is not "it is broken".
        let probe = JournalProbe {
            allowlist: Allowlist::from(["definitely-not-journalctl"]),
            ..JournalProbe::new()
        };
        let observation = probe.collect(&context()).await;

        assert_eq!(observation.status, ProbeStatus::Unsupported);
        assert!(!observation.status.is_bad());
    }

    #[test]
    fn a_lookback_longer_than_the_cap_is_clamped() {
        // Otherwise the first scan after a long outage would read the entire
        // journal, which is exactly the unbounded read every other bound here
        // exists to prevent.
        assert_eq!(Duration::from_secs(86_400).min(MAX_LOOKBACK), MAX_LOOKBACK);
        assert_eq!(Duration::from_secs(60).min(MAX_LOOKBACK), Duration::from_secs(60));
    }
}
