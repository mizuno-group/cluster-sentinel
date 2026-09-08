//! Network reachability (SPEC.md §59).
//!
//! TCP rather than ICMP by default: ICMP is frequently filtered, needs
//! privileges to send raw, and answers a different question. What Sentinel
//! wants to know is whether a service can be reached, and a TCP handshake
//! answers exactly that.
//!
//! A failure here says *this observer could not reach this address*. It does
//! not say the host is down. Deciding that needs several observers to agree,
//! which is why observations carry who made them (SPEC.md §50).

use std::net::ToSocketAddrs;
use std::time::{Duration, Instant};

use async_trait::async_trait;

use crate::capability::well_known;
use crate::entity::EntityType;
use crate::observation::{Observation, ProbeStatus};
use crate::probes::{ExecutionMode, Probe, ProbeContext, ProbeDefinition};

/// Probe id.
pub const PROBE_ID: &str = "network.tcp";

/// The outcome of one connection attempt.
#[derive(Debug, Clone, PartialEq)]
pub enum ConnectOutcome {
    /// The handshake completed.
    Connected {
        /// How long it took.
        latency: Duration,
    },
    /// The address could not be resolved.
    ///
    /// Distinguished from a refusal because it usually means a DNS problem
    /// rather than a host problem, and sends an operator somewhere different.
    Unresolvable {
        /// What went wrong.
        detail: String,
    },
    /// The host answered, actively refusing the connection.
    ///
    /// Strong evidence that the host is *up*: something replied.
    Refused,
    /// Nothing answered within the timeout.
    TimedOut,
    /// Something else went wrong.
    Failed {
        /// What went wrong.
        detail: String,
    },
}

impl ConnectOutcome {
    /// The probe status this outcome implies.
    pub fn status(&self) -> ProbeStatus {
        match self {
            ConnectOutcome::Connected { .. } => ProbeStatus::Ok,
            ConnectOutcome::TimedOut => ProbeStatus::Timeout,
            _ => ProbeStatus::Failed,
        }
    }

    /// A short machine-readable discriminator.
    pub fn code(&self) -> &'static str {
        match self {
            ConnectOutcome::Connected { .. } => "connected",
            ConnectOutcome::Unresolvable { .. } => "unresolvable",
            ConnectOutcome::Refused => "refused",
            ConnectOutcome::TimedOut => "timed_out",
            ConnectOutcome::Failed { .. } => "failed",
        }
    }

    /// Whether this outcome is evidence that something is listening *and*
    /// something is there at all.
    ///
    /// A refusal proves the host is alive: a dead host does not send RST.
    pub fn proves_host_responds(&self) -> bool {
        matches!(self, ConnectOutcome::Connected { .. } | ConnectOutcome::Refused)
    }
}

/// Attempt a TCP connection.
pub async fn connect(address: &str, port: u16, timeout: Duration) -> ConnectOutcome {
    let target = format!("{address}:{port}");

    // Resolution is blocking, so it runs on the blocking pool with the same
    // deadline as the connection itself.
    let resolved = {
        let owned = target.clone();
        match tokio::time::timeout(timeout, tokio::task::spawn_blocking(move || owned.to_socket_addrs())).await {
            Ok(Ok(Ok(mut addresses))) => match addresses.next() {
                Some(address) => address,
                None => {
                    return ConnectOutcome::Unresolvable {
                        detail: format!("{target} resolved to no addresses"),
                    }
                }
            },
            Ok(Ok(Err(error))) => {
                return ConnectOutcome::Unresolvable {
                    detail: error.to_string(),
                }
            }
            Ok(Err(error)) => {
                return ConnectOutcome::Failed {
                    detail: error.to_string(),
                }
            }
            Err(_) => return ConnectOutcome::TimedOut,
        }
    };

    let started = Instant::now();
    match tokio::time::timeout(timeout, tokio::net::TcpStream::connect(resolved)).await {
        Ok(Ok(_stream)) => ConnectOutcome::Connected {
            latency: started.elapsed(),
        },
        Ok(Err(error)) if error.kind() == std::io::ErrorKind::ConnectionRefused => ConnectOutcome::Refused,
        Ok(Err(error)) => ConnectOutcome::Failed {
            detail: error.to_string(),
        },
        Err(_) => ConnectOutcome::TimedOut,
    }
}

