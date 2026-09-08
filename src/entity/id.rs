//! Entity identity (IMPLEMENTATION.md §38).
//!
//! Internally an entity is a UUID. That UUID is *derived deterministically*
//! from the natural key `(environment, entity_type, canonical_name)` with
//! UUIDv5, so that two inventory providers discovering the same thing
//! independently converge on the same id without a central allocator.
//!
//! An IP address is never part of identity: one host legitimately owns several
//! addresses across management, storage and interconnect networks (SPEC.md §37).

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use super::EntityType;

/// Namespace for Sentinel entity UUIDv5 derivation. Fixed for all time: it is
/// part of the on-disk identity contract.
const ENTITY_NAMESPACE: Uuid = Uuid::from_u128(0x7d1a3f52_5c9a_5e17_9f24_3f2c1b8e6a40);

/// Stable internal identifier of a [`super::ManagedEntity`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct EntityId(Uuid);

impl EntityId {
    /// Wrap an existing UUID (used when loading from the database).
    pub fn from_uuid(uuid: Uuid) -> Self {
        Self(uuid)
    }

    /// The underlying UUID.
    pub fn as_uuid(&self) -> Uuid {
        self.0
    }
}

impl fmt::Display for EntityId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl FromStr for EntityId {
    type Err = uuid::Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Ok(Self(Uuid::parse_str(s)?))
    }
}

/// The natural key of an entity, used to merge discoveries.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct EntityKey {
    /// Environment the entity lives in.
    pub environment: String,
    /// Entity type.
    pub entity_type: EntityType,
    /// Canonical name, unique within `(environment, entity_type)`.
    pub canonical_name: String,
}

impl EntityKey {
    /// Build a natural key.
    pub fn new(environment: &str, entity_type: EntityType, canonical_name: &str) -> Self {
        Self {
            environment: environment.to_string(),
            entity_type,
            canonical_name: canonical_name.to_string(),
        }
    }

    /// Derive the deterministic entity id for this key.
    pub fn entity_id(&self) -> EntityId {
        let name = format!(
            "{}\u{1f}{}\u{1f}{}",
            self.environment,
            self.entity_type.as_str(),
            self.canonical_name
        );
        EntityId(Uuid::new_v5(&ENTITY_NAMESPACE, name.as_bytes()))
    }
}

impl fmt::Display for EntityKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}/{}/{}", self.environment, self.entity_type, self.canonical_name)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn derivation_is_deterministic_across_processes() {
        // The *derivation* is what must never change: an id is computed from
        // the key, so altering how would silently orphan every existing row.
        // The environment name here is only an example, and this golden value
        // moves with it -- what it pins is the algorithm, not this name.
        let key = EntityKey::new("example-lab", EntityType::Host, "node01");
        assert_eq!(key.entity_id().to_string(), "13c270fd-2a94-5b08-9e4f-34e1aa8ecdfa");
    }

    #[test]
    fn separator_prevents_key_field_collisions() {
        let a = EntityKey::new("a", EntityType::Host, "b-c");
        let b = EntityKey::new("a-b", EntityType::Host, "c");
        assert_ne!(a.entity_id(), b.entity_id());
    }

    #[test]
    fn entity_id_string_roundtrips() {
        let id = EntityKey::new("env", EntityType::Storage, "store").entity_id();
        assert_eq!(EntityId::from_str(&id.to_string()).unwrap(), id);
    }
}
