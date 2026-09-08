//! Maintenance windows (IMPLEMENTATION.md §76).
//!
//! Maintenance suppresses **notification only**. Observation continues, state
//! continues, diagnosis continues, and an abnormal state is never rewritten to
//! look healthy.
//!
//! That distinction is the whole design. A maintenance window that made things
//! *appear* fine would hide a genuine fault that started during the window and
//! leave nobody able to reconstruct when it began.

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::entity::EntityId;
use crate::time::{now, Timestamp};

/// A period during which notifications about something are suppressed.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MaintenanceWindow {
    /// Window identifier.
    pub id: Uuid,
    /// The entity under maintenance; `None` means the whole environment.
    pub entity: Option<EntityId>,
    /// Why, for the record.
    pub reason: String,
    /// When it starts.
    pub starts_at: Timestamp,
    /// When it ends; `None` means open-ended.
    pub ends_at: Option<Timestamp>,
    /// Who declared it.
    pub created_by: Option<String>,
}

impl MaintenanceWindow {
    /// A window for one entity.
    pub fn for_entity(entity: EntityId, reason: impl Into<String>) -> Self {
        Self {
            id: Uuid::new_v4(),
            entity: Some(entity),
            reason: reason.into(),
            starts_at: now(),
            ends_at: None,
            created_by: None,
        }
    }

    /// A window covering the whole environment.
    pub fn for_environment(reason: impl Into<String>) -> Self {
        Self {
            entity: None,
            ..Self::for_entity(EntityId::from_uuid(Uuid::nil()), reason)
        }
    }

    /// Builder: set the end time.
    pub fn until(mut self, ends_at: Timestamp) -> Self {
        self.ends_at = Some(ends_at);
        self
    }

    /// Builder: set the start time.
    pub fn from(mut self, starts_at: Timestamp) -> Self {
        self.starts_at = starts_at;
        self
    }

    /// Builder: record who declared it.
    pub fn by(mut self, actor: impl Into<String>) -> Self {
        self.created_by = Some(actor.into());
        self
    }

    /// Whether the window is in force at a given moment.
    pub fn is_active_at(&self, at: Timestamp) -> bool {
        at >= self.starts_at && self.ends_at.is_none_or(|ends_at| at < ends_at)
    }

    /// Whether the window covers an entity.
    pub fn covers(&self, entity: EntityId) -> bool {
        match self.entity {
            None => true,
            Some(covered) => covered == entity,
        }
    }
}

/// The maintenance windows in force.
#[derive(Debug, Clone, Default)]
pub struct MaintenanceWindows {
    windows: Vec<MaintenanceWindow>,
}

impl MaintenanceWindows {
    /// No windows.
    pub fn new() -> Self {
        Self::default()
    }

    /// Build from a set of windows.
    pub fn from_windows(windows: impl IntoIterator<Item = MaintenanceWindow>) -> Self {
        Self {
            windows: windows.into_iter().collect(),
        }
    }

    /// Add a window.
    pub fn add(&mut self, window: MaintenanceWindow) {
        self.windows.push(window);
    }

    /// Whether notifications about an entity are currently suppressed.
    pub fn suppresses(&self, entity: EntityId) -> bool {
        let at = now();
        self.windows.iter().any(|w| w.is_active_at(at) && w.covers(entity))
    }

    /// Whether notifications about any of these entities are suppressed.
    ///
    /// An incident is suppressed only when **every** entity it affects is under
    /// maintenance. One machine being worked on must not silence an incident
    /// that also involves four that are not.
    pub fn suppresses_all(&self, entities: &[EntityId]) -> bool {
        !entities.is_empty() && entities.iter().all(|entity| self.suppresses(*entity))
    }

    /// The windows currently in force.
    pub fn active(&self) -> Vec<&MaintenanceWindow> {
        let at = now();
        self.windows.iter().filter(|w| w.is_active_at(at)).collect()
    }

    /// How many windows are held.
    pub fn len(&self) -> usize {
        self.windows.len()
    }

