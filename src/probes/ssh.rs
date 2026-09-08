//! SSH service health (SPEC.md §60).
//!
//! Checks that something answering the SSH protocol is listening. It does
//! **not** authenticate: Sentinel holds no keys, logs into nothing, and runs
//! nothing over SSH. The question is "can an operator reach this host", and the
//! banner answers it.
//!
//! Telling SSH apart from the host is the point. An operator locked out of a
//! machine that is otherwise perfectly healthy is a common situation, and
//! reporting it as an outage wastes a trip.

use std::time::Duration;

use async_trait::async_trait;
use tokio::io::AsyncReadExt;

use crate::capability::well_known;
use crate::observation::{Observation, ProbeStatus};
use crate::probes::{ExecutionMode, Probe, ProbeContext, ProbeDefinition};

/// Probe id.
pub const PROBE_ID: &str = "ssh.service";
/// Default SSH port.
pub const DEFAULT_PORT: u16 = 22;
/// Longest banner worth reading. Real banners are well under a hundred bytes.
const MAX_BANNER: usize = 512;

/// What the far end said.
#[derive(Debug, Clone, PartialEq)]
pub enum SshOutcome {
    /// A valid SSH identification string was received.
    Banner {
        /// The banner, trimmed.
        banner: String,
    },
    /// Something is listening but it is not SSH.
    NotSsh {
        /// What arrived instead.
        received: String,
    },
    /// The port accepted a connection but sent nothing.
    ///
    /// Distinct from a refusal: an `sshd` that accepts and then stalls is a
    /// different fault from one that is not running, and it is what a host
    /// under severe memory pressure tends to do.
    Silent,
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

impl SshOutcome {
    /// The probe status this outcome implies.
    pub fn status(&self) -> ProbeStatus {
        match self {
            SshOutcome::Banner { .. } => ProbeStatus::Ok,
            // Listening but not speaking SSH is degraded rather than failed:
            // something is there, and it may be a port-forward or a proxy.
            SshOutcome::NotSsh { .. } => ProbeStatus::Degraded,
            SshOutcome::Silent => ProbeStatus::Degraded,
            SshOutcome::TimedOut => ProbeStatus::Timeout,
            SshOutcome::Refused | SshOutcome::Failed { .. } => ProbeStatus::Failed,
        }
    }

    /// A short machine-readable discriminator.
    pub fn code(&self) -> &'static str {
        match self {
            SshOutcome::Banner { .. } => "banner",
            SshOutcome::NotSsh { .. } => "not_ssh",
            SshOutcome::Silent => "silent",
            SshOutcome::Refused => "refused",
            SshOutcome::TimedOut => "timed_out",
            SshOutcome::Failed { .. } => "failed",
        }
    }

    /// Whether something answered on the port at all.
    pub fn something_answered(&self) -> bool {
        !matches!(self, SshOutcome::Refused | SshOutcome::TimedOut)
    }
}

/// Whether a line is an SSH identification string (RFC 4253 §4.2).
pub fn is_ssh_banner(line: &str) -> bool {
    line.starts_with("SSH-")
}

/// Connect and read the identification string.
pub async fn read_banner(address: &str, port: u16, timeout: Duration) -> SshOutcome {
    let target = format!("{address}:{port}");

    let stream = match tokio::time::timeout(timeout, tokio::net::TcpStream::connect(&target)).await {
        Ok(Ok(stream)) => stream,
        Ok(Err(error)) if error.kind() == std::io::ErrorKind::ConnectionRefused => return SshOutcome::Refused,
        Ok(Err(error)) => {
            return SshOutcome::Failed {
                detail: error.to_string(),
            }
        }
        Err(_) => return SshOutcome::TimedOut,
    };

    let mut stream = stream;
    let mut buffer = vec![0u8; MAX_BANNER];

    match tokio::time::timeout(timeout, stream.read(&mut buffer)).await {
        Ok(Ok(0)) => SshOutcome::Silent,
        Ok(Ok(read)) => {
            let text = String::from_utf8_lossy(&buffer[..read]);
            let line = text.lines().next().unwrap_or("").trim_end_matches('\r').trim();
            if is_ssh_banner(line) {
                SshOutcome::Banner {
                    banner: line.to_string(),
                }
            } else {
                SshOutcome::NotSsh {
                    received: line.chars().take(120).collect(),
                }
            }
        }
        Ok(Err(error)) => SshOutcome::Failed {
            detail: error.to_string(),
        },
        Err(_) => SshOutcome::Silent,
    }
}

