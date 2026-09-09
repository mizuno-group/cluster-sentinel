//! The agent's own probes.
//!
//! What the agent measures about the machine it is on. Only these can see
//! `/proc`, local units and mounted filesystems; everything else is visible
//! from outside and is measured by an observer instead.
//!
//! Each probe runs on its own schedule with jitter, so a fleet of agents
//! restarted together does not settle into lockstep and arrive at the
//! controller in a thundering herd (SPEC.md §123).

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::capability::CapabilitySet;
use crate::entity::{EntityId, EntityType};
use crate::observation::Observation;
use crate::probes::{ExecutionMode, Probe, ProbeContext, ProbeId, ProbeRunner, Skipped};

/// A probe, its parameters, and when it is next due.
///
/// Parameters are per entry rather than per agent, because one host can mount
/// several filesystems and each needs its own probe with its own mount point —
/// and, crucially, its own concurrency slot, so one wedged mount does not stop
/// the others being checked.
struct Scheduled {
    probe: Arc<dyn Probe>,
    parameters: serde_json::Value,
    next_due: Instant,
    /// What this probe measures, when that is not the host itself.
    ///
    /// A service running on this host is its own entity, and an observation
    /// about `slurmd@node01` must be attributed to `slurmd@node01` rather than
    /// to the machine it happens to run on -- otherwise the distinction
    /// between a failed daemon and a failed host, which is the whole point,
    /// has nowhere to live.
    target: Option<EntityId>,
}

/// Runs the agent's local probes on their own schedules.
pub struct LocalProbes {
    runner: ProbeRunner,
    scheduled: Vec<Scheduled>,
    entity: EntityId,
    capabilities: CapabilitySet,
    parameters: serde_json::Value,
    jitter_seed: u64,
}

impl LocalProbes {
    /// Build the default local probe set for a host.
    pub fn new(entity: EntityId, capabilities: CapabilitySet) -> Self {
        let probes: Vec<Arc<dyn Probe>> = vec![Arc::new(crate::probes::host::HostMetricsProbe::new())];
        Self::with_probes(entity, capabilities, probes)
    }

    /// Build with an explicit probe set.
    pub fn with_probes(entity: EntityId, capabilities: CapabilitySet, probes: Vec<Arc<dyn Probe>>) -> Self {
        let mut agent = Self {
            runner: ProbeRunner::new(),
            scheduled: Vec::new(),
            entity,
            capabilities,
            parameters: serde_json::Value::Null,
            // Derived from the process id: enough to break lockstep between
            // hosts, and stable within one process so the schedule does not
            // wander.
            jitter_seed: u64::from(std::process::id()),
        };

        let now = Instant::now();
        for probe in probes {
            if !agent.applies(probe.as_ref()) {
                continue;
            }
            let jitter = agent.jitter_for(&probe.definition().id, probe.definition().interval);
            agent.scheduled.push(Scheduled {
                probe,
                parameters: serde_json::Value::Null,
                next_due: now + jitter,
                target: None,
            });
        }
        agent
    }

    /// Schedule an additional probe with its own parameters.
    ///
    /// Used for probes that apply once per mount, per unit or per device rather
    /// than once per host. Each entry gets its own concurrency slot, so one
    /// wedged mount does not stop the others being checked.
    ///
    /// Returns whether the probe was scheduled; a probe this host lacks the
    /// capability for is refused.
    pub fn add(&mut self, probe: Arc<dyn Probe>, parameters: serde_json::Value) -> bool {
        self.add_for(None, probe, parameters)
    }

