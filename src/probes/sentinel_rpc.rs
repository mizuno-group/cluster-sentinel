//! Checking that a Sentinel agent is alive on another host.
//!
//! This probe exists to make one specific mistake impossible: concluding that
//! a host is gone when only the monitoring on it has stopped. The agent being
//! unreachable while SSH, the scheduler and the network all answer is a
//! *monitoring* fault, and an operator woken for it should be told so.

use std::time::Duration;

use async_trait::async_trait;

use crate::agent::rpc::{AgentHealth, HEALTH_PATH};
use crate::capability::well_known;
use crate::entity::EntityType;
use crate::observation::{Observation, ProbeStatus};
use crate::probes::{ExecutionMode, Probe, ProbeContext, ProbeDefinition};

/// Probe id.
pub const PROBE_ID: &str = "sentinel.agent";

/// What the agent said, if anything.
#[derive(Debug, Clone, PartialEq)]
pub enum AgentOutcome {
    /// The agent answered.
    Healthy(Box<AgentHealth>),
    /// Something answered but it was not a Sentinel agent.
    NotAnAgent {
        /// What went wrong decoding it.
        detail: String,
    },
    /// Nothing is listening.
    Refused,
    /// Nothing answered in time.
    TimedOut,
    /// Something else went wrong.
    Failed {
        /// What went wrong.
        detail: String,
    },
}

impl AgentOutcome {
    /// The probe status this outcome implies.
    pub fn status(&self) -> ProbeStatus {
        match self {
            AgentOutcome::Healthy(_) => ProbeStatus::Ok,
            AgentOutcome::TimedOut => ProbeStatus::Timeout,
            _ => ProbeStatus::Failed,
        }
    }

    /// A short machine-readable discriminator.
    pub fn code(&self) -> &'static str {
        match self {
            AgentOutcome::Healthy(_) => "healthy",
            AgentOutcome::NotAnAgent { .. } => "not_an_agent",
            AgentOutcome::Refused => "refused",
            AgentOutcome::TimedOut => "timed_out",
            AgentOutcome::Failed { .. } => "failed",
        }
    }
}

/// Ask an agent how it is.
pub async fn query(address: &str, port: u16, timeout: Duration) -> AgentOutcome {
    let url = format!("http://{address}:{port}{HEALTH_PATH}");

    let client = match reqwest::Client::builder().timeout(timeout).build() {
        Ok(client) => client,
        Err(error) => {
            return AgentOutcome::Failed {
                detail: error.to_string(),
            }
        }
    };

    let response = match client.get(&url).send().await {
        Ok(response) => response,
        Err(error) if error.is_timeout() => return AgentOutcome::TimedOut,
        Err(error) if error.is_connect() => return AgentOutcome::Refused,
        Err(error) => {
            return AgentOutcome::Failed {
                detail: error.to_string(),
            }
        }
    };

    if !response.status().is_success() {
        return AgentOutcome::Failed {
            detail: format!("agent returned {}", response.status()),
        };
    }

    match response.json::<AgentHealth>().await {
        Ok(health) => AgentOutcome::Healthy(Box::new(health)),
        Err(error) => AgentOutcome::NotAnAgent {
            detail: error.to_string(),
        },
    }
}

/// Checks that a Sentinel agent is answering.
#[derive(Debug, Clone)]
pub struct SentinelAgentProbe {
    definition: ProbeDefinition,
    default_port: u16,
}

impl Default for SentinelAgentProbe {
    fn default() -> Self {
        Self::new()
    }
}

impl SentinelAgentProbe {
    /// A probe with the default schedule.
    pub fn new() -> Self {
        Self {
            definition: ProbeDefinition::new(PROBE_ID)
                .requiring([well_known::SENTINEL_AGENT])
                .targeting([EntityType::Host])
                .every(Duration::from_secs(5))
                .within(Duration::from_secs(3))
                // Only meaningful from somewhere else: an agent asking itself
                // whether it is running can only ever say yes.
                .mode(ExecutionMode::Remote),
            default_port: crate::agent::rpc::DEFAULT_PORT,
        }
    }
}

#[async_trait]
impl Probe for SentinelAgentProbe {
    fn definition(&self) -> &ProbeDefinition {
        &self.definition
    }