/// Checks that SSH answers.
#[derive(Debug, Clone)]
pub struct SshProbe {
    definition: ProbeDefinition,
}

impl Default for SshProbe {
    fn default() -> Self {
        Self::new()
    }
}

impl SshProbe {
    /// A probe with the default schedule.
    pub fn new() -> Self {
        Self {
            definition: ProbeDefinition::new(PROBE_ID)
                .requiring([well_known::SSH_SERVER])
                .every(Duration::from_secs(15))
                .within(Duration::from_secs(5))
                .mode(ExecutionMode::Either),
        }
    }
}

#[async_trait]
impl Probe for SshProbe {
    fn definition(&self) -> &ProbeDefinition {
        &self.definition
    }

    async fn collect(&self, context: &ProbeContext) -> Observation {
        let Some(address) = context.parameter_str("address") else {
            return Observation::new(PROBE_ID.into(), context.target_entity, ProbeStatus::NotApplicable)
                .with_error("no_address", "no address is known for this entity");
        };
        let port = context.parameter_u64("ssh_port").unwrap_or(DEFAULT_PORT as u64) as u16;

        let outcome = read_banner(address, port, context.timeout).await;

        let observation = Observation::new(PROBE_ID.into(), context.target_entity, outcome.status()).with_payload(
            serde_json::json!({
                "address": address,
                "port": port,
                "outcome": outcome.code(),
                "something_answered": outcome.something_answered(),
                "banner": match &outcome {
                    SshOutcome::Banner { banner } => Some(banner.clone()),
                    _ => None,
                },
            }),
        );

        match &outcome {
            SshOutcome::Banner { .. } => observation,
            SshOutcome::NotSsh { received } => {
                observation.with_error("not_ssh", format!("something is listening but said {received:?}"))
            }
            SshOutcome::Silent => {
                observation.with_error("silent", "the port accepted a connection but sent no identification")
            }
            SshOutcome::Refused => observation.with_error("refused", "nothing is listening on the SSH port"),
            SshOutcome::TimedOut => observation.with_error("timed_out", "the SSH port did not answer in time"),
            SshOutcome::Failed { detail } => observation.with_error("failed", detail.clone()),
        }
    }
}

// Operators may retune this probe's schedule in [probes].
crate::probes::configurable_probe!(SshProbe);

#[cfg(test)]
mod tests {
    use super::*;
    use crate::capability::CapabilitySet;
    use crate::entity::{EntityKey, EntityType};
    use tokio::io::AsyncWriteExt;

    fn context(parameters: serde_json::Value) -> ProbeContext {
        ProbeContext::local(
            EntityKey::new("lab", EntityType::Host, "node-a").entity_id(),
            CapabilitySet::new(),
        )
        .with_parameters(parameters)
        .with_timeout(Duration::from_millis(500))
    }

    /// A listener that sends `response` and then closes.
    async fn fake_server(response: Option<&'static [u8]>) -> u16 {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let port = listener.local_addr().expect("addr").port();
        tokio::spawn(async move {
            while let Ok((mut stream, _)) = listener.accept().await {
                if let Some(response) = response {
                    let _ = stream.write_all(response).await;
                }
                let _ = stream.shutdown().await;
            }
        });
        port
    }

    #[test]
    fn banners_are_recognised_by_their_prefix() {
        assert!(is_ssh_banner("SSH-2.0-OpenSSH_9.6p1 Ubuntu-3ubuntu13"));
        assert!(is_ssh_banner("SSH-1.99-OpenSSH_3.9p1"));
        assert!(!is_ssh_banner("HTTP/1.1 200 OK"));
        assert!(!is_ssh_banner(""));
        assert!(!is_ssh_banner("220 mail.example.org ESMTP"));
    }

    #[tokio::test]
    async fn a_real_looking_banner_is_accepted() {
        let port = fake_server(Some(b"SSH-2.0-OpenSSH_9.6p1\r\n")).await;
        let outcome = read_banner("127.0.0.1", port, Duration::from_secs(2)).await;

        assert_eq!(
            outcome,
            SshOutcome::Banner {
                banner: "SSH-2.0-OpenSSH_9.6p1".into()
            }
        );
        assert_eq!(outcome.status(), ProbeStatus::Ok);
    }