    /// Schedule a probe that measures something other than this host.
    ///
    /// `target` is the entity the observation is about. The capability gate is
    /// unchanged and still asks about **this host**, because it is this host
    /// that has to run the probe: watching `slurmd@node01` needs systemd on
    /// node01, whatever the observation is attributed to. Skipping the gate
    /// here would schedule `systemctl` on hosts that have no systemd and
    /// report UNSUPPORTED for ever.
    pub fn add_for(&mut self, target: Option<EntityId>, probe: Arc<dyn Probe>, parameters: serde_json::Value) -> bool {
        if !self.applies(probe.as_ref()) {
            return false;
        }
        let jitter = self.jitter_for(&probe.definition().id, probe.definition().interval);
        self.scheduled.push(Scheduled {
            probe,
            parameters,
            next_due: Instant::now() + jitter,
            target,
        });
        true
    }

    /// Builder: set probe parameters (unit names, mount points, and so on).
    pub fn with_parameters(mut self, parameters: serde_json::Value) -> Self {
        self.parameters = parameters;
        self
    }

    /// Whether a probe applies to this host.
    fn applies(&self, probe: &dyn Probe) -> bool {
        let definition = probe.definition();
        // A probe that only makes sense from elsewhere is not the agent's job:
        // an agent asking itself whether it is running can only say yes.
        definition.execution_mode != ExecutionMode::Remote
            && definition.applies_to(EntityType::Host, &self.capabilities)
    }

    /// A stable per-probe jitter of up to a tenth of the interval.
    fn jitter_for(&self, probe: &ProbeId, interval: Duration) -> Duration {
        let span = interval.as_millis() as u64 / 10;
        if span == 0 {
            return Duration::ZERO;
        }
        let hash = probe
            .as_str()
            .bytes()
            .fold(self.jitter_seed, |acc, b| acc.wrapping_mul(31).wrapping_add(b as u64));
        Duration::from_millis(hash % span)
    }

    /// The probes that will run on this host.
    pub fn probe_ids(&self) -> Vec<&str> {
        self.scheduled
            .iter()
            .map(|s| s.probe.definition().id.as_str())
            .collect()
    }

    /// How many probes are scheduled.
    /// The scheduled probes, so a caller can inspect the schedules in force.
    pub fn probes(&self) -> impl Iterator<Item = &Arc<dyn Probe>> {
        self.scheduled.iter().map(|s| &s.probe)
    }

    pub fn len(&self) -> usize {
        self.scheduled.len()
    }

    /// Whether no probe applies to this host.
    pub fn is_empty(&self) -> bool {
        self.scheduled.is_empty()
    }

    /// Run every probe that is due, and reschedule it.
    pub async fn run_due(&mut self) -> Vec<Observation> {
        let now = Instant::now();
        let mut observations = Vec::new();

        for index in 0..self.scheduled.len() {
            if self.scheduled[index].next_due > now {
                continue;
            }

            let probe = Arc::clone(&self.scheduled[index].probe);
            let definition = probe.definition().clone();

            // Per-probe parameters win; the agent-wide set is the fallback.
            let parameters = match &self.scheduled[index].parameters {
                serde_json::Value::Null => self.parameters.clone(),
                specific => specific.clone(),
            };
            let target = self.scheduled[index].target.unwrap_or(self.entity);
            let context = ProbeContext::local(target, self.capabilities.clone())
                .with_parameters(parameters)
                .with_timeout(definition.timeout);

            match self.runner.run(probe, context).await {
                Ok(observation) => observations.push(observation),
                Err(Skipped::AlreadyRunning) => {
                    tracing::debug!(probe = %definition.id, "probe still outstanding; skipping this turn");
                }
            }

            self.scheduled[index].next_due = Instant::now() + definition.interval;
        }

        observations
    }

    /// Run every probe now, ignoring the schedule.
    pub async fn run_all(&mut self) -> Vec<Observation> {
        for scheduled in &mut self.scheduled {
            scheduled.next_due = Instant::now();
        }
        self.run_due().await
    }

    /// When the next probe is due.
    pub fn next_due(&self) -> Option<Instant> {
        self.scheduled.iter().map(|s| s.next_due).min()
    }