    /// Whether none are held.
    pub fn is_empty(&self) -> bool {
        self.windows.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::entity::{EntityKey, EntityType};

    fn host(name: &str) -> EntityId {
        EntityKey::new("lab", EntityType::Host, name).entity_id()
    }

    #[test]
    fn a_window_suppresses_the_entity_it_names() {
        let windows = MaintenanceWindows::from_windows([MaintenanceWindow::for_entity(host("a"), "disk swap")]);
        assert!(windows.suppresses(host("a")));
        assert!(!windows.suppresses(host("b")));
    }

    #[test]
    fn an_environment_window_suppresses_everything() {
        let windows = MaintenanceWindows::from_windows([MaintenanceWindow::for_environment("power work")]);
        assert!(windows.suppresses(host("a")));
        assert!(windows.suppresses(host("b")));
    }

    #[test]
    fn a_window_that_has_not_started_suppresses_nothing() {
        let future = MaintenanceWindow::for_entity(host("a"), "planned").from(now() + chrono::Duration::hours(1));
        assert!(!MaintenanceWindows::from_windows([future]).suppresses(host("a")));
    }

    #[test]
    fn an_expired_window_suppresses_nothing() {
        // A window nobody closed must not silence a host forever.
        let past = MaintenanceWindow::for_entity(host("a"), "finished")
            .from(now() - chrono::Duration::hours(2))
            .until(now() - chrono::Duration::hours(1));
        assert!(!MaintenanceWindows::from_windows([past]).suppresses(host("a")));
    }

    #[test]
    fn an_open_ended_window_stays_in_force() {
        let open = MaintenanceWindow::for_entity(host("a"), "indefinite");
        assert!(MaintenanceWindows::from_windows([open]).suppresses(host("a")));
    }

    #[test]
    fn an_incident_is_suppressed_only_when_every_affected_entity_is_covered() {
        // One machine being worked on must not silence an incident that also
        // involves four that are not.
        let windows = MaintenanceWindows::from_windows([MaintenanceWindow::for_entity(host("a"), "work")]);

        assert!(windows.suppresses_all(&[host("a")]));
        assert!(!windows.suppresses_all(&[host("a"), host("b")]));
        assert!(!windows.suppresses_all(&[host("b")]));
    }

    #[test]
    fn an_incident_affecting_nothing_is_not_suppressed() {
        let windows = MaintenanceWindows::from_windows([MaintenanceWindow::for_environment("work")]);
        assert!(
            !windows.suppresses_all(&[]),
            "an empty set must not be silently covered"
        );
    }

    #[test]
    fn with_no_windows_nothing_is_suppressed() {
        let windows = MaintenanceWindows::new();
        assert!(windows.is_empty());
        assert!(!windows.suppresses(host("a")));
        assert!(!windows.suppresses_all(&[host("a")]));
    }

    #[test]
    fn only_active_windows_are_listed() {
        let windows = MaintenanceWindows::from_windows([
            MaintenanceWindow::for_entity(host("a"), "now"),
            MaintenanceWindow::for_entity(host("b"), "later").from(now() + chrono::Duration::hours(1)),
        ]);
        assert_eq!(windows.len(), 2);
        assert_eq!(windows.active().len(), 1);
    }

    #[test]
    fn maintenance_touches_notification_only() {
        // IMPLEMENTATION.md §76, asserted against the source so it stays true.
        // A window that rewrote health would hide a genuine fault that started
        // during it, and leave nobody able to reconstruct when it began.
        let implementation = include_str!("maintenance.rs")
            .split("#[cfg(test)]")
            .next()
            .expect("implementation");
        // Code identifiers only: the module documentation necessarily uses the
        // word "observation" to explain that observation continues.
        for forbidden in [
            "EntityState",
            "Health::",
            "set_component",
            "ProbeStatus",
            "Observation::",
        ] {
            assert!(
                !implementation.contains(forbidden),
                "maintenance must not touch {forbidden}: it suppresses notification, nothing else"
            );
        }
    }
}