/// Checks that a TCP port answers.
#[derive(Debug, Clone)]
pub struct TcpProbe {
    definition: ProbeDefinition,
    default_port: u16,
    refusal_is_reachable: bool,
    /// Probe parameters consulted for a port, in order of preference.
    ///
    /// Reachability asks for the host's SSH port before falling back to the
    /// default, because that is a port something is known to be listening on.
    /// See [`TcpProbe::reachability`].
    port_parameters: &'static [&'static str],
}

impl TcpProbe {
    /// A probe that checks a specific service's port.
    ///
    /// A refusal is a failure here: the service is supposed to be listening.
    pub fn new(probe_id: &str, default_port: u16) -> Self {
        Self {
            definition: ProbeDefinition::new(probe_id)
                .requiring([well_known::NETWORK_TCP])
                .every(Duration::from_secs(5))
                .within(Duration::from_secs(3))
                .mode(ExecutionMode::Either),
            default_port,
            refusal_is_reachable: false,
            port_parameters: &["port"],
        }
    }

    /// The generic reachability probe: can this observer reach this host at all.
    ///
    /// A **refusal counts as success**, and that is the whole point. A refusal
    /// is a packet: something at that address received the connection and
    /// answered. Treating it as a network failure would mean that stopping
    /// `sshd` marked the network unreachable, which then makes it impossible to
    /// tell a service failure from a dead host — the single distinction this
    /// system exists to make.
    ///
    /// **This probe requires no capability**, unlike every other one. Opening a
    /// TCP connection needs nothing installed on the far end; it needs an
    /// address, which the caller has already found or it would not be asking.
    /// Gating it behind a capability would mean a host only gets checked once
    /// something on it reports that it can be checked — so a cluster whose
    /// nodes come from Slurm discovery, with no agent yet, would be listed as
    /// healthy without anyone ever having contacted it. Capabilities gate
    /// probes that need something *present* (`nvidia-smi`, `journalctl`, an
    /// NFS export). Reachability is not one of them.
    /// It asks the host's **configured SSH port**, not port 22.
    ///
    /// Aiming at 22 regardless is only harmless where nothing is listening
    /// there and the kernel refuses, which reads as reachable. Where a
    /// firewall drops instead of refusing -- the ordinary configuration on a
    /// cluster that has moved SSH elsewhere -- the probe times out and a
    /// perfectly reachable host is reported unreachable. A whole cluster was.
    ///
    /// So it asks a port something is known to answer on. That is still a
    /// different question from the SSH probe's, which wants a banner: a
    /// refusal here is success, because a refusal is a packet.
    pub fn reachability() -> Self {
        Self {
            refusal_is_reachable: true,
            port_parameters: &["ssh_port", "port"],
            definition: ProbeDefinition::new(PROBE_ID)
                .targeting([EntityType::Host])
                .every(Duration::from_secs(5))
                .within(Duration::from_secs(3))
                .mode(ExecutionMode::Either),
            ..Self::new(PROBE_ID, 22)
        }
    }

    /// Builder: replace the definition.
    pub fn with_definition(mut self, definition: ProbeDefinition) -> Self {
        self.definition = definition;
        self
    }

    /// The status an outcome implies for this probe.
    fn status_for(&self, outcome: &ConnectOutcome) -> ProbeStatus {
        if self.refusal_is_reachable && outcome.proves_host_responds() {
            return ProbeStatus::Ok;
        }
        outcome.status()
    }
}

#[async_trait]
impl Probe for TcpProbe {
    fn definition(&self) -> &ProbeDefinition {
        &self.definition
    }

