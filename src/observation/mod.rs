//! Observations: immutable facts reported by probes.
//!
//! An observation records *what was seen*, never *what it means*. A probe must
//! not emit `HOST_UNREACHABLE` or `SHARED_STORAGE_FAILURE` — those are
//! conclusions the diagnosis engine draws from many observations
//! (IMPLEMENTATION.md §68).
//!
//! Stored observations are never rewritten (IMPLEMENTATION.md §44); only
//! derived state, diagnoses and incident correlation change afterwards.

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::entity::EntityId;
use crate::probes::ProbeId;
use crate::time::{now, Timestamp};

/// Globally unique observation identifier, used to make ingestion idempotent
/// when an agent replays its spool (IMPLEMENTATION.md §45).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ObservationId(Uuid);

impl ObservationId {
    /// Allocate a fresh identifier.
    #[allow(clippy::new_without_default)]
    pub fn new() -> Self {
        Self(Uuid::new_v4())
    }

    /// Wrap an existing UUID.
    pub fn from_uuid(uuid: Uuid) -> Self {
        Self(uuid)
    }

    /// The underlying UUID.
    pub fn as_uuid(&self) -> Uuid {
        self.0
    }
}

impl fmt::Display for ObservationId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl FromStr for ObservationId {
    type Err = uuid::Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Ok(Self(Uuid::parse_str(s)?))
    }
}

/// Outcome of one probe execution (SPEC.md §55).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProbeStatus {
    /// The probe succeeded and everything it measured looks normal.
    Ok,
    /// The probe succeeded but the result is outside the healthy range.
    Degraded,
    /// The probe ran and the thing it measures is broken.
    Failed,
    /// The probe did not complete within its timeout.
    Timeout,
    /// The probe is blocked in a way a timeout cannot cancel (NFS D-state).
    Stuck,
    /// This probe cannot run here (missing tool, missing permission).
    Unsupported,
    /// This probe does not apply to this entity at all.
    NotApplicable,
}

impl ProbeStatus {
    /// Stable string form used in the database and on the wire.
    pub fn as_str(&self) -> &'static str {
        match self {
            ProbeStatus::Ok => "ok",
            ProbeStatus::Degraded => "degraded",
            ProbeStatus::Failed => "failed",
            ProbeStatus::Timeout => "timeout",
            ProbeStatus::Stuck => "stuck",
            ProbeStatus::Unsupported => "unsupported",
            ProbeStatus::NotApplicable => "not_applicable",
        }
    }

    /// Parse the stable string form.
    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "ok" => ProbeStatus::Ok,
            "degraded" => ProbeStatus::Degraded,
            "failed" => ProbeStatus::Failed,
            "timeout" => ProbeStatus::Timeout,
            "stuck" => ProbeStatus::Stuck,
            "unsupported" => ProbeStatus::Unsupported,
            "not_applicable" => ProbeStatus::NotApplicable,
            _ => return None,
        })
    }

    /// Whether this status is evidence that the measured thing is broken.
    ///
    /// [`ProbeStatus::Unsupported`] and [`ProbeStatus::NotApplicable`] are
    /// explicitly *not* failures: a node without GPUs is not a broken node.
    pub fn is_bad(&self) -> bool {
        matches!(self, ProbeStatus::Failed | ProbeStatus::Timeout | ProbeStatus::Stuck)
    }

    /// Whether this status carries any signal about health at all.
    pub fn is_conclusive(&self) -> bool {
        !matches!(self, ProbeStatus::Unsupported | ProbeStatus::NotApplicable)
    }
}

impl fmt::Display for ProbeStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// An immutable fact produced by one probe execution.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Observation {
    /// Globally unique id; the ingestion idempotency key.
    pub id: ObservationId,
    /// Which probe produced this.
    pub probe_id: ProbeId,
    /// The entity the probe measured.
    pub target_entity: EntityId,
    /// The entity that ran the probe, when it was a remote observation.
    ///
    /// `None` means the target observed itself. A remote observation carries
    /// the observer so that quorum logic can tell independent viewpoints apart
    /// (SPEC.md §50).
    pub observer_entity: Option<EntityId>,
    /// Agent session that produced this, if any.
    pub agent_session: Option<Uuid>,
    /// When the probe started (UTC).
    pub started_at: Timestamp,
    /// When the probe finished (UTC).
    pub finished_at: Timestamp,
    /// Measured duration, from a monotonic clock.
    pub duration_ms: u64,
    /// Outcome.
    pub status: ProbeStatus,
    /// Probe-specific structured facts.
    pub payload: serde_json::Value,
    /// Raw supporting material (command output excerpts, etc).
    pub evidence: serde_json::Value,
    /// Machine-readable error discriminator.
    pub error_code: Option<String>,
    /// Human-readable error detail.
    pub error_message: Option<String>,
}

