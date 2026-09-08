//! Running probes safely.
//!
//! Every probe execution goes through here, and the runner enforces three
//! things no individual probe can be trusted to enforce for itself
//! (SPEC.md §49, §76, §119):
//!
//! * **A timeout.** A probe that never returns must not stall the schedule.
//! * **Panic isolation.** A bug in one probe must not take the daemon down.
//!   Monitoring that dies during an incident is worse than no monitoring.
//! * **A concurrency limit per target.** A blocked NFS syscall is in
//!   uninterruptible sleep: a second one cannot help, cannot be cancelled, and
//!   each one costs a thread. The limit is what stops a hung mount from
//!   consuming the agent.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use crate::entity::EntityId;
use crate::observation::{Observation, ProbeStatus};
use crate::probes::{Probe, ProbeContext, ProbeId};
use crate::time::now;

/// Why a probe produced no observation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Skipped {
    /// A previous execution against this target is still outstanding.
    ///
    /// Deliberately produces no observation: nothing new was measured, and
    /// inventing a result would be worse than a gap. The outstanding execution
    /// will report when it finishes or times out.
    AlreadyRunning,
}

/// Runs probes with timeout, isolation and concurrency limits.
#[derive(Debug, Default)]
pub struct ProbeRunner {
    outstanding: Arc<Mutex<HashMap<(ProbeId, EntityId), u32>>>,
}

impl ProbeRunner {
    /// A runner with nothing in flight.
    pub fn new() -> Self {
        Self::default()
    }

    /// How many executions of a probe are in flight against a target.
    pub fn outstanding(&self, probe: &ProbeId, target: EntityId) -> u32 {
        self.outstanding
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&(probe.clone(), target))
            .copied()
            .unwrap_or(0)
    }

    /// Run one probe.
    ///
    /// Returns the resulting observation, or why none was produced.
    pub async fn run(&self, probe: Arc<dyn Probe>, context: ProbeContext) -> Result<Observation, Skipped> {
        let definition = probe.definition().clone();
        let key = (definition.id.clone(), context.target_entity);

        let Some(guard) = OutstandingGuard::acquire(Arc::clone(&self.outstanding), key, definition.max_outstanding)
        else {
            return Err(Skipped::AlreadyRunning);
        };

        let started_at = now();
        let started = std::time::Instant::now();
        let target = context.target_entity;
        let observer = context.observer_entity;
        let probe_id = definition.id.clone();

        // Spawned so that a panic surfaces as a JoinError instead of unwinding
        // through the scheduler. The guard moves into the task: if the probe
        // hangs past its timeout it keeps holding its slot, which is exactly
        // what must happen for a stuck filesystem.
        let task = tokio::spawn(async move {
            let observation = probe.collect(&context).await;
            drop(guard);
            observation
        });

        let outcome = tokio::time::timeout(definition.timeout, task).await;
        let duration_ms = started.elapsed().as_millis() as u64;

        let observation = match outcome {
            Ok(Ok(observation)) => observation,
            Ok(Err(error)) if error.is_panic() => {
                tracing::error!(probe = %probe_id, %target, "probe panicked");
                Observation::new(probe_id, target, ProbeStatus::Failed).with_error(
                    "probe_panicked",
                    "the probe panicked; this is a bug in the probe, not a fault of the target",
                )
            }
            Ok(Err(error)) => {
                Observation::new(probe_id, target, ProbeStatus::Failed).with_error("probe_cancelled", error.to_string())
            }
            Err(_) => {
                tracing::debug!(probe = %probe_id, %target, timeout = ?definition.timeout, "probe timed out");
                Observation::new(probe_id, target, ProbeStatus::Timeout)
                    .with_error("probe_timeout", format!("no result within {:?}", definition.timeout))
            }
        };

        let observation = observation.with_times(started_at, now()).with_duration_ms(duration_ms);
        Ok(match observer {
            Some(observer) => observation.with_observer(observer),
            None => observation,
        })
    }
}

/// Holds a concurrency slot until dropped.
struct OutstandingGuard {
    counters: Arc<Mutex<HashMap<(ProbeId, EntityId), u32>>>,
    key: (ProbeId, EntityId),
}

