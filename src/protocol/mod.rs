//! The controller/agent wire protocol.
//!
//! Versioned JSON over HTTP (IMPLEMENTATION.md §61-§63). The protocol version
//! is independent of the binary version: a fleet mid-upgrade runs mixed binary
//! versions speaking one protocol.
//!
//! Compatibility rules this module is built to keep:
//!
//! * Every request declares its `protocol_version`; the controller refuses one
//!   it does not speak rather than guessing.
//! * Response types tolerate unknown fields, so an older agent survives a newer
//!   controller adding one.
//! * Nothing here can express "run this command". Probes are compiled in; the
//!   RPC surface is deliberately incapable of remote execution (SPEC.md §116).

mod auth;
pub mod tls;

pub use auth::{AuthError, ClusterCredential};

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::capability::CapabilitySet;
use crate::observation::Observation;
use crate::time::Timestamp;
use crate::PROTOCOL_VERSION;

/// URL prefix for this protocol version.
pub const API_PREFIX: &str = "/v1";

/// Header carrying the cluster credential.
pub const AUTH_HEADER: &str = "authorization";

/// A protocol version mismatch.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VersionMismatch {
    /// What the peer offered.
    pub offered: u32,
    /// What this build speaks.
    pub supported: u32,
}

/// What an agent reports about the machine it runs on (IMPLEMENTATION.md §64).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RegisterRequest {
    /// Protocol version the agent speaks.
    pub protocol_version: u32,
    /// Agent binary version.
    pub agent_version: String,
    /// Environment the agent believes it belongs to.
    pub environment: String,
    /// Host name, used as the entity's canonical name.
    pub hostname: String,
    /// Fully qualified name, when known.
    #[serde(default)]
    pub fqdn: Option<String>,
    /// Linux boot id. A change means the machine rebooted (SPEC.md §107).
    #[serde(default)]
    pub boot_id: Option<String>,
    /// Addresses this host answers on. Never identity (SPEC.md §37).
    #[serde(default)]
    pub addresses: Vec<String>,
    /// Ports this host's services listen on, where they are not the defaults.
    ///
    /// Reported by the agent rather than assumed by the controller: the host
    /// is the only thing that actually knows, and a peer probing the wrong
    /// port would report a service down that is running perfectly.
    #[serde(default)]
    pub ports: std::collections::BTreeMap<String, u16>,
    /// Capabilities discovered at runtime.
    pub capabilities: CapabilitySet,
    /// Hardware summary, for comparison against scheduler configuration.
    #[serde(default)]
    pub hardware: serde_json::Value,
    /// Operator-declared roles. Grouping only; these never enable a probe.
    #[serde(default)]
    pub roles: Vec<String>,
}

impl RegisterRequest {
    /// A registration for this build.
    pub fn new(environment: impl Into<String>, hostname: impl Into<String>, capabilities: CapabilitySet) -> Self {
        Self {
            protocol_version: PROTOCOL_VERSION,
            agent_version: crate::VERSION.to_string(),
            environment: environment.into(),
            hostname: hostname.into(),
            fqdn: None,
            boot_id: None,
            addresses: Vec::new(),
            ports: std::collections::BTreeMap::new(),
            capabilities,
            hardware: serde_json::Value::Null,
            roles: Vec::new(),
        }
    }
}

/// What the controller tells an agent after registration.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RegisterResponse {
    /// Protocol version the controller speaks.
    pub protocol_version: u32,
    /// The agent's stable identity.
    pub agent_id: Uuid,
    /// This registration's session, restarted on every reconnect or reboot.
    pub session_id: Uuid,
    /// The entity id the controller assigned to this host.
    pub entity_id: String,
    /// The controller's clock, for skew detection (SPEC.md §108).
    pub server_time: Timestamp,
    /// How often to heartbeat.
    #[serde(with = "humantime_serde")]
    pub heartbeat_interval: std::time::Duration,
}

/// A periodic liveness report.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HeartbeatRequest {
    /// Protocol version.
    pub protocol_version: u32,
    /// The agent's identity.
    pub agent_id: Uuid,
    /// The current session.
    pub session_id: Uuid,
    /// Current boot id, so a reboot is visible even if registration was missed.
    #[serde(default)]
    pub boot_id: Option<String>,
    /// The agent's clock.
    pub agent_time: Timestamp,
    /// Observations waiting in the local spool.
    #[serde(default)]
    pub spooled_observations: u64,
}

/// The controller's reply to a heartbeat.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HeartbeatResponse {
    /// Protocol version.
    pub protocol_version: u32,
    /// The controller's clock.
    pub server_time: Timestamp,
    /// Measured wall-clock difference, in milliseconds.
    pub clock_skew_ms: i64,
    /// Whether the agent should register again (unknown session, restart).
    pub reregister: bool,
    /// Current peer assignment revision, so an agent can tell it is stale.
    #[serde(default)]
    pub assignment_revision: u64,
}

/// A batch of observations from an agent.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ObservationBatch {
    /// Protocol version.
    pub protocol_version: u32,
    /// The reporting agent.
    pub agent_id: Uuid,
    /// The current session.
    pub session_id: Uuid,
    /// The observations. Ids are agent-generated, making replay idempotent.
    pub observations: Vec<Observation>,
}

/// What the controller did with a batch.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ObservationBatchResponse {
    /// Protocol version.
    pub protocol_version: u32,
    /// Observations stored for the first time.
    pub accepted: usize,
    /// Observations already present.
    pub duplicates: usize,
    /// Observations rejected, with the reason.
    #[serde(default)]
    pub rejected: Vec<RejectedObservation>,
}