    async fn collect(&self, context: &ProbeContext) -> Observation {
        // The address is a parameter, resolved from configuration or discovery.
        // A probe that knew a host name would be a probe with the deployment
        // compiled into it (IMPLEMENTATION.md §101).
        let Some(address) = context.parameter_str("address") else {
            return Observation::new(
                self.definition.id.clone(),
                context.target_entity,
                ProbeStatus::NotApplicable,
            )
            .with_error("no_address", "no address is known for this entity");
        };
        let port = self
            .port_parameters
            .iter()
            .find_map(|name| context.parameter_u64(name))
            .unwrap_or(self.default_port as u64) as u16;

        let outcome = connect(address, port, context.timeout).await;
        let latency_ms = match &outcome {
            ConnectOutcome::Connected { latency } => Some(latency.as_millis() as u64),
            _ => None,
        };

        let status = self.status_for(&outcome);
        let observation = Observation::new(self.definition.id.clone(), context.target_entity, status).with_payload(
            serde_json::json!({
                "address": address,
                "port": port,
                "outcome": outcome.code(),
                "latency_ms": latency_ms,
                "host_responded": outcome.proves_host_responds(),
            }),
        );

        // A refusal that counts as reachable is a success, not an error worth
        // attaching a message to.
        if status == ProbeStatus::Ok {
            return observation;
        }

        match &outcome {
            ConnectOutcome::Connected { .. } => observation,
            ConnectOutcome::Unresolvable { detail } => observation.with_error("unresolvable", detail.clone()),
            ConnectOutcome::Refused => {
                observation.with_error("refused", format!("{address}:{port} refused the connection"))
            }
            ConnectOutcome::TimedOut => {
                observation.with_error("timed_out", format!("{address}:{port} did not answer in time"))
            }
            ConnectOutcome::Failed { detail } => observation.with_error("failed", detail.clone()),
        }
    }
}

// Operators may retune this probe's schedule in [probes].
crate::probes::configurable_probe!(TcpProbe);

#[cfg(test)]
mod tests {
    use super::*;
    use crate::capability::CapabilitySet;
    use crate::entity::{EntityKey, EntityType};

    fn entity() -> crate::entity::EntityId {
        EntityKey::new("lab", EntityType::Host, "node-a").entity_id()
    }

    fn context(parameters: serde_json::Value) -> ProbeContext {
        ProbeContext::local(entity(), CapabilitySet::new())
            .with_parameters(parameters)
            .with_timeout(Duration::from_millis(500))
    }

    #[tokio::test]
    async fn a_listening_port_connects() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let port = listener.local_addr().expect("addr").port();

