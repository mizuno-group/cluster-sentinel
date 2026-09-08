//! Agent sessions.
//!
//! A *session* exists so that a restart, a reconnect and a reboot are
//! distinguishable. A new boot id under the same agent identity means the
//! machine rebooted (SPEC.md §107); a new session with the same boot id means
//! only that the agent process restarted, which is a much smaller event.

use std::collections::HashMap;

use uuid::Uuid;

use crate::entity::EntityId;
use crate::protocol::{HeartbeatRequest, RegisterRequest};
use crate::time::Timestamp;

/// Namespace for deriving a stable agent id from its host.
const AGENT_NAMESPACE: Uuid = Uuid::from_u128(0x2b91c4a7_88f3_5d61_9c04_7ae35bd12f88);

/// One agent's current session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentSession {
    /// Stable across restarts, derived from environment and host name.
    pub agent_id: Uuid,
    /// New on every registration.
    pub session_id: Uuid,
    /// The host entity this agent reports for.
    pub entity_id: EntityId,
    /// The agent's binary version.
    pub agent_version: String,
    /// Boot id at registration.
    pub boot_id: Option<String>,
    /// When this session began.
    pub started_at: Timestamp,
    /// Last time anything was heard from this agent.
    pub last_seen_at: Timestamp,
}

/// What the controller concluded from a heartbeat.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct HeartbeatOutcome {
    /// Whether the agent must register again.
    pub reregister: bool,
    /// The current peer assignment revision.
    pub assignment_revision: u64,
}

/// Whether a registration represents a reboot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RegistrationKind {
    /// This agent has not been seen before.
    New,
    /// The agent process restarted; the machine did not.
    AgentRestart,
    /// The boot id changed: the machine rebooted.
    HostRebooted,
}

/// The set of agents the controller knows about.
#[derive(Debug, Default)]
pub struct AgentRegistry {
    sessions: HashMap<Uuid, AgentSession>,
    assignment_revision: u64,
}

impl AgentRegistry {
    /// An empty registry.
    pub fn new() -> Self {
        Self::default()
    }

    /// Derive an agent's stable identity from the host it runs on.
    ///
    /// Stable so that a restarted agent is recognised as the same agent rather
    /// than accumulating identities.
    pub fn agent_id_for(environment: &str, hostname: &str) -> Uuid {
        Uuid::new_v5(&AGENT_NAMESPACE, format!("{environment}\u{1f}{hostname}").as_bytes())
    }

    /// Record a registration, returning the new session.
    pub fn register(&mut self, request: &RegisterRequest, entity_id: EntityId) -> AgentSession {
        let agent_id = Self::agent_id_for(&request.environment, &request.hostname);
        let now = crate::time::now();

        let session = AgentSession {
            agent_id,
            session_id: Uuid::new_v4(),
            entity_id,
            agent_version: request.agent_version.clone(),
            boot_id: request.boot_id.clone(),
            started_at: now,
            last_seen_at: now,
        };
        self.sessions.insert(agent_id, session.clone());
        session
    }

    /// Classify a registration against what is already known.
    pub fn classify(&self, request: &RegisterRequest) -> RegistrationKind {
        let agent_id = Self::agent_id_for(&request.environment, &request.hostname);
        match self.sessions.get(&agent_id) {
            None => RegistrationKind::New,
            Some(existing) => {
                // Only a *changed* boot id is evidence of a reboot. An agent
                // that cannot read its boot id must not be assumed to have
                // rebooted every time it restarts.
                match (&existing.boot_id, &request.boot_id) {
                    (Some(before), Some(after)) if before != after => RegistrationKind::HostRebooted,
                    _ => RegistrationKind::AgentRestart,
                }
            }
        }
    }

    /// Record a heartbeat.
    pub fn heartbeat(&mut self, request: &HeartbeatRequest, at: Timestamp) -> HeartbeatOutcome {
        let assignment_revision = self.assignment_revision;
        match self.sessions.get_mut(&request.agent_id) {
            Some(session) if session.session_id == request.session_id => {
                session.last_seen_at = at;
                HeartbeatOutcome {
                    reregister: false,
                    assignment_revision,
                }
            }
            // Either the controller has never heard of this agent, or the
            // session is from before a controller restart. Ask it to register
            // rather than silently accepting a session we cannot vouch for.
            _ => HeartbeatOutcome {
                reregister: true,
                assignment_revision,
            },
        }
    }

    /// Note that an agent was heard from.
    pub fn touch(&mut self, agent_id: Uuid, at: Timestamp) {
        if let Some(session) = self.sessions.get_mut(&agent_id) {
            session.last_seen_at = at;
        }
    }

    /// Look a session up.
    pub fn get(&self, agent_id: Uuid) -> Option<&AgentSession> {
        self.sessions.get(&agent_id)
    }

    /// Every known session.
    pub fn sessions(&self) -> impl Iterator<Item = &AgentSession> {
        self.sessions.values()
    }

    /// Agents not heard from since `cutoff`.
    pub fn stale_since(&self, cutoff: Timestamp) -> Vec<&AgentSession> {
        self.sessions.values().filter(|s| s.last_seen_at < cutoff).collect()
    }

    /// How many agents are registered.
    pub fn len(&self) -> usize {
        self.sessions.len()
    }

    /// Whether any agent is registered.
    pub fn is_empty(&self) -> bool {
        self.sessions.is_empty()
    }

