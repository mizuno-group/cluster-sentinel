//! Capability resolution (IMPLEMENTATION.md §40).
//!
//! Precedence, highest first:
//!
//! ```text
//! explicit force-disable
//!   > explicit force-enable
//!   > runtime discovery
//!   > role-derived hint
//! ```
//!
//! A role hint alone must never start a probe: it only applies when runtime
//! discovery had nothing to say about that capability at all.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

use super::{Capability, CapabilitySet};

/// An operator's explicit decision about one capability (SPEC.md §19).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CapabilityOverride {
    /// Enable regardless of what discovery found.
    Force,
    /// Enable if discovery had no opinion.
    Enable,
    /// Never enable, whatever discovery found.
    Disable,
}

/// What runtime discovery concluded about one capability.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiscoveryOutcome {
    /// The agent looked and found the capability present.
    Detected,
    /// The agent looked and found it absent.
    NotDetected,
}

/// Why a capability ended up enabled or disabled. Kept so `sentinel entity
/// show` can explain the decision instead of presenting a bare list.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ResolutionReason {
    /// Operator forced it off.
    ForcedDisabled,
    /// Operator forced it on.
    ForcedEnabled,
    /// Runtime discovery detected it.
    Discovered,
    /// Runtime discovery explicitly did not detect it.
    NotDiscovered,
    /// Operator asked to enable it and discovery had no opinion.
    OperatorEnabled,
    /// A role suggested it and nothing else had an opinion.
    RoleHint,
}

impl ResolutionReason {
    /// Whether this reason results in an enabled capability.
    pub fn is_enabled(&self) -> bool {
        matches!(
            self,
            ResolutionReason::ForcedEnabled
                | ResolutionReason::Discovered
                | ResolutionReason::OperatorEnabled
                | ResolutionReason::RoleHint
        )
    }
}

/// The outcome of resolving one entity's capabilities.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Resolution {
    /// The capabilities that are actually enabled.
    pub enabled: CapabilitySet,
    /// Why each considered capability was enabled or disabled.
    pub reasons: BTreeMap<Capability, ResolutionReason>,
}

impl Resolution {
    /// Why this capability was enabled or disabled, if it was considered.
    pub fn reason(&self, name: &str) -> Option<ResolutionReason> {
        self.reasons.get(&Capability::new(name)).copied()
    }
}