    #[tokio::test]
    async fn a_port_with_no_listener_is_refused_and_that_is_a_failure() {
        let port = {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("bind");
            listener.local_addr().expect("addr").port()
        };

        let outcome = read_banner("127.0.0.1", port, Duration::from_secs(2)).await;
        assert_eq!(outcome, SshOutcome::Refused);
        assert_eq!(outcome.status(), ProbeStatus::Failed);
        assert!(!outcome.something_answered());
    }

    #[tokio::test]
    async fn something_that_is_not_ssh_is_degraded_not_failed() {
        // Something is listening. That is a different situation from nothing
        // listening, and worth telling apart.
        let port = fake_server(Some(b"HTTP/1.1 200 OK\r\n")).await;
        let outcome = read_banner("127.0.0.1", port, Duration::from_secs(2)).await;

        assert!(matches!(outcome, SshOutcome::NotSsh { .. }), "{outcome:?}");
        assert_eq!(outcome.status(), ProbeStatus::Degraded);
        assert!(outcome.something_answered());
    }

    #[tokio::test]
    async fn a_port_that_accepts_but_says_nothing_is_degraded() {
        // What an sshd under severe memory pressure tends to do.
        let port = fake_server(None).await;
        let outcome = read_banner("127.0.0.1", port, Duration::from_millis(300)).await;

        assert_eq!(outcome, SshOutcome::Silent);
        assert_eq!(outcome.status(), ProbeStatus::Degraded);
        assert!(
            outcome.something_answered(),
            "the connection was accepted, so something is alive"
        );
    }

    #[tokio::test]
    async fn an_over_long_banner_is_truncated_rather_than_read_forever() {
        let port = fake_server(Some(&[b'S'; 100_000])).await;
        let outcome = read_banner("127.0.0.1", port, Duration::from_secs(2)).await;

        match outcome {
            SshOutcome::NotSsh { received } => assert!(received.len() <= 120, "{}", received.len()),
            other => panic!("unexpected outcome {other:?}"),
        }
    }

    #[tokio::test]
    async fn the_probe_reports_the_banner_it_saw() {
        let port = fake_server(Some(b"SSH-2.0-OpenSSH_9.6p1\r\n")).await;
        let observation = SshProbe::new()
            .collect(&context(serde_json::json!({"address": "127.0.0.1", "ssh_port": port})))
            .await;

        assert_eq!(observation.status, ProbeStatus::Ok);
        assert_eq!(observation.payload["banner"], "SSH-2.0-OpenSSH_9.6p1");
        assert_eq!(observation.payload["outcome"], "banner");
    }

    #[tokio::test]
    async fn a_stopped_sshd_is_reported_as_a_service_failure() {
        let port = {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("bind");
            listener.local_addr().expect("addr").port()
        };
        let observation = SshProbe::new()
            .collect(&context(serde_json::json!({"address": "127.0.0.1", "ssh_port": port})))
            .await;

        assert_eq!(observation.status, ProbeStatus::Failed);
        assert_eq!(observation.payload["outcome"], "refused");
        assert_eq!(observation.payload["something_answered"], false);
    }

    #[tokio::test]
    async fn an_entity_with_no_address_is_not_applicable() {
        let observation = SshProbe::new().collect(&context(serde_json::Value::Null)).await;
        assert_eq!(observation.status, ProbeStatus::NotApplicable);
    }

    #[test]
    fn the_probe_is_gated_on_the_ssh_capability() {
        let definition = SshProbe::new().definition().clone();
        assert!(definition.applies_to(EntityType::Host, &CapabilitySet::from_iter(["ssh.server"])));
        assert!(!definition.applies_to(EntityType::Host, &CapabilitySet::new()));
    }

    #[test]
    fn the_probe_never_authenticates_or_executes_anything() {
        // Sentinel holds no keys, logs into nothing, and runs nothing over SSH
        // (SPEC.md §60, §116). Asserted against the implementation so it stays
        // true as the file grows; the test module itself is excluded, since it
        // has to name the things it is forbidding.
        let source = include_str!("ssh.rs");
        let implementation = source.split("#[cfg(test)]").next().expect("implementation");

        // Code-level markers only. Prose words like "authenticate" appear in
        // the module documentation precisely because it explains that the probe
        // does not, so matching on them would fail on the comment that states
        // the guarantee.
        for forbidden in ["private_key", "Command::new", "std::process", "spawn_blocking"] {
            assert!(
                !implementation.contains(forbidden),
                "the SSH probe implementation must not contain {forbidden}"
            );
        }
    }
}