/// One observation the controller would not store.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RejectedObservation {
    /// The observation's id.
    pub id: String,
    /// Why it was rejected.
    pub reason: String,
}

/// What one agent has been asked to observe (SPEC.md §45, §66).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AssignmentsResponse {
    /// Protocol version.
    pub protocol_version: u32,
    /// The plan revision, so an agent can tell its own is stale.
    pub revision: u64,
    /// The entities this agent should observe.
    pub targets: Vec<AssignedTarget>,
}

/// One entity an agent should observe, and how to reach it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AssignedTarget {
    /// The entity's id.
    pub entity_id: String,
    /// Its canonical name, for logging.
    pub name: String,
    /// Where to reach it. Resolved by the controller from inventory, so an
    /// agent never has to guess an address (SPEC.md §37).
    pub address: String,
    /// Probe parameters, including any non-default ports.
    pub parameters: serde_json::Value,
    /// The target's capabilities, which decide what to probe.
    pub capabilities: CapabilitySet,
}

/// The controller's own health, for peers watching it (SPEC.md §109).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HealthResponse {
    /// Protocol version.
    pub protocol_version: u32,
    /// Controller binary version.
    pub version: String,
    /// The environment served.
    pub environment: String,
    /// The controller's clock.
    pub server_time: Timestamp,
    /// Entities known.
    pub entities: usize,
    /// Agents that have registered.
    pub agents: usize,
}

/// An error, in the shape every endpoint uses.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ErrorResponse {
    /// Machine-readable discriminator.
    pub error: String,
    /// Human-readable detail.
    pub message: String,
}

impl ErrorResponse {
    /// Build an error response.
    pub fn new(error: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            error: error.into(),
            message: message.into(),
        }
    }
}

/// Check a peer's protocol version against this build's.
pub fn check_version(offered: u32) -> Result<(), VersionMismatch> {
    if offered == PROTOCOL_VERSION {
        Ok(())
    } else {
        Err(VersionMismatch {
            offered,
            supported: PROTOCOL_VERSION,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_matching_protocol_version_is_accepted() {
        assert!(check_version(PROTOCOL_VERSION).is_ok());
    }

    #[test]
    fn a_mismatched_protocol_version_is_refused_with_both_numbers() {
        let mismatch = check_version(PROTOCOL_VERSION + 1).expect_err("must refuse");
        assert_eq!(mismatch.offered, PROTOCOL_VERSION + 1);
        assert_eq!(mismatch.supported, PROTOCOL_VERSION);

        assert!(check_version(0).is_err(), "an older protocol is refused too");
    }

    #[test]
    fn a_registration_round_trips_through_json() {
        let request = RegisterRequest::new("lab", "node-a", ["host.metrics", "slurm.compute"].into_iter().collect());
        let text = serde_json::to_string(&request).expect("serialize");
        assert_eq!(
            serde_json::from_str::<RegisterRequest>(&text).expect("deserialize"),
            request
        );
    }

    #[test]
    fn an_older_agent_survives_a_newer_controller_adding_a_field() {
        // Unknown fields must be ignored, not rejected: otherwise a controller
        // upgrade takes the whole fleet offline.
        let json = serde_json::json!({
            "protocol_version": PROTOCOL_VERSION,
            "agent_version": "0.3.0",
            "environment": "lab",
            "hostname": "node-a",
            "capabilities": ["host.metrics"],
            "some_future_field": {"nested": true},
        });
        let request: RegisterRequest = serde_json::from_value(json).expect("tolerates unknown fields");
        assert_eq!(request.hostname, "node-a");
    }

    #[test]
    fn optional_registration_fields_may_be_omitted() {
        let json = serde_json::json!({
            "protocol_version": PROTOCOL_VERSION,
            "agent_version": "0.3.0",
            "environment": "lab",
            "hostname": "node-a",
            "capabilities": [],
        });
        let request: RegisterRequest = serde_json::from_value(json).expect("deserialize");
        assert_eq!(request.boot_id, None);
        assert!(request.addresses.is_empty());
        assert!(request.roles.is_empty());
    }

    #[test]
    fn the_protocol_cannot_express_a_command_to_run() {
        // SPEC.md §116. This is a property of the type surface, so it is worth
        // asserting rather than trusting to review.
        let schema = [
            serde_json::to_string(&RegisterRequest::new("lab", "n", CapabilitySet::new())).unwrap(),
            serde_json::to_string(&HeartbeatRequest {
                protocol_version: PROTOCOL_VERSION,
                agent_id: Uuid::nil(),
                session_id: Uuid::nil(),
                boot_id: None,
                agent_time: crate::time::now(),
                spooled_observations: 0,
            })
            .unwrap(),
        ]
        .join(" ");

        for forbidden in ["command", "exec", "script", "shell", "argv"] {
            assert!(
                !schema.contains(forbidden),
                "the wire format must not carry {forbidden}"
            );
        }
    }

    #[test]
    fn the_api_prefix_matches_the_protocol_version() {
        assert_eq!(API_PREFIX, format!("/v{PROTOCOL_VERSION}"));
    }

    #[test]
    fn durations_on_the_wire_are_human_readable() {
        let response = RegisterResponse {
            protocol_version: PROTOCOL_VERSION,
            agent_id: Uuid::nil(),
            session_id: Uuid::nil(),
            entity_id: "x".into(),
            server_time: crate::time::now(),
            heartbeat_interval: std::time::Duration::from_secs(5),
        };
        let text = serde_json::to_string(&response).expect("serialize");
        assert!(text.contains("\"5s\""), "{text}");
        assert_eq!(
            serde_json::from_str::<RegisterResponse>(&text).expect("deserialize"),
            response
        );
    }
}
