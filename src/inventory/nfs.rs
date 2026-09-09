//! Storage topology derived from the mounts the agents already report.
//!
//! The dependency graph is what turns "nine nodes have a storage problem" into
//! "one fileserver is down". Declaring it by hand works and is exact, but it is
//! a second copy of a fact the cluster already knows: every agent reports its
//! own NFS mounts on every cycle, and each of those mounts *is* an edge. A
//! second copy has to be maintained, and the failure mode when it is not is
//! silent -- diagnosis quietly degrades to one incident per client, and nobody
//! finds out until an outage.
//!
//! So the edges are derived from evidence. Declaring them stays possible and
//! wins where it disagrees, because merging is additive and the static
//! provider is authoritative about what it declares.
//!
//! Two rules keep this honest:
//!
//! * **A server is resolved, never invented from an address.** A mount written
//!   `10.0.0.4:/data` names a machine this code cannot identify: an address is
//!   not an identity (ADR 0001), and minting an entity called `10.0.0.4` would
//!   create a second, permanent identity for a host that already exists under
//!   its name. Such a mount is reported as unresolved instead, which is a
//!   thing an operator can act on.
//! * **A name is enough to create a host.** `filesrv01:/data` is evidence that
//!   a machine called `filesrv01` exists and serves storage, which is exactly
//!   what the static configuration was being used to say.

use std::collections::{BTreeMap, BTreeSet};

use crate::capability::well_known;
use crate::dependency::{DependencyEdge, DependencyType};
use crate::entity::{DiscoverySource, EntityId, EntityKey, EntityType, ManagedEntity};
use crate::observation::Observation;
use crate::probes::nfs::PROBE_CLIENT_MOUNT;

use super::{Inventory, InventorySnapshot};

/// The name this integration is recorded under.
pub const SOURCE: &str = "nfs";

/// A mount whose server could not be tied to a known host.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnresolvedServer {
    /// The server as the mount table spells it.
    pub server: String,
    /// The hosts that mount from it.
    pub clients: Vec<String>,
}

/// What one pass over the mount reports produced.
#[derive(Debug, Clone, Default)]
pub struct StorageTopology {
    /// Entities and edges to merge.
    pub snapshot: InventorySnapshot,
    /// Servers that could not be resolved, for the operator to declare.
    pub unresolved: Vec<UnresolvedServer>,
}

impl StorageTopology {
    /// How many `uses_storage` edges were derived.
    pub fn edges(&self) -> usize {
        self.snapshot
            .dependencies
            .iter()
            .filter(|edge| edge.dependency_type == DependencyType::UsesStorage)
            .count()
    }
}

/// The name a derived storage entity takes, given the host that serves it.
///
/// One storage entity per server rather than per export: the question the
/// graph is asked is "did this fileserver take several nodes down with it",
/// and per-export granularity would split the answer across exports without
/// making it more accurate.
pub fn storage_name(server: &str) -> String {
    server.to_string()
}

/// Derive the storage topology from the latest mount observations.
pub fn topology_from_mounts<'a>(
    environment: &str,
    inventory: &Inventory,
    observations: impl IntoIterator<Item = &'a Observation>,
) -> StorageTopology {
    let mut snapshot = InventorySnapshot::new(DiscoverySource::Integration(SOURCE.to_string()));
    let mut servers: BTreeMap<String, BTreeSet<EntityId>> = BTreeMap::new();
    let mut unresolved: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();

    for observation in observations {
        if observation.probe_id.as_str() != PROBE_CLIENT_MOUNT {
            continue;
        }
        for server in servers_in(observation) {
            match resolve(environment, inventory, &server) {
                Some(host) => {
                    servers.entry(host).or_default().insert(observation.target_entity);
                }
                None => {
                    let client = inventory
                        .get(observation.target_entity)
                        .map(|e| e.canonical_name.clone())
                        .unwrap_or_else(|| observation.target_entity.to_string());
                    unresolved.entry(server).or_default().insert(client);
                }
            }
        }
    }

    for (server, clients) in servers {
        let storage = storage_name(&server);
        let server_id = EntityKey::new(environment, EntityType::Host, &server).entity_id();
        let storage_id = EntityKey::new(environment, EntityType::Storage, &storage).entity_id();

        // The server itself. Declared as a capability hint only: if it runs an
        // agent, runtime discovery outranks this and has the final say.
        snapshot.add_entity(
            ManagedEntity::new(environment, EntityType::Host, &server)
                .with_capabilities([well_known::STORAGE_NFS_SERVER].into_iter().collect()),
        );
        snapshot.add_entity(ManagedEntity::new(environment, EntityType::Storage, &storage));
        snapshot.add_dependency(DependencyEdge::new(storage_id, server_id, DependencyType::Provides));

        for client in clients {
            // A server that mounts its own export is not a client of itself,
            // and an edge saying so would make it its own suspected cause.
            if client == server_id {
                continue;
            }
            snapshot.add_dependency(DependencyEdge::new(client, storage_id, DependencyType::UsesStorage));
        }
    }

    StorageTopology {
        snapshot,
        unresolved: unresolved
            .into_iter()
            .map(|(server, clients)| UnresolvedServer {
                server,
                clients: clients.into_iter().collect(),
            })
            .collect(),
    }
}