    async fn collect(&self, context: &ProbeContext) -> Observation {
        let Some(address) = context.parameter_str("address") else {
            return Observation::new(PROBE_ID.into(), context.target_entity, ProbeStatus::NotApplicable)
                .with_error("no_address", "no address is known for this entity");
        };
        let port = context.parameter_u64("agent_port").unwrap_or(self.default_port as u64) as u16;

        let outcome = query(address, port, context.timeout).await;

        let payload = match &outcome {
            AgentOutcome::Healthy(health) => serde_json::json!({
                "address": address,
                "port": port,
                "outcome": outcome.code(),
                "agent_version": health.agent_version,
                "boot_id": health.boot_id,
                "spooled_observations": health.spooled_observations,
                "registered": health.registered,
                "capabilities": health.capabilities,
                // The agent's own clock, so skew can be measured from a peer
                // as well as from the controller (SPEC.md §108).
                "agent_time": health.timestamp,
            }),
            _ => serde_json::json!({ "address": address, "port": port, "outcome": outcome.code() }),
        };

        let observation =
            Observation::new(PROBE_ID.into(), context.target_entity, outcome.status()).with_payload(payload);

        match &outcome {
            AgentOutcome::Healthy(_) => observation,
            AgentOutcome::NotAnAgent { detail } => observation.with_error("not_an_agent", detail.clone()),
            AgentOutcome::Refused => observation.with_error("refused", "no Sentinel agent is listening on this host"),
            AgentOutcome::TimedOut => observation.with_error("timed_out", "the agent did not answer in time"),
            AgentOutcome::Failed { detail } => observation.with_error("failed", detail.clone()),
        }
    }
}

// Operators may retune this probe's schedule in [probes].
crate::probes::configurable_probe!(SentinelAgentProbe);

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::rpc::{health_snapshot, serve, RpcState, StaticHealth};
    use crate::capability::CapabilitySet;
    use crate::entity::{EntityKey, EntityType};
    use std::sync::Arc;

    fn context(parameters: serde_json::Value) -> ProbeContext {
        ProbeContext::local(
            EntityKey::new("lab", EntityType::Host, "node-a").entity_id(),
            CapabilitySet::new(),
        )
        .with_parameters(parameters)
        .with_timeout(Duration::from_millis(500))
    }

    async fn running_agent() -> crate::agent::rpc::RpcHandle {
        serve(
            "127.0.0.1:0",
            RpcState::new(Arc::new(StaticHealth(health_snapshot(
                "lab",
                "node-a",
                Some("boot-1".into()),
                CapabilitySet::from_iter(["host.metrics"]),
                std::time::Instant::now(),
                7,
                true,
            )))),
        )
        .await
        .expect("serve")
    }

    #[tokio::test]
    async fn a_running_agent_answers_and_identifies_itself() {
        let handle = running_agent().await;
        let observation = SentinelAgentProbe::new()
            .collect(&context(
                serde_json::json!({"address": "127.0.0.1", "agent_port": handle.local_addr.port()}),
            ))
            .await;

        assert_eq!(observation.status, ProbeStatus::Ok);
        assert_eq!(observation.payload["outcome"], "healthy");
        assert_eq!(observation.payload["boot_id"], "boot-1");
        assert_eq!(observation.payload["spooled_observations"], 7);

        handle.shutdown().await;
    }

    #[tokio::test]
    async fn a_stopped_agent_is_reported_as_refused() {
        // The situation `SENTINEL_AGENT_FAILURE` is built on.
        let port = {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("bind");
            listener.local_addr().expect("addr").port()
        };

        let observation = SentinelAgentProbe::new()
            .collect(&context(
                serde_json::json!({"address": "127.0.0.1", "agent_port": port}),
            ))
            .await;

        assert_eq!(observation.status, ProbeStatus::Failed);
        assert_eq!(observation.payload["outcome"], "refused");
    }

    #[tokio::test]
    async fn something_that_is_not_an_agent_is_told_apart_from_no_agent() {
        // A port reused by another service is a different problem from a dead
        // agent, and pretending otherwise sends an operator the wrong way.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let port = listener.local_addr().expect("addr").port();
        tokio::spawn(async move {
            let app = axum::Router::new().route(HEALTH_PATH, axum::routing::get(|| async { "not json" }));
            let _ = axum::serve(listener, app).await;
        });

        let observation = SentinelAgentProbe::new()
            .collect(&context(
                serde_json::json!({"address": "127.0.0.1", "agent_port": port}),
            ))
            .await;

        assert_eq!(observation.status, ProbeStatus::Failed);
        assert_eq!(observation.payload["outcome"], "not_an_agent");
    }

    #[tokio::test]
    async fn an_entity_with_no_address_is_not_applicable() {
        let observation = SentinelAgentProbe::new()
            .collect(&context(serde_json::Value::Null))
            .await;
        assert_eq!(observation.status, ProbeStatus::NotApplicable);
    }

    #[test]
    fn the_probe_only_runs_from_a_peer() {
        // An agent asking itself whether it is running can only say yes.
        assert_eq!(
            SentinelAgentProbe::new().definition().execution_mode,
            ExecutionMode::Remote
        );
    }

    #[test]
    fn the_probe_is_gated_on_the_agent_capability() {
        let definition = SentinelAgentProbe::new().definition().clone();
        assert!(definition.applies_to(EntityType::Host, &CapabilitySet::from_iter(["sentinel.agent"])));
        assert!(!definition.applies_to(EntityType::Host, &CapabilitySet::new()));
    }
}