impl Observation {
    /// Build an observation for a locally executed probe.
    pub fn new(probe_id: ProbeId, target_entity: EntityId, status: ProbeStatus) -> Self {
        let ts = now();
        Self {
            id: ObservationId::new(),
            probe_id,
            target_entity,
            observer_entity: None,
            agent_session: None,
            started_at: ts,
            finished_at: ts,
            duration_ms: 0,
            status,
            payload: serde_json::Value::Null,
            evidence: serde_json::Value::Null,
            error_code: None,
            error_message: None,
        }
    }

    /// Builder: record the remote observer that produced this.
    pub fn with_observer(mut self, observer: EntityId) -> Self {
        self.observer_entity = Some(observer);
        self
    }

    /// Builder: attach structured facts.
    pub fn with_payload(mut self, payload: serde_json::Value) -> Self {
        self.payload = payload;
        self
    }

    /// Builder: attach evidence.
    pub fn with_evidence(mut self, evidence: serde_json::Value) -> Self {
        self.evidence = evidence;
        self
    }

    /// Builder: attach an error.
    pub fn with_error(mut self, code: impl Into<String>, message: impl Into<String>) -> Self {
        self.error_code = Some(code.into());
        self.error_message = Some(message.into());
        self
    }

    /// Builder: set the measured duration.
    pub fn with_duration_ms(mut self, duration_ms: u64) -> Self {
        self.duration_ms = duration_ms;
        self
    }

    /// Builder: set the observation timestamps explicitly.
    pub fn with_times(mut self, started_at: Timestamp, finished_at: Timestamp) -> Self {
        self.started_at = started_at;
        self.finished_at = finished_at;
        self
    }

    /// Builder: set the agent session.
    pub fn with_agent_session(mut self, session: Uuid) -> Self {
        self.agent_session = Some(session);
        self
    }

    /// Whether this observation came from a remote observer.
    pub fn is_remote(&self) -> bool {
        self.observer_entity.is_some()
    }

    /// Read a field out of the structured payload.
    pub fn payload_field(&self, key: &str) -> Option<&serde_json::Value> {
        self.payload.get(key)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::entity::{EntityKey, EntityType};

    fn target() -> EntityId {
        EntityKey::new("env", EntityType::Host, "node01").entity_id()
    }

    #[test]
    fn probe_status_string_form_roundtrips() {
        for status in [
            ProbeStatus::Ok,
            ProbeStatus::Degraded,
            ProbeStatus::Failed,
            ProbeStatus::Timeout,
            ProbeStatus::Stuck,
            ProbeStatus::Unsupported,
            ProbeStatus::NotApplicable,
        ] {
            assert_eq!(ProbeStatus::parse(status.as_str()), Some(status));
        }
    }

    #[test]
    fn inapplicable_probes_are_not_failures() {
        assert!(!ProbeStatus::NotApplicable.is_bad());
        assert!(!ProbeStatus::Unsupported.is_bad());
        assert!(!ProbeStatus::NotApplicable.is_conclusive());
        assert!(!ProbeStatus::Unsupported.is_conclusive());
        assert!(ProbeStatus::Ok.is_conclusive());
    }

    #[test]
    fn stuck_and_timeout_count_as_bad() {
        assert!(ProbeStatus::Failed.is_bad());
        assert!(ProbeStatus::Timeout.is_bad());
        assert!(ProbeStatus::Stuck.is_bad());
        assert!(!ProbeStatus::Degraded.is_bad());
        assert!(!ProbeStatus::Ok.is_bad());
    }

    #[test]
    fn observation_ids_are_unique() {
        let a = Observation::new(ProbeId::new("test"), target(), ProbeStatus::Ok);
        let b = Observation::new(ProbeId::new("test"), target(), ProbeStatus::Ok);
        assert_ne!(a.id, b.id);
    }

    #[test]
    fn remote_observations_carry_their_observer() {
        let observer = EntityKey::new("env", EntityType::Host, "peer01").entity_id();
        let local = Observation::new(ProbeId::new("test"), target(), ProbeStatus::Ok);
        assert!(!local.is_remote());
        assert!(local.with_observer(observer).is_remote());
    }

    #[test]
    fn json_roundtrip_preserves_every_field() {
        let observation = Observation::new(ProbeId::new("network.tcp"), target(), ProbeStatus::Failed)
            .with_observer(EntityKey::new("env", EntityType::Host, "peer01").entity_id())
            .with_payload(serde_json::json!({"port": 22}))
            .with_error("connect_refused", "connection refused")
            .with_duration_ms(42);
        let text = serde_json::to_string(&observation).expect("serialize");
        let decoded: Observation = serde_json::from_str(&text).expect("deserialize");
        assert_eq!(decoded, observation);
    }
}