/// The NFS servers named by one mount observation.
fn servers_in(observation: &Observation) -> Vec<String> {
    let mut found = BTreeSet::new();

    if let Some(mounts) = observation.payload.get("mounts").and_then(|v| v.as_array()) {
        for mount in mounts {
            if let Some(server) = mount.get("server").and_then(|v| v.as_str()) {
                found.insert(server.to_string());
            }
        }
    }
    // Older payloads carry only the flat list.
    if found.is_empty() {
        if let Some(servers) = observation.payload.get("servers").and_then(|v| v.as_array()) {
            for server in servers.iter().filter_map(|v| v.as_str()) {
                found.insert(server.to_string());
            }
        }
    }

    found.into_iter().collect()
}

/// Tie a mount's server string to a host's canonical name.
///
/// Returns `None` when the mount names an address that belongs to no known
/// host: naming an entity after an address would give one machine two
/// identities, which is the failure ADR 0001 exists to prevent.
fn resolve(environment: &str, inventory: &Inventory, server: &str) -> Option<String> {
    if is_address(server) {
        return inventory
            .entities()
            .find(|entity| entity.entity_type == EntityType::Host && has_address(entity, server))
            .map(|entity| entity.canonical_name.clone());
    }

    // An exact match first, then the short name, so `fs1.cluster.example` and
    // `fs1` are the same machine rather than two.
    let short = server.split('.').next().unwrap_or(server);
    for candidate in [server, short] {
        let id = EntityKey::new(environment, EntityType::Host, candidate).entity_id();
        if inventory.get(id).is_some() {
            return Some(candidate.to_string());
        }
    }
    for entity in inventory.entities() {
        if entity.entity_type == EntityType::Host && entity.canonical_name.eq_ignore_ascii_case(short) {
            return Some(entity.canonical_name.clone());
        }
    }

    // Unknown, but named: that is enough to declare it.
    Some(short.to_string())
}

/// Whether a mount's server field is an address rather than a name.
fn is_address(server: &str) -> bool {
    server.parse::<std::net::IpAddr>().is_ok()
}