impl OutstandingGuard {
    fn acquire(
        counters: Arc<Mutex<HashMap<(ProbeId, EntityId), u32>>>,
        key: (ProbeId, EntityId),
        limit: u32,
    ) -> Option<Self> {
        {
            let mut counters = counters.lock().unwrap_or_else(|e| e.into_inner());
            let count = counters.entry(key.clone()).or_insert(0);
            if *count >= limit {
                return None;
            }
            *count += 1;
        }
        Some(Self { counters, key })
    }
}

impl Drop for OutstandingGuard {
    fn drop(&mut self) {
        let mut counters = self.counters.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(count) = counters.get_mut(&self.key) {
            *count = count.saturating_sub(1);
            if *count == 0 {
                counters.remove(&self.key);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::capability::CapabilitySet;
    use crate::entity::{EntityKey, EntityType};
    use crate::probes::ProbeDefinition;
    use async_trait::async_trait;
    use std::time::Duration;

    fn entity() -> EntityId {
        EntityKey::new("lab", EntityType::Host, "node-a").entity_id()
    }

    fn context() -> ProbeContext {
        ProbeContext::local(entity(), CapabilitySet::new())
    }

    /// A probe that does whatever the test tells it to.
    struct TestProbe {
        definition: ProbeDefinition,
        behaviour: Behaviour,
    }

    #[derive(Clone, Copy)]
    enum Behaviour {
        Succeed,
        Panic,
        Sleep(Duration),
    }

    impl TestProbe {
        fn new(behaviour: Behaviour) -> Self {
            Self {
                definition: ProbeDefinition::new("test.probe").within(Duration::from_millis(200)),
                behaviour,
            }
        }

        fn with_definition(definition: ProbeDefinition, behaviour: Behaviour) -> Self {
            Self { definition, behaviour }
        }
    }

    #[async_trait]
    impl Probe for TestProbe {
        fn definition(&self) -> &ProbeDefinition {
            &self.definition
        }

        async fn collect(&self, context: &ProbeContext) -> Observation {
            match self.behaviour {
                Behaviour::Succeed => {
                    Observation::new(self.definition.id.clone(), context.target_entity, ProbeStatus::Ok)
                }
                Behaviour::Panic => panic!("this probe is broken"),
                Behaviour::Sleep(duration) => {
                    tokio::time::sleep(duration).await;
                    Observation::new(self.definition.id.clone(), context.target_entity, ProbeStatus::Ok)
                }
            }
        }
    }

    #[tokio::test]
    async fn a_successful_probe_returns_its_observation_with_timings() {
        let runner = ProbeRunner::new();
        let observation = runner
            .run(Arc::new(TestProbe::new(Behaviour::Succeed)), context())
            .await
            .expect("not skipped");

        assert_eq!(observation.status, ProbeStatus::Ok);
        assert_eq!(observation.target_entity, entity());
        assert!(observation.finished_at >= observation.started_at);
    }

    #[tokio::test]
    async fn a_panicking_probe_does_not_bring_down_the_runner() {
        // SPEC.md §119. The daemon must survive a bug in one probe.
        let runner = ProbeRunner::new();
        let observation = runner
            .run(Arc::new(TestProbe::new(Behaviour::Panic)), context())
            .await
            .expect("not skipped");

        assert_eq!(observation.status, ProbeStatus::Failed);
        assert_eq!(observation.error_code.as_deref(), Some("probe_panicked"));

        // And the runner still works afterwards.
        let next = runner
            .run(Arc::new(TestProbe::new(Behaviour::Succeed)), context())
            .await
            .expect("not skipped");
        assert_eq!(next.status, ProbeStatus::Ok);
    }

    #[tokio::test]
    async fn a_panic_is_blamed_on_the_probe_not_on_the_target() {
        // Recording a host as failed because our own code has a bug would send
        // someone to investigate the wrong thing entirely.
        let runner = ProbeRunner::new();
        let observation = runner
            .run(Arc::new(TestProbe::new(Behaviour::Panic)), context())
            .await
            .expect("not skipped");
        assert!(
            observation
                .error_message
                .as_deref()
                .is_some_and(|m| m.contains("bug in the probe")),
            "{:?}",
            observation.error_message
        );
    }

    #[tokio::test]
    async fn a_slow_probe_times_out_rather_than_blocking_the_schedule() {
        let runner = ProbeRunner::new();
        let started = std::time::Instant::now();
        let observation = runner
            .run(
                Arc::new(TestProbe::new(Behaviour::Sleep(Duration::from_secs(30)))),
                context(),
            )
            .await
            .expect("not skipped");

        assert_eq!(observation.status, ProbeStatus::Timeout);
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "the timeout must actually cut it short"
        );
    }

    #[tokio::test]
    async fn a_second_execution_is_skipped_while_the_first_is_outstanding() {
        // SPEC.md §76: for a filesystem probe this is what stops a hung mount
        // consuming the agent one blocked thread at a time.
        let definition = ProbeDefinition::new("fs.probe")
            .within(Duration::from_millis(100))
            .max_outstanding(1);
        let runner = Arc::new(ProbeRunner::new());

        let first = {
            let runner = Arc::clone(&runner);
            tokio::spawn(async move {
                runner
                    .run(
                        Arc::new(TestProbe::with_definition(
                            definition.clone(),
                            Behaviour::Sleep(Duration::from_secs(30)),
                        )),
                        context(),
                    )
                    .await
            })
        };

        // Let the first probe get going and time out; its slot stays held
        // because the probe itself has not finished.
        tokio::time::sleep(Duration::from_millis(300)).await;

        let definition = ProbeDefinition::new("fs.probe")
            .within(Duration::from_millis(100))
            .max_outstanding(1);
        let second = runner
            .run(
                Arc::new(TestProbe::with_definition(definition, Behaviour::Succeed)),
                context(),
            )
            .await;

        assert_eq!(second, Err(Skipped::AlreadyRunning));
        first.abort();
    }

    #[tokio::test]
    async fn a_finished_probe_releases_its_slot() {
        let definition = ProbeDefinition::new("fs.probe").max_outstanding(1);
        let runner = ProbeRunner::new();
        let probe = || Arc::new(TestProbe::with_definition(definition.clone(), Behaviour::Succeed));

        assert!(runner.run(probe(), context()).await.is_ok());
        assert_eq!(runner.outstanding(&ProbeId::new("fs.probe"), entity()), 0);
        assert!(runner.run(probe(), context()).await.is_ok(), "the slot came back");
    }

    #[tokio::test]
    async fn a_panicking_probe_releases_its_slot() {
        // Otherwise one bug would permanently disable that probe.
        let definition = ProbeDefinition::new("fs.probe").max_outstanding(1);
        let runner = ProbeRunner::new();

        let _ = runner
            .run(
                Arc::new(TestProbe::with_definition(definition.clone(), Behaviour::Panic)),
                context(),
            )
            .await;

        assert_eq!(runner.outstanding(&ProbeId::new("fs.probe"), entity()), 0);
        assert!(runner
            .run(
                Arc::new(TestProbe::with_definition(definition, Behaviour::Succeed)),
                context()
            )
            .await
            .is_ok());
    }

    #[tokio::test]
    async fn the_limit_is_per_target_not_global() {
        // One wedged fileserver must not stop every other host being probed.
        let definition = ProbeDefinition::new("fs.probe")
            .within(Duration::from_millis(100))
            .max_outstanding(1);
        let runner = Arc::new(ProbeRunner::new());

        let stuck = {
            let runner = Arc::clone(&runner);
            let definition = definition.clone();
            tokio::spawn(async move {
                runner
                    .run(
                        Arc::new(TestProbe::with_definition(
                            definition,
                            Behaviour::Sleep(Duration::from_secs(30)),
                        )),
                        context(),
                    )
                    .await
            })
        };
        tokio::time::sleep(Duration::from_millis(300)).await;

        let other = ProbeContext::local(
            EntityKey::new("lab", EntityType::Host, "node-b").entity_id(),
            CapabilitySet::new(),
        );
        let result = runner
            .run(
                Arc::new(TestProbe::with_definition(definition, Behaviour::Succeed)),
                other,
            )
            .await;

        assert!(result.is_ok(), "a different target must still be probed");
        stuck.abort();
    }

    #[tokio::test]
    async fn a_remote_probe_records_its_observer() {
        let runner = ProbeRunner::new();
        let observer = EntityKey::new("lab", EntityType::Host, "peer-a").entity_id();
        let observation = runner
            .run(
                Arc::new(TestProbe::new(Behaviour::Succeed)),
                context().observed_by(observer),
            )
            .await
            .expect("not skipped");

        assert_eq!(observation.observer_entity, Some(observer));
        assert!(observation.is_remote());
    }
}