    /// Outstanding executions, by probe.
    pub fn outstanding(&self) -> HashMap<String, u32> {
        self.scheduled
            .iter()
            .map(|s| {
                let id = &s.probe.definition().id;
                (id.to_string(), self.runner.outstanding(id, self.entity))
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::entity::{EntityKey, EntityType};
    use crate::observation::ProbeStatus;
    use crate::probes::ProbeDefinition;
    use async_trait::async_trait;

    fn entity() -> EntityId {
        EntityKey::new("lab", EntityType::Host, "node-a").entity_id()
    }

    struct CountingProbe {
        definition: ProbeDefinition,
        runs: Arc<std::sync::atomic::AtomicU32>,
    }

    impl CountingProbe {
        fn new(definition: ProbeDefinition) -> (Arc<Self>, Arc<std::sync::atomic::AtomicU32>) {
            let runs = Arc::new(std::sync::atomic::AtomicU32::new(0));
            (
                Arc::new(Self {
                    definition,
                    runs: Arc::clone(&runs),
                }),
                runs,
            )
        }
    }

    #[async_trait]
    impl Probe for CountingProbe {
        fn definition(&self) -> &ProbeDefinition {
            &self.definition
        }

        async fn collect(&self, context: &ProbeContext) -> Observation {
            self.runs.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Observation::new(self.definition.id.clone(), context.target_entity, ProbeStatus::Ok)
        }
    }

    #[tokio::test]
    async fn only_probes_this_host_has_the_capability_for_are_scheduled() {
        let with = LocalProbes::new(entity(), CapabilitySet::from_iter(["host.metrics"]));
        assert_eq!(with.probe_ids(), [crate::probes::host::PROBE_ID]);

        let without = LocalProbes::new(entity(), CapabilitySet::new());
        assert!(without.is_empty(), "no capability, no probe");
    }

    #[tokio::test]
    async fn a_remote_only_probe_is_never_scheduled_locally() {
        // Asking yourself whether your own agent is running is not evidence.
        let probes: Vec<Arc<dyn Probe>> = vec![Arc::new(crate::probes::sentinel_rpc::SentinelAgentProbe::new())];
        let local = LocalProbes::with_probes(entity(), CapabilitySet::from_iter(["sentinel.agent"]), probes);
        assert!(local.is_empty());
    }

    #[tokio::test]
    async fn a_due_probe_runs_and_a_pending_one_does_not() {
        let (probe, runs) = CountingProbe::new(ProbeDefinition::new("test.probe").every(Duration::from_secs(3600)));
        let mut local = LocalProbes::with_probes(entity(), CapabilitySet::new(), vec![probe]);

        let observations = local.run_all().await;
        assert_eq!(observations.len(), 1);
        assert_eq!(runs.load(std::sync::atomic::Ordering::SeqCst), 1);

        // Not due again for an hour.
        assert!(local.run_due().await.is_empty());
        assert_eq!(runs.load(std::sync::atomic::Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn a_short_interval_probe_runs_again_once_it_is_due() {
        let (probe, runs) = CountingProbe::new(ProbeDefinition::new("test.probe").every(Duration::from_millis(50)));
        let mut local = LocalProbes::with_probes(entity(), CapabilitySet::new(), vec![probe]);

        local.run_all().await;
        tokio::time::sleep(Duration::from_millis(120)).await;
        assert_eq!(local.run_due().await.len(), 1);
        assert_eq!(runs.load(std::sync::atomic::Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn probes_are_jittered_so_a_fleet_does_not_move_in_lockstep() {
        let local = LocalProbes::new(entity(), CapabilitySet::from_iter(["host.metrics"]));
        let next = local.next_due().expect("scheduled");
        let interval = crate::probes::host::HostMetricsProbe::new().definition().interval;

        assert!(next <= Instant::now() + interval / 10 + Duration::from_millis(10));
    }

    #[test]
    fn jitter_never_exceeds_a_tenth_of_the_interval() {
        let local = LocalProbes::new(entity(), CapabilitySet::new());
        for seconds in [1u64, 5, 15, 30, 300] {
            let interval = Duration::from_secs(seconds);
            let jitter = local.jitter_for(&ProbeId::new("some.probe"), interval);
            assert!(
                jitter < interval / 10 + Duration::from_millis(1),
                "{seconds}s: {jitter:?}"
            );
        }
    }

    #[test]
    fn a_tiny_interval_produces_no_jitter_rather_than_dividing_by_zero() {
        let local = LocalProbes::new(entity(), CapabilitySet::new());
        assert_eq!(
            local.jitter_for(&ProbeId::new("p"), Duration::from_millis(5)),
            Duration::ZERO
        );
        assert_eq!(local.jitter_for(&ProbeId::new("p"), Duration::ZERO), Duration::ZERO);
    }

    #[test]
    fn different_probes_get_different_jitter() {
        // Otherwise every probe on a host would still fire together.
        let local = LocalProbes::new(entity(), CapabilitySet::new());
        let interval = Duration::from_secs(60);
        let a = local.jitter_for(&ProbeId::new("probe.a"), interval);
        let b = local.jitter_for(&ProbeId::new("probe.bb"), interval);
        assert_ne!(a, b);
    }

    #[tokio::test]
    async fn a_probe_can_be_scheduled_more_than_once_with_different_parameters() {
        // One host, several mounts: each needs its own probe and its own
        // concurrency slot.
        let (probe, runs) = CountingProbe::new(ProbeDefinition::new("fs.probe"));
        let mut local = LocalProbes::with_probes(entity(), CapabilitySet::new(), vec![]);

        assert!(local.add(
            Arc::clone(&probe) as Arc<dyn Probe>,
            serde_json::json!({"mount_point": "/home"})
        ));
        assert!(local.add(probe as Arc<dyn Probe>, serde_json::json!({"mount_point": "/scratch"})));
        assert_eq!(local.len(), 2);

        local.run_all().await;
        assert_eq!(runs.load(std::sync::atomic::Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn adding_a_probe_the_host_lacks_the_capability_for_is_refused() {
        let mut local = LocalProbes::with_probes(entity(), CapabilitySet::new(), vec![]);
        let probe = Arc::new(crate::probes::nfs::NfsClientIoProbe::new()) as Arc<dyn Probe>;
        assert!(!local.add(probe, serde_json::json!({"mount_point": "/home"})));
        assert!(local.is_empty());
    }

    #[tokio::test]
    async fn a_panicking_probe_does_not_stop_the_others() {
        struct Exploding(ProbeDefinition);

        #[async_trait]
        impl Probe for Exploding {
            fn definition(&self) -> &ProbeDefinition {
                &self.0
            }
            async fn collect(&self, _: &ProbeContext) -> Observation {
                panic!("boom");
            }
        }

        let (healthy, runs) = CountingProbe::new(ProbeDefinition::new("healthy.probe"));
        let probes: Vec<Arc<dyn Probe>> = vec![Arc::new(Exploding(ProbeDefinition::new("exploding.probe"))), healthy];

        let mut local = LocalProbes::with_probes(entity(), CapabilitySet::new(), probes);
        let observations = local.run_all().await;

        assert_eq!(observations.len(), 2, "both probes reported");
        assert_eq!(
            runs.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "the healthy probe still ran"
        );
        assert!(observations
            .iter()
            .any(|o| o.error_code.as_deref() == Some("probe_panicked")));
    }

    #[tokio::test]
    async fn the_host_metrics_probe_actually_produces_a_reading_here() {
        let mut local = LocalProbes::new(entity(), CapabilitySet::from_iter(["host.metrics"]));
        let observations = local.run_all().await;

        assert_eq!(observations.len(), 1);
        assert_ne!(observations[0].status, ProbeStatus::Unsupported);
        assert_eq!(observations[0].target_entity, entity());
        assert!(!observations[0].is_remote(), "a local probe has no observer");
    }
}