/// Resolve the effective capability set for one entity.
///
/// `discovered` is what the agent actually observed, `overrides` the operator's
/// configuration and `role_hints` the capabilities a role merely suggests.
pub fn resolve_capabilities(
    discovered: &BTreeMap<Capability, DiscoveryOutcome>,
    overrides: &BTreeMap<Capability, CapabilityOverride>,
    role_hints: &BTreeSet<Capability>,
) -> Resolution {
    let mut considered: BTreeSet<Capability> = BTreeSet::new();
    considered.extend(discovered.keys().cloned());
    considered.extend(overrides.keys().cloned());
    considered.extend(role_hints.iter().cloned());

    let mut resolution = Resolution::default();
    for capability in considered {
        let reason = match overrides.get(&capability) {
            Some(CapabilityOverride::Disable) => ResolutionReason::ForcedDisabled,
            Some(CapabilityOverride::Force) => ResolutionReason::ForcedEnabled,
            _ => match discovered.get(&capability) {
                Some(DiscoveryOutcome::Detected) => ResolutionReason::Discovered,
                Some(DiscoveryOutcome::NotDetected) => ResolutionReason::NotDiscovered,
                None => match overrides.get(&capability) {
                    Some(CapabilityOverride::Enable) => ResolutionReason::OperatorEnabled,
                    _ if role_hints.contains(&capability) => ResolutionReason::RoleHint,
                    _ => ResolutionReason::NotDiscovered,
                },
            },
        };
        if reason.is_enabled() {
            resolution.enabled.insert(capability.clone());
        }
        resolution.reasons.insert(capability, reason);
    }
    resolution
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cap(name: &str) -> Capability {
        Capability::new(name)
    }

    #[test]
    fn force_disable_beats_everything_else() {
        let discovered = BTreeMap::from([(cap("storage.nfs.server"), DiscoveryOutcome::Detected)]);
        let overrides = BTreeMap::from([(cap("storage.nfs.server"), CapabilityOverride::Disable)]);
        let hints = BTreeSet::from([cap("storage.nfs.server")]);

        let resolved = resolve_capabilities(&discovered, &overrides, &hints);
        assert!(!resolved.enabled.has("storage.nfs.server"));
        assert_eq!(
            resolved.reason("storage.nfs.server"),
            Some(ResolutionReason::ForcedDisabled)
        );
    }

    #[test]
    fn force_enable_beats_negative_discovery() {
        let discovered = BTreeMap::from([(cap("storage.nfs.server"), DiscoveryOutcome::NotDetected)]);
        let overrides = BTreeMap::from([(cap("storage.nfs.server"), CapabilityOverride::Force)]);

        let resolved = resolve_capabilities(&discovered, &overrides, &BTreeSet::new());
        assert!(resolved.enabled.has("storage.nfs.server"));
        assert_eq!(
            resolved.reason("storage.nfs.server"),
            Some(ResolutionReason::ForcedEnabled)
        );
    }

    #[test]
    fn discovery_beats_a_role_hint_in_both_directions() {
        let hints = BTreeSet::from([cap("storage.nfs.server")]);

        let negative = BTreeMap::from([(cap("storage.nfs.server"), DiscoveryOutcome::NotDetected)]);
        let resolved = resolve_capabilities(&negative, &BTreeMap::new(), &hints);
        assert!(
            !resolved.enabled.has("storage.nfs.server"),
            "a role must never start a probe discovery has ruled out"
        );
        assert_eq!(
            resolved.reason("storage.nfs.server"),
            Some(ResolutionReason::NotDiscovered)
        );

        let positive = BTreeMap::from([(cap("gpu.nvidia"), DiscoveryOutcome::Detected)]);
        let resolved = resolve_capabilities(&positive, &BTreeMap::new(), &BTreeSet::new());
        assert!(resolved.enabled.has("gpu.nvidia"));
        assert_eq!(resolved.reason("gpu.nvidia"), Some(ResolutionReason::Discovered));
    }

    #[test]
    fn role_hint_applies_only_when_nothing_else_has_an_opinion() {
        let hints = BTreeSet::from([cap("observer.peer")]);
        let resolved = resolve_capabilities(&BTreeMap::new(), &BTreeMap::new(), &hints);
        assert!(resolved.enabled.has("observer.peer"));
        assert_eq!(resolved.reason("observer.peer"), Some(ResolutionReason::RoleHint));
    }

    #[test]
    fn operator_enable_applies_when_discovery_is_silent() {
        let overrides = BTreeMap::from([(cap("storage.zfs"), CapabilityOverride::Enable)]);
        let resolved = resolve_capabilities(&BTreeMap::new(), &overrides, &BTreeSet::new());
        assert!(resolved.enabled.has("storage.zfs"));
        assert_eq!(resolved.reason("storage.zfs"), Some(ResolutionReason::OperatorEnabled));

        // ...but discovery still wins over it.
        let discovered = BTreeMap::from([(cap("storage.zfs"), DiscoveryOutcome::NotDetected)]);
        let resolved = resolve_capabilities(&discovered, &overrides, &BTreeSet::new());
        assert!(!resolved.enabled.has("storage.zfs"));
    }

    #[test]
    fn resolution_is_empty_when_there_is_nothing_to_consider() {
        let resolved = resolve_capabilities(&BTreeMap::new(), &BTreeMap::new(), &BTreeSet::new());
        assert!(resolved.enabled.is_empty());
        assert!(resolved.reasons.is_empty());
    }
}