        let outcome = connect("127.0.0.1", port, Duration::from_secs(2)).await;
        assert!(matches!(outcome, ConnectOutcome::Connected { .. }), "{outcome:?}");
        assert_eq!(outcome.status(), ProbeStatus::Ok);
        assert!(outcome.proves_host_responds());
    }

    #[tokio::test]
    async fn a_closed_port_is_refused_which_proves_the_host_is_alive() {
        // This distinction matters: a refusal is positive evidence about the
        // host, even though it is a failure for the service.
        let port = {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("bind");
            listener.local_addr().expect("addr").port()
        };

        let outcome = connect("127.0.0.1", port, Duration::from_secs(2)).await;
        assert_eq!(outcome, ConnectOutcome::Refused);
        assert_eq!(outcome.status(), ProbeStatus::Failed);
        assert!(
            outcome.proves_host_responds(),
            "something sent us a refusal, so something is there"
        );
    }

    #[tokio::test]
    async fn an_unresolvable_name_is_told_apart_from_an_unreachable_host() {
        // A DNS problem sends an operator somewhere entirely different from a
        // host problem.
        let outcome = connect("no-such-host.invalid", 22, Duration::from_secs(3)).await;
        assert!(
            matches!(outcome, ConnectOutcome::Unresolvable { .. } | ConnectOutcome::TimedOut),
            "{outcome:?}"
        );
        assert!(!outcome.proves_host_responds());
    }

    #[tokio::test]
    async fn an_unroutable_address_times_out_rather_than_hanging() {
        // 203.0.113.0/24 is reserved for documentation and is not routed.
        let started = Instant::now();
        let outcome = connect("203.0.113.1", 22, Duration::from_millis(300)).await;
        assert!(
            matches!(outcome, ConnectOutcome::TimedOut | ConnectOutcome::Failed { .. }),
            "{outcome:?}"
        );
        assert!(started.elapsed() < Duration::from_secs(5));
    }

    #[tokio::test]
    async fn the_probe_reports_a_successful_connection_with_its_latency() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let port = listener.local_addr().expect("addr").port();

        let observation = TcpProbe::reachability()
            .collect(&context(serde_json::json!({"address": "127.0.0.1", "port": port})))
            .await;

        assert_eq!(observation.status, ProbeStatus::Ok);
        assert_eq!(observation.payload["outcome"], "connected");
        assert_eq!(observation.payload["host_responded"], true);
        assert!(observation.payload["latency_ms"].is_number());
    }

    #[tokio::test]
    async fn an_entity_with_no_address_is_not_applicable_rather_than_failed() {
        // Not knowing where something is is not evidence that it is broken.
        let observation = TcpProbe::reachability()
            .collect(&context(serde_json::Value::Null))
            .await;
        assert_eq!(observation.status, ProbeStatus::NotApplicable);
        assert!(!observation.status.is_bad());
    }

    #[tokio::test]
    async fn the_default_port_is_used_when_none_is_given() {
        let observation = TcpProbe::new("test.tcp", 12345)
            .collect(&context(serde_json::json!({"address": "127.0.0.1"})))
            .await;
        assert_eq!(observation.payload["port"], 12345);
    }

    #[tokio::test]
    async fn a_refused_port_still_counts_as_reachable_for_the_reachability_probe() {
        // Stopping sshd must not make the host look unreachable: the refusal
        // proves the path works, and conflating the two would destroy the
        // distinction between a service failure and a dead host.
        let port = {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("bind");
            listener.local_addr().expect("addr").port()
        };

        let observation = TcpProbe::reachability()
            .collect(&context(serde_json::json!({"address": "127.0.0.1", "port": port})))
            .await;

        assert_eq!(observation.status, ProbeStatus::Ok, "a refusal is an answer");
        assert_eq!(observation.payload["outcome"], "refused");
        assert_eq!(observation.payload["host_responded"], true);
        assert_eq!(observation.error_code, None);
    }

    #[tokio::test]
    async fn a_service_probe_treats_the_same_refusal_as_a_failure() {
        // The same packet means something different when the question is
        // "is this service up" rather than "can I reach this host".
        let port = {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("bind");
            listener.local_addr().expect("addr").port()
        };

        let observation = TcpProbe::new("service.tcp", port)
            .collect(&context(serde_json::json!({"address": "127.0.0.1", "port": port})))
            .await;

        assert_eq!(observation.status, ProbeStatus::Failed);
        assert_eq!(observation.error_code.as_deref(), Some("refused"));
    }

    #[tokio::test]
    async fn an_unanswered_address_is_a_reachability_failure() {
        // No answer at all is the only thing that means "cannot reach".
        let observation = TcpProbe::reachability()
            .collect(&context(serde_json::json!({"address": "203.0.113.1", "port": 22})))
            .await;

        assert!(observation.status.is_bad(), "{:?}", observation.status);
        assert_eq!(observation.payload["host_responded"], false);
    }

    #[test]
    fn a_probe_carries_no_host_name_of_its_own() {
        // The address must arrive as a parameter, never as a constant.
        let definition = TcpProbe::reachability().definition().clone();
        assert!(definition.applies_to(EntityType::Host, &CapabilitySet::from_iter(["network.tcp"])));
        assert_eq!(
            definition.execution_mode,
            ExecutionMode::Either,
            "this probe also runs from a peer"
        );
    }
}