    /// Bump the peer assignment revision.
    pub fn bump_assignment_revision(&mut self) -> u64 {
        self.assignment_revision += 1;
        self.assignment_revision
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::capability::CapabilitySet;
    use crate::entity::{EntityKey, EntityType};
    use crate::time::now;

    fn entity(name: &str) -> EntityId {
        EntityKey::new("lab", EntityType::Host, name).entity_id()
    }

    fn registration(hostname: &str, boot_id: Option<&str>) -> RegisterRequest {
        let mut request = RegisterRequest::new("lab", hostname, CapabilitySet::new());
        request.boot_id = boot_id.map(str::to_string);
        request
    }

    fn heartbeat_for(session: &AgentSession) -> HeartbeatRequest {
        HeartbeatRequest {
            protocol_version: crate::PROTOCOL_VERSION,
            agent_id: session.agent_id,
            session_id: session.session_id,
            boot_id: session.boot_id.clone(),
            agent_time: now(),
            spooled_observations: 0,
        }
    }

    #[test]
    fn an_agent_id_is_stable_across_restarts_but_scoped_to_its_host() {
        let a = AgentRegistry::agent_id_for("lab", "node-a");
        assert_eq!(a, AgentRegistry::agent_id_for("lab", "node-a"));
        assert_ne!(a, AgentRegistry::agent_id_for("lab", "node-b"));
        assert_ne!(a, AgentRegistry::agent_id_for("other", "node-a"));
    }

    #[test]
    fn re_registering_keeps_the_agent_id_and_issues_a_new_session() {
        let mut registry = AgentRegistry::new();
        let first = registry.register(&registration("node-a", Some("boot-1")), entity("node-a"));
        let second = registry.register(&registration("node-a", Some("boot-1")), entity("node-a"));

        assert_eq!(first.agent_id, second.agent_id);
        assert_ne!(first.session_id, second.session_id);
        assert_eq!(registry.len(), 1, "one agent, not two");
    }

    #[test]
    fn a_changed_boot_id_is_a_reboot_and_an_unchanged_one_is_not() {
        let mut registry = AgentRegistry::new();
        assert_eq!(
            registry.classify(&registration("node-a", Some("boot-1"))),
            RegistrationKind::New
        );

        registry.register(&registration("node-a", Some("boot-1")), entity("node-a"));
        assert_eq!(
            registry.classify(&registration("node-a", Some("boot-1"))),
            RegistrationKind::AgentRestart,
            "the agent restarted; the machine did not"
        );
        assert_eq!(
            registry.classify(&registration("node-a", Some("boot-2"))),
            RegistrationKind::HostRebooted
        );
    }

    #[test]
    fn a_missing_boot_id_is_never_read_as_a_reboot() {
        // An agent that cannot read /proc must not look like it reboots every
        // time it restarts.
        let mut registry = AgentRegistry::new();
        registry.register(&registration("node-a", None), entity("node-a"));
        assert_eq!(
            registry.classify(&registration("node-a", None)),
            RegistrationKind::AgentRestart
        );

        registry.register(&registration("node-b", Some("boot-1")), entity("node-b"));
        assert_eq!(
            registry.classify(&registration("node-b", None)),
            RegistrationKind::AgentRestart
        );
    }

    #[test]
    fn a_heartbeat_for_a_current_session_is_accepted() {
        let mut registry = AgentRegistry::new();
        let session = registry.register(&registration("node-a", Some("boot-1")), entity("node-a"));
        let outcome = registry.heartbeat(&heartbeat_for(&session), now());
        assert!(!outcome.reregister);
    }

    #[test]
    fn a_heartbeat_for_a_superseded_session_asks_for_re_registration() {
        let mut registry = AgentRegistry::new();
        let old = registry.register(&registration("node-a", Some("boot-1")), entity("node-a"));
        registry.register(&registration("node-a", Some("boot-1")), entity("node-a"));

        assert!(registry.heartbeat(&heartbeat_for(&old), now()).reregister);
    }

    #[test]
    fn a_heartbeat_from_an_unknown_agent_asks_for_re_registration() {
        let mut registry = AgentRegistry::new();
        let unknown = HeartbeatRequest {
            protocol_version: crate::PROTOCOL_VERSION,
            agent_id: Uuid::new_v4(),
            session_id: Uuid::new_v4(),
            boot_id: None,
            agent_time: now(),
            spooled_observations: 0,
        };
        assert!(registry.heartbeat(&unknown, now()).reregister);
    }

    #[test]
    fn heartbeats_and_batches_both_refresh_the_last_seen_time() {
        let mut registry = AgentRegistry::new();
        let session = registry.register(&registration("node-a", Some("boot-1")), entity("node-a"));
        let later = now() + chrono::Duration::seconds(60);

        registry.heartbeat(&heartbeat_for(&session), later);
        assert_eq!(registry.get(session.agent_id).unwrap().last_seen_at, later);

        let later_still = later + chrono::Duration::seconds(60);
        registry.touch(session.agent_id, later_still);
        assert_eq!(registry.get(session.agent_id).unwrap().last_seen_at, later_still);
    }

    #[test]
    fn silent_agents_can_be_listed_so_the_controller_can_notice_them() {
        let mut registry = AgentRegistry::new();
        let session = registry.register(&registration("node-a", Some("boot-1")), entity("node-a"));
        registry.register(&registration("node-b", Some("boot-1")), entity("node-b"));
        registry.touch(session.agent_id, now() - chrono::Duration::hours(1));

        let stale = registry.stale_since(now() - chrono::Duration::minutes(5));
        assert_eq!(stale.len(), 1);
        assert_eq!(stale[0].agent_id, session.agent_id);
    }

    #[test]
    fn the_assignment_revision_advances_monotonically() {
        let mut registry = AgentRegistry::new();
        assert_eq!(registry.bump_assignment_revision(), 1);
        assert_eq!(registry.bump_assignment_revision(), 2);
    }
}