/// Whether an entity registered this address.
fn has_address(entity: &ManagedEntity, address: &str) -> bool {
    let listed = |value: Option<&serde_json::Value>| {
        value
            .and_then(|v| v.as_array())
            .is_some_and(|addresses| addresses.iter().filter_map(|a| a.as_str()).any(|a| a == address))
    };

    listed(entity.metadata.get("addresses")) || listed(entity.metadata.get("host").and_then(|h| h.get("addresses")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::observation::ProbeStatus;
    use crate::probes::ProbeId;

    const ENV: &str = "lab";

    fn host_id(name: &str) -> EntityId {
        EntityKey::new(ENV, EntityType::Host, name).entity_id()
    }

    fn storage_id(name: &str) -> EntityId {
        EntityKey::new(ENV, EntityType::Storage, name).entity_id()
    }

    fn inventory_of(hosts: &[&str]) -> Inventory {
        let mut inventory = Inventory::new();
        let mut snapshot = InventorySnapshot::new(DiscoverySource::StaticConfig);
        for name in hosts {
            snapshot.add_entity(ManagedEntity::new(ENV, EntityType::Host, *name));
        }
        inventory.merge(&snapshot);
        inventory
    }

    /// One host's mount report, as its agent writes it.
    fn mounts(client: &str, sources: &[&str]) -> Observation {
        let described: Vec<serde_json::Value> = sources
            .iter()
            .map(|source| {
                let (server, target) = source.split_once(":/").expect("server:/export");
                serde_json::json!({
                    "source": source,
                    "target": format!("/mnt/{target}"),
                    "fstype": "nfs4",
                    "server": server,
                    "read_only": false,
                })
            })
            .collect();

        Observation::new(ProbeId::new(PROBE_CLIENT_MOUNT), host_id(client), ProbeStatus::Ok)
            .with_payload(serde_json::json!({ "mounts": described }))
    }

    fn edges_of(topology: &StorageTopology, kind: DependencyType) -> Vec<(EntityId, EntityId)> {
        topology
            .snapshot
            .dependencies
            .iter()
            .filter(|edge| edge.dependency_type == kind)
            .map(|edge| (edge.source, edge.target))
            .collect()
    }

    #[test]
    fn clients_of_one_server_become_clients_of_one_storage() {
        // The whole point: this is the graph that turns nine simultaneous
        // client faults into one incident naming the fileserver.
        let inventory = inventory_of(&["node01", "node02", "filesrv01"]);
        let observations = [
            mounts("node01", &["filesrv01:/data"]),
            mounts("node02", &["filesrv01:/data"]),
        ];

        let topology = topology_from_mounts(ENV, &inventory, observations.iter());

        assert_eq!(
            edges_of(&topology, DependencyType::Provides),
            vec![(storage_id("filesrv01"), host_id("filesrv01"))]
        );
        let uses = edges_of(&topology, DependencyType::UsesStorage);
        assert_eq!(uses.len(), 2, "{uses:#?}");
        assert!(uses.contains(&(host_id("node01"), storage_id("filesrv01"))));
        assert!(uses.contains(&(host_id("node02"), storage_id("filesrv01"))));
        assert!(topology.unresolved.is_empty());
    }

    #[test]
    fn a_client_of_two_servers_gets_an_edge_to_each() {
        let inventory = inventory_of(&["node01", "filesrv01", "filesrv02"]);
        let observations = [mounts("node01", &["filesrv01:/data", "filesrv02:/home"])];

        let topology = topology_from_mounts(ENV, &inventory, observations.iter());

        let uses = edges_of(&topology, DependencyType::UsesStorage);
        assert_eq!(uses.len(), 2, "{uses:#?}");
        assert!(uses.contains(&(host_id("node01"), storage_id("filesrv01"))));
        assert!(uses.contains(&(host_id("node01"), storage_id("filesrv02"))));
    }

    #[test]
    fn several_exports_from_one_server_are_one_storage_entity() {
        // The question the graph answers is "did this fileserver take nodes
        // down with it". Splitting per export would spread the answer across
        // exports without making it more accurate.
        let inventory = inventory_of(&["node01", "filesrv01"]);
        let observations = [mounts("node01", &["filesrv01:/data", "filesrv01:/home"])];

        let topology = topology_from_mounts(ENV, &inventory, observations.iter());

        assert_eq!(edges_of(&topology, DependencyType::UsesStorage).len(), 1);
        assert_eq!(
            topology
                .snapshot
                .entities
                .iter()
                .filter(|e| e.entity_type == EntityType::Storage)
                .count(),
            1
        );
    }

    #[test]
    fn a_server_nobody_declared_is_created_from_its_name() {
        // `filesrv01:/data` is evidence that a machine called filesrv01 exists
        // and serves storage, which is exactly what the static configuration
        // was being used to say.
        let inventory = inventory_of(&["node01"]);
        let observations = [mounts("node01", &["filesrv01:/data"])];

        let topology = topology_from_mounts(ENV, &inventory, observations.iter());
        let server = topology
            .snapshot
            .entities
            .iter()
            .find(|e| e.entity_type == EntityType::Host)
            .expect("the server is declared");

        assert_eq!(server.canonical_name, "filesrv01");
        assert!(
            server.capabilities.has(well_known::STORAGE_NFS_SERVER),
            "so the export probes run against it"
        );
    }

    #[test]
    fn a_fully_qualified_server_is_the_same_machine_as_its_short_name() {
        let inventory = inventory_of(&["node01", "filesrv01"]);
        let observations = [mounts("node01", &["filesrv01.cluster.example:/data"])];

        let topology = topology_from_mounts(ENV, &inventory, observations.iter());

        assert_eq!(
            edges_of(&topology, DependencyType::UsesStorage),
            vec![(host_id("node01"), storage_id("filesrv01"))],
            "an FQDN mount must not create a second identity for a known host"
        );
    }

    #[test]
    fn an_address_that_belongs_to_a_known_host_resolves_to_it() {
        let mut inventory = inventory_of(&["node01"]);
        let mut snapshot = InventorySnapshot::new(DiscoverySource::AgentRegistration);
        let mut server = ManagedEntity::new(ENV, EntityType::Host, "filesrv01");
        server.metadata = serde_json::json!({"addresses": ["10.0.0.4"]});
        snapshot.add_entity(server);
        inventory.merge(&snapshot);

        let observations = [mounts("node01", &["10.0.0.4:/data"])];
        let topology = topology_from_mounts(ENV, &inventory, observations.iter());

        assert_eq!(
            edges_of(&topology, DependencyType::UsesStorage),
            vec![(host_id("node01"), storage_id("filesrv01"))]
        );
        assert!(topology.unresolved.is_empty());
    }

    #[test]
    fn an_address_belonging_to_nobody_is_reported_rather_than_invented() {
        // ADR 0001: an address is not an identity. Minting `host/10.0.0.9`
        // would give one machine a second, permanent identity the moment it
        // registers under its name.
        let inventory = inventory_of(&["node01", "node02"]);
        let observations = [
            mounts("node01", &["10.0.0.9:/data"]),
            mounts("node02", &["10.0.0.9:/data"]),
        ];

        let topology = topology_from_mounts(ENV, &inventory, observations.iter());

        assert!(topology.snapshot.entities.is_empty(), "nothing is invented");
        assert!(topology.snapshot.dependencies.is_empty());
        assert_eq!(
            topology.unresolved,
            vec![UnresolvedServer {
                server: "10.0.0.9".into(),
                clients: vec!["node01".into(), "node02".into()],
            }],
            "and the operator is told what to declare"
        );
    }

    #[test]
    fn a_server_mounting_its_own_export_is_not_its_own_client() {
        // Otherwise the fileserver becomes a suspected cause of its own
        // storage failure by way of an edge to itself.
        let inventory = inventory_of(&["filesrv01"]);
        let observations = [mounts("filesrv01", &["filesrv01:/data"])];

        let topology = topology_from_mounts(ENV, &inventory, observations.iter());
        assert!(edges_of(&topology, DependencyType::UsesStorage).is_empty());
        assert_eq!(edges_of(&topology, DependencyType::Provides).len(), 1);
    }

    #[test]
    fn a_host_with_no_nfs_mounts_contributes_nothing() {
        let inventory = inventory_of(&["node01"]);
        let observation = Observation::new(
            ProbeId::new(PROBE_CLIENT_MOUNT),
            host_id("node01"),
            ProbeStatus::NotApplicable,
        )
        .with_payload(serde_json::json!({"mounts": []}));

        let topology = topology_from_mounts(ENV, &inventory, [&observation]);
        assert!(topology.snapshot.entities.is_empty());
        assert!(topology.unresolved.is_empty());
    }

    #[test]
    fn observations_from_other_probes_are_ignored() {
        let inventory = inventory_of(&["node01"]);
        let observation = Observation::new(ProbeId::new("host.metrics"), host_id("node01"), ProbeStatus::Ok)
            .with_payload(serde_json::json!({"mounts": [{"server": "filesrv01"}]}));

        let topology = topology_from_mounts(ENV, &inventory, [&observation]);
        assert!(topology.snapshot.entities.is_empty());
    }

    #[test]
    fn the_older_flat_payload_is_still_understood() {
        // Payloads written before the per-mount detail existed carry only the
        // list of servers. An upgrade must not lose the topology until every
        // agent has reported again.
        let inventory = inventory_of(&["node01", "filesrv01"]);
        let observation = Observation::new(ProbeId::new(PROBE_CLIENT_MOUNT), host_id("node01"), ProbeStatus::Ok)
            .with_payload(serde_json::json!({"servers": ["filesrv01"]}));

        let topology = topology_from_mounts(ENV, &inventory, [&observation]);
        assert_eq!(
            edges_of(&topology, DependencyType::UsesStorage),
            vec![(host_id("node01"), storage_id("filesrv01"))]
        );
    }

    #[test]
    fn the_result_is_stable_across_passes() {
        // Merging happens every cycle. Identities derived from names are
        // deterministic, so re-deriving must produce exactly the same graph
        // rather than churning entities in and out of the inventory.
        let inventory = inventory_of(&["node01", "filesrv01"]);
        let observations = [mounts("node01", &["filesrv01:/data"])];

        let first = topology_from_mounts(ENV, &inventory, observations.iter());
        let second = topology_from_mounts(ENV, &inventory, observations.iter());

        assert_eq!(
            edges_of(&first, DependencyType::UsesStorage),
            edges_of(&second, DependencyType::UsesStorage)
        );
        assert_eq!(
            first.snapshot.entities.iter().map(|e| e.id).collect::<Vec<_>>(),
            second.snapshot.entities.iter().map(|e| e.id).collect::<Vec<_>>()
        );
    }
}
