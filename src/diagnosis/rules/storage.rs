//! Storage diagnosis rules.
//!
//! The distinction that matters here is between a fault at the server and a
//! fault at one client, because they are worth waking different people for:
//!
//! * **Shared storage failure** — several clients of one storage entity fail
//!   together. Something upstream is broken and everything behind it is
//!   affected.
//! * **Client-local failure** — one client fails while its neighbours on the
//!   same storage are fine. The server is innocent.
//!
//! Getting this backwards is expensive in both directions: declaring a
//! fileserver dead because one client's mount is wedged sends people to the
//! wrong machine, and reporting five separate client faults hides the single
//! cause behind them.
//!
//! Nothing in this file names NFS. The rules work on storage entities and the
//! dependency graph, so replacing NFS with anything else needs no change here
//! (SPEC.md §78, §146).

use std::collections::{BTreeMap, BTreeSet};

use crate::diagnosis::{kind, Confidence, Diagnosis, DiagnosisContext, DiagnosisRule, RuleId};
use crate::entity::{EntityId, EntityType};
use crate::probes::nfs::{PROBE_CLIENT_IO, PROBE_CLIENT_MOUNT, PROBE_SERVER_EXPORTS, PROBE_SERVER_PORT};
use crate::state::{Health, StateComponent};

/// Whether a host looks reachable and alive.
fn host_looks_healthy(context: &DiagnosisContext, host: EntityId) -> bool {
    context.is_healthy(host, StateComponent::Network)
        && (context.is_healthy(host, StateComponent::Agent) || context.is_healthy(host, StateComponent::Ssh))
}

/// Whether anything in the cluster actually uses what this host serves.
///
/// The capability that gates the export probes is detected from the server
/// software being *present*, which is true of many machines that serve nothing
/// -- clients usually have it too. On such a host the export service is often
/// running as well, answering on its port with an empty export list. That is
/// not a fault. It is what an ordinary host with the package installed looks
/// like, and there is nobody to be refused.
///
/// The graph knows the difference, because it is derived from mounts that
/// clients actually have: a storage domain exists for a host only when someone
/// mounts from it. So this rule asks the graph rather than the packages.
///
/// A fileserver whose exports vanish keeps its clients here -- a mount that is
/// refused stays in the client's mount table -- so the case this rule exists
/// for still fires.
fn serves_anyone(context: &DiagnosisContext, host: EntityId) -> bool {
    use crate::dependency::DependencyType;

    // Direct edges, not reachability. Transitive closure is the wrong tool
    // here and spectacularly so: a head node hosts the scheduler service,
    // every compute node depends on the scheduler, and some of those nodes
    // provide storage -- so "everything that transitively depends on the head
    // node" reaches most of the cluster, including storage domains it has
    // nothing to do with. Walking one more hop from there reaches every host
    // again. Asked that way, every host serves everyone.
    //
    // The two edges that actually mean "this host serves storage that someone
    // uses" are the ones the derivation writes, and they are one hop each.
    let graph = context.inventory.graph();
    let provided: Vec<EntityId> = graph
        .edges()
        .iter()
        .filter(|edge| edge.dependency_type == DependencyType::Provides && edge.target == host)
        .map(|edge| edge.source)
        .collect();

    graph.edges().iter().any(|edge| {
        edge.dependency_type == DependencyType::UsesStorage && provided.contains(&edge.target) && edge.source != host
    })
}

/// The storage domains a host uses, from its own edges.
///
/// Direct edges, for the same reason [`serves_anyone`] uses them: a real
/// cluster's graph contains a loop -- storage is provided by a host that
/// depends on the scheduler, which is hosted on a machine that mounts storage
/// -- and a transitive walk from any host arrives at every storage domain in
/// the cluster. Asked that way, a host "uses" storage it has never heard of,
/// and the diagnosis says so out loud: one node was reported as having lost
/// access to four domains when it declares one.
fn storages_used(context: &DiagnosisContext, host: EntityId) -> Vec<EntityId> {
    use crate::dependency::DependencyType;

    context
        .inventory
        .graph()
        .dependencies_of_type(host, &DependencyType::UsesStorage)
        .into_iter()
        .map(|edge| edge.target)
        .collect()
}

/// The hosts that use a storage domain, from their own edges.
fn clients_of(context: &DiagnosisContext, storage: EntityId) -> BTreeSet<EntityId> {
    use crate::dependency::DependencyType;

    context
        .inventory
        .graph()
        .edges()
        .iter()
        .filter(|edge| edge.dependency_type == DependencyType::UsesStorage && edge.target == storage)
        .map(|edge| edge.source)
        .filter(|id| context.entity(*id).is_some_and(|e| e.entity_type == EntityType::Host))
        .collect()
}

/// Whether an entity's storage is currently impaired.
fn storage_is_impaired(context: &DiagnosisContext, entity: EntityId) -> bool {
    matches!(
        context.component(entity, StateComponent::Storage),
        Health::Degraded | Health::Unavailable
    )
}

/// Evidence ids for an entity.
fn evidence_for(context: &DiagnosisContext, entity: EntityId) -> Vec<crate::observation::ObservationId> {
    context
        .observations
        .for_entity(entity)
        .into_iter()
        .map(|o| o.id)
        .collect()
}

/// A fileserver's export service has failed while the host itself is up.
///
/// SPEC.md §172: the fileserver and its NFS service are different things, and
/// only one of them being broken is a much smaller problem.
pub struct StorageServiceFailure;

impl DiagnosisRule for StorageServiceFailure {
    fn id(&self) -> RuleId {
        RuleId::new("storage.service_failure")
    }

    fn description(&self) -> &str {
        "a storage server's export service has failed while the host is still up"
    }

    fn evaluate(&self, context: &DiagnosisContext) -> Vec<Diagnosis> {
        let mut diagnoses = Vec::new();

        for host in context.entities_of_type(EntityType::Host) {
            let port = context.observation(host.id, PROBE_SERVER_PORT);
            let port_failed = port.is_some_and(|o| o.status.is_bad());
            let port_answers = port.is_some_and(|o| !o.status.is_bad());

            // Exports being empty is a fact the probe records. Whether it is a
            // fault depends on this: **is anything listening on 2049?**
            //
            // If something is, a client will connect and be refused, which is
            // the confusing outage this rule exists to name. If nothing is,
            // this host is simply not an NFS server -- and most hosts are not.
            // The capability that gates the export probe is detected from
            // `/etc/exports` existing or `exportfs` being installed, which is
            // true of any machine with the NFS packages, clients included. A
            // head node that mounts five shares and exports none of them was
            // reported as a broken fileserver at critical.
            let exports_empty = context
                .observation(host.id, PROBE_SERVER_EXPORTS)
                .and_then(|o| o.payload.get("export_count").and_then(|v| v.as_u64()))
                .is_some_and(|count| count == 0);
            let serving_nothing = exports_empty && port_answers;

            if !port_failed && !serving_nothing {
                continue;
            }

            // Nobody uses what this host serves, so there is nothing here to
            // fail. A head node with the server package installed answers on
            // its export port and exports nothing, which satisfies the check
            // above -- and it was reported as a broken fileserver at critical
            // the moment peers began probing that port.
            if !serves_anyone(context, host.id) {
                continue;
            }

            // The whole point of this rule: the machine is fine, the service is
            // not. Without evidence of the former, this is a host problem.
            if !host_looks_healthy(context, host.id) {
                continue;
            }

            let detail = if port_failed && exports_empty {
                "its export port is not answering and it exports nothing"
            } else if port_failed {
                "its export port is not answering"
            } else {
                "its export port answers while it exports nothing, so clients are refused rather than timed out"
            };

            diagnoses.push(
                Diagnosis::new(kind::NFS_SERVICE_FAILURE, self.id(), Confidence::High)
                    .affecting([host.id])
                    .rooted_at([host.id])
                    .with_evidence(evidence_for(context, host.id))
                    .with_summary(format!("{} is up but {detail}", host.canonical_name))
                    .recommending(vec![
                        format!("systemctl status nfs-server  # on {}", host.canonical_name),
                        format!("exportfs -v  # on {}", host.canonical_name),
                        format!("ss -lntp sport = :2049  # on {}", host.canonical_name),
                    ]),
            );
        }

        diagnoses
    }
}

/// Several clients of one storage entity are impaired at the same time.
///
/// The group is derived from the dependency graph rather than declared, so a
/// new storage domain needs no code change (SPEC.md §29, §95).
pub struct SharedStorageFailure;

impl DiagnosisRule for SharedStorageFailure {
    fn id(&self) -> RuleId {
        RuleId::new("storage.shared_failure")
    }

    fn description(&self) -> &str {
        "several clients of the same storage are impaired together"
    }

    fn evaluate(&self, context: &DiagnosisContext) -> Vec<Diagnosis> {
        let impaired: Vec<EntityId> = context
            .entities_of_type(EntityType::Host)
            .into_iter()
            .filter(|host| storage_is_impaired(context, host.id))
            .map(|host| host.id)
            .collect();

        if impaired.len() < 2 {
            // One client is not a pattern. The client-local rule owns that case.
            return Vec::new();
        }

        // Grouped by the storage each host *declares* it uses, not by what the
        // graph can reach from it. Reachability was the original approach and
        // it cannot survive a loop: a real cluster's storage is provided by a
        // host that depends on the scheduler, which is hosted on a machine that
        // mounts storage, so walking upstream from any host arrives at every
        // storage domain there is. Every impaired host would then "share" every
        // domain with every other, and the shared-storage rule would name a
        // fileserver for a fault that has nothing to do with it.
        let mut by_storage: BTreeMap<EntityId, BTreeSet<EntityId>> = BTreeMap::new();
        for host in &impaired {
            for storage in storages_used(context, *host) {
                by_storage.entry(storage).or_default().insert(*host);
            }
        }
        by_storage.retain(|_, members| members.len() > 1);

        let mut diagnoses = Vec::new();

        for (upstream, members) in by_storage {
            let Some(entity) = context.entity(upstream) else {
                continue;
            };
            if entity.entity_type != EntityType::Storage {
                continue;
            }

            // Is this really shared, or is it simply that every client of this
            // storage happens to be broken for its own reasons? If some client
            // of the same storage is fine, the storage is probably not at fault.
            let all_clients: BTreeSet<EntityId> = clients_of(context, upstream);

            let healthy_clients: Vec<EntityId> =
                all_clients.iter().copied().filter(|id| !members.contains(id)).collect();

            // Confidence follows the evidence: every client affected is a much
            // stronger signal than some of them.
            let confidence = if healthy_clients.is_empty() {
                Confidence::High
            } else {
                Confidence::Medium
            };

            // The suspected cause is whatever provides this storage, if the
            // graph says; otherwise the storage entity itself.
            let providers: Vec<EntityId> = context
                .inventory
                .graph()
                .dependencies_of(upstream)
                .into_iter()
                .map(|edge| edge.target)
                .collect();
            let roots = if providers.is_empty() {
                vec![upstream]
            } else {
                providers
            };

            let mut evidence = evidence_for(context, upstream);
            for member in &members {
                evidence.extend(evidence_for(context, *member));
            }

            let names: Vec<String> = members
                .iter()
                .filter_map(|id| context.entity(*id))
                .map(|e| e.canonical_name.clone())
                .collect();

            let mut affected: Vec<EntityId> = members.iter().copied().collect();
            affected.push(upstream);

            diagnoses.push(
                Diagnosis::new(kind::SHARED_STORAGE_FAILURE, self.id(), confidence)
                    .affecting(affected)
                    .rooted_at(roots)
                    .with_evidence(evidence)
                    .with_summary(format!(
                        "{} client(s) of {} are impaired together: {}",
                        members.len(),
                        entity.canonical_name,
                        names.join(", ")
                    ))
                    .recommending(vec![
                        format!("sentinel entity show {}", entity.canonical_name),
                        "sentinel dependency list".to_string(),
                    ]),
            );
        }

        diagnoses
    }
}

/// One client's storage access is broken while its neighbours are fine.
///
/// SPEC.md §174: this must not be reported as a fileserver failure. The most
/// expensive possible mistake here is sending someone to reboot a fileserver
/// that five other nodes are happily using.
pub struct ClientLocalStorageFailure;

impl DiagnosisRule for ClientLocalStorageFailure {
    fn id(&self) -> RuleId {
        RuleId::new("storage.client_local_failure")
    }

    fn description(&self) -> &str {
        "one client's storage is impaired while others on the same storage are fine"
    }

    fn evaluate(&self, context: &DiagnosisContext) -> Vec<Diagnosis> {
        let mut diagnoses = Vec::new();

        for host in context.entities_of_type(EntityType::Host) {
            if !storage_is_impaired(context, host.id) {
                continue;
            }

            // Which storage does this client depend on, and how are its
            // neighbours faring?
            let storages = storages_used(context, host.id);

            if storages.is_empty() {
                continue;
            }

            // A peer is a host depending on the same storage.
            let mut peers: BTreeSet<EntityId> = BTreeSet::new();
            for storage in &storages {
                peers.extend(clients_of(context, *storage).into_iter().filter(|id| *id != host.id));
            }

            // With no peers there is nothing to compare against, and "only this
            // client is affected" is not a claim the evidence supports.
            if peers.is_empty() {
                continue;
            }

            // If any peer is also impaired, this is not client-local; the
            // shared rule owns it.
            if peers.iter().any(|peer| storage_is_impaired(context, *peer)) {
                continue;
            }

            let healthy_peers: Vec<String> = peers
                .iter()
                .filter_map(|id| context.entity(*id))
                .map(|e| e.canonical_name.clone())
                .collect();

            let storage_names: Vec<String> = storages
                .iter()
                .filter_map(|id| context.entity(*id))
                .map(|e| e.canonical_name.clone())
                .collect();

            let stuck = context
                .observation(host.id, PROBE_CLIENT_IO)
                .map(|o| {
                    matches!(
                        o.status,
                        crate::observation::ProbeStatus::Stuck | crate::observation::ProbeStatus::Timeout
                    )
                })
                .unwrap_or(false);

            let mut actions = vec![
                format!("findmnt -t nfs,nfs4  # on {}", host.canonical_name),
                format!("sentinel entity show {}", host.canonical_name),
            ];
            if stuck {
                actions.push(format!(
                    "cat /proc/*/stack  # on {}, to find blocked tasks",
                    host.canonical_name
                ));
            }

            diagnoses.push(
                Diagnosis::new(kind::NFS_CLIENT_FAILURE, self.id(), Confidence::High)
                    .affecting([host.id])
                    .rooted_at([host.id])
                    .with_evidence(evidence_for(context, host.id))
                    .with_summary(format!(
                        "{}'s access to {} is impaired, but {} using the same storage {} fine",
                        host.canonical_name,
                        storage_names.join(", "),
                        healthy_peers.join(", "),
                        if healthy_peers.len() == 1 { "is" } else { "are" }
                    ))
                    .recommending(actions),
            );
        }

        diagnoses
    }
}

/// Which state component a storage probe informs, for registration.
pub fn storage_probe_ids() -> [&'static str; 4] {
    [
        PROBE_CLIENT_MOUNT,
        PROBE_CLIENT_IO,
        PROBE_SERVER_PORT,
        PROBE_SERVER_EXPORTS,
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::capability::CapabilitySet;
    use crate::dependency::{DependencyEdge, DependencyType};
    use crate::diagnosis::ObservationIndex;
    use crate::entity::{EntityKey, ManagedEntity};
    use crate::inventory::Inventory;
    use crate::observation::{Observation, ProbeStatus};
    use crate::probes::ProbeId;
    use crate::state::{ComponentState, EntityState};
    use std::collections::HashMap;

    /// A small cluster with two storage domains, built for these tests.
    struct World {
        inventory: Inventory,
        states: HashMap<EntityId, EntityState>,
        observations: ObservationIndex,
    }

    fn host_id(name: &str) -> EntityId {
        EntityKey::new("lab", EntityType::Host, name).entity_id()
    }

    fn storage_id(name: &str) -> EntityId {
        EntityKey::new("lab", EntityType::Storage, name).entity_id()
    }

    impl World {
        fn new() -> Self {
            Self {
                inventory: Inventory::new(),
                states: HashMap::new(),
                observations: ObservationIndex::new(),
            }
        }

        fn host(mut self, name: &str) -> Self {
            self.inventory.insert_entity(
                ManagedEntity::new("lab", EntityType::Host, name)
                    .with_capabilities(CapabilitySet::from_iter(["storage.nfs.client"])),
            );
            self
        }

        fn fileserver(mut self, name: &str) -> Self {
            self.inventory.insert_entity(
                ManagedEntity::new("lab", EntityType::Host, name)
                    .with_capabilities(CapabilitySet::from_iter(["storage.nfs.server"])),
            );
            self
        }

        fn storage(mut self, name: &str, provided_by: &str) -> Self {
            self.inventory
                .insert_entity(ManagedEntity::new("lab", EntityType::Storage, name));
            self.inventory.insert_dependency(DependencyEdge::new(
                storage_id(name),
                host_id(provided_by),
                DependencyType::Provides,
            ));
            self
        }

        fn uses(mut self, client: &str, storage: &str) -> Self {
            self.inventory.insert_dependency(DependencyEdge::new(
                host_id(client),
                storage_id(storage),
                DependencyType::UsesStorage,
            ));
            self
        }

        /// The chain a head node really has: it hosts the scheduler service,
        /// and every compute node depends on the scheduler.
        fn scheduler_on(mut self, head: &str, computes: &[&str]) -> Self {
            let scheduler = EntityKey::new("lab", EntityType::Scheduler, "slurm").entity_id();
            let service = EntityKey::new("lab", EntityType::Service, "slurmctld").entity_id();
            self.inventory
                .insert_entity(ManagedEntity::new("lab", EntityType::Scheduler, "slurm"));
            self.inventory
                .insert_entity(ManagedEntity::new("lab", EntityType::Service, "slurmctld"));
            self.inventory
                .insert_dependency(DependencyEdge::new(service, host_id(head), DependencyType::HostedOn));
            self.inventory
                .insert_dependency(DependencyEdge::new(scheduler, service, DependencyType::Provides));
            for compute in computes {
                self.inventory.insert_dependency(DependencyEdge::new(
                    host_id(compute),
                    scheduler,
                    DependencyType::UsesScheduler,
                ));
            }
            self
        }

        fn storage_health(mut self, name: &str, health: Health) -> Self {
            let id = host_id(name);
            let state = self.states.entry(id).or_insert_with(|| EntityState::unknown(id));
            state.set_component(StateComponent::Storage, ComponentState::new(health));
            self
        }

        fn host_is_up(mut self, name: &str) -> Self {
            let id = host_id(name);
            let state = self.states.entry(id).or_insert_with(|| EntityState::unknown(id));
            state.set_component(StateComponent::Network, ComponentState::new(Health::Healthy));
            state.set_component(StateComponent::Agent, ComponentState::new(Health::Healthy));
            self
        }

        fn host_is_down(mut self, name: &str) -> Self {
            let id = host_id(name);
            let state = self.states.entry(id).or_insert_with(|| EntityState::unknown(id));
            state.set_component(StateComponent::Network, ComponentState::new(Health::Unavailable));
            state.set_component(StateComponent::Agent, ComponentState::new(Health::Unavailable));
            self
        }

        fn observe(mut self, name: &str, probe: &str, status: ProbeStatus) -> Self {
            self.observations
                .insert(Observation::new(ProbeId::new(probe), host_id(name), status));
            self
        }

        /// What the export probe found, as it records it: a count, not a verdict.
        fn exports(mut self, name: &str, count: u64) -> Self {
            self.observations.insert(
                Observation::new(ProbeId::new(PROBE_SERVER_EXPORTS), host_id(name), ProbeStatus::Ok)
                    .with_payload(serde_json::json!({"export_count": count})),
            );
            self
        }

        fn evaluate(&self, rule: &dyn DiagnosisRule) -> Vec<Diagnosis> {
            let context = DiagnosisContext {
                environment: "lab",
                inventory: &self.inventory,
                states: &self.states,
                observations: &self.observations,
            };
            rule.evaluate(&context)
        }
    }

    /// Two clients on storage-a, two on storage-b, one fileserver each.
    fn two_domains() -> World {
        World::new()
            .fileserver("fs-a")
            .fileserver("fs-b")
            .storage("storage-a", "fs-a")
            .storage("storage-b", "fs-b")
            .host("c1")
            .host("c2")
            .host("c3")
            .host("c4")
            .uses("c1", "storage-a")
            .uses("c2", "storage-a")
            .uses("c3", "storage-b")
            .uses("c4", "storage-b")
    }

    // --- NFS_SERVICE_FAILURE ---------------------------------------------

    #[test]
    fn a_fileserver_whose_export_port_is_down_is_diagnosed() {
        let world = two_domains()
            .host_is_up("fs-a")
            .observe("fs-a", PROBE_SERVER_PORT, ProbeStatus::Failed);

        let diagnoses = world.evaluate(&StorageServiceFailure);
        assert_eq!(diagnoses.len(), 1);
        assert!(diagnoses[0].is(kind::NFS_SERVICE_FAILURE));
        assert!(diagnoses[0].summary.contains("is up but"), "{}", diagnoses[0].summary);
    }

    #[test]
    fn a_fileserver_answering_on_2049_while_exporting_nothing_is_diagnosed() {
        // The confusing outage this rule is for: the daemon is listening, so
        // nothing looks down, and every client is refused rather than timed
        // out.
        let world = two_domains()
            .host_is_up("fs-a")
            .observe("fs-a", PROBE_SERVER_PORT, ProbeStatus::Ok)
            .exports("fs-a", 0);

        let diagnoses = world.evaluate(&StorageServiceFailure);
        assert_eq!(diagnoses.len(), 1, "{diagnoses:#?}");
        assert!(
            diagnoses[0].summary.contains("exports nothing"),
            "{}",
            diagnoses[0].summary
        );
    }

    #[test]
    fn a_host_nobody_uses_is_not_diagnosed_even_when_its_port_answers() {
        // Found on a live cluster the day peers began probing the export port.
        // The head node has the server package installed, so the capability is
        // in force and the service answers -- with nothing exported, because it
        // is a client, not a server. That satisfied the "port answers while it
        // exports nothing" check and raised a critical against a machine doing
        // exactly what it should.
        //
        // The graph is what tells the difference: a storage domain exists for a
        // host only when someone actually mounts from it.
        let world = World::new()
            .fileserver("head")
            .host("c1")
            .observe("head", PROBE_SERVER_PORT, ProbeStatus::Ok)
            .exports("head", 0)
            .host_is_up("head");

        assert!(
            world.evaluate(&StorageServiceFailure).is_empty(),
            "nobody mounts from this host, so there is nothing here to fail"
        );
    }

    #[test]
    fn a_client_is_only_said_to_use_the_storage_it_declares() {
        // With the scheduler loop present, walking the graph upstream from any
        // host reaches every storage domain in the cluster. A node that
        // declares one mount was reported as having lost access to four, and
        // the summary said so in as many words.
        let world = two_domains()
            .scheduler_on("head", &["c1", "c2", "c3", "c4"])
            .storage("c2-scratch", "c2")
            .uses("head", "c2-scratch")
            .host_is_up("c3")
            .storage_health("c3", Health::Unavailable);

        let diagnoses = world.evaluate(&ClientLocalStorageFailure);
        assert_eq!(diagnoses.len(), 1, "{diagnoses:#?}");

        let summary = &diagnoses[0].summary;
        assert!(summary.contains("storage-b"), "{summary}");
        for other in ["storage-a", "c2-scratch"] {
            assert!(!summary.contains(other), "c3 does not use {other}: {summary}");
        }
    }

    #[test]
    fn a_shared_failure_groups_by_declared_use_not_reachability() {
        // The same loop, on the other rule. If impaired hosts were grouped by
        // what they can reach, every one of them would share every domain with
        // every other, and a fileserver would be named for a fault nothing to
        // do with it.
        let world = two_domains()
            .scheduler_on("head", &["c1", "c2", "c3", "c4"])
            .storage("c2-scratch", "c2")
            .uses("head", "c2-scratch")
            .host_is_up("c1")
            .host_is_up("c2")
            .storage_health("c1", Health::Unavailable)
            .storage_health("c2", Health::Unavailable);

        let diagnoses = world.evaluate(&SharedStorageFailure);
        assert_eq!(diagnoses.len(), 1, "{diagnoses:#?}");
        assert!(
            diagnoses[0].summary.contains("storage-a"),
            "c1 and c2 share storage-a and nothing else: {}",
            diagnoses[0].summary
        );
    }

    #[test]
    fn a_head_node_is_not_dragged_in_through_the_scheduler() {
        // The first attempt at this asked the graph for everything that
        // *transitively* depends on the host, which on a real cluster reaches
        // most of it: the head node hosts the scheduler service, every compute
        // node depends on the scheduler, and some of those nodes serve
        // storage. One hop further reaches every host again. Asked that way,
        // every host serves everyone, and the head node kept its critical.
        //
        // Only two edges mean "this host serves storage someone uses", and
        // they are one hop each.
        let world = World::new()
            .fileserver("head")
            .host("c1")
            .host("c2")
            .scheduler_on("head", &["c1", "c2"])
            // c1 serves storage, and the head node is its only client -- the
            // shape that made the transitive version reach back round.
            .storage("storage-c1", "c1")
            .uses("head", "storage-c1")
            .observe("head", PROBE_SERVER_PORT, ProbeStatus::Ok)
            .exports("head", 0)
            .host_is_up("head");

        assert!(
            world.evaluate(&StorageServiceFailure).is_empty(),
            "nobody mounts from the head node; the scheduler chain is not storage"
        );
    }

    #[test]
    fn a_fileserver_someone_uses_is_still_diagnosed_when_it_serves_nothing() {
        // The other half: same evidence, but the graph says clients depend on
        // it. Those clients connect and are refused, which is the confusing
        // outage this rule exists to name.
        let world = two_domains()
            .host_is_up("fs-a")
            .observe("fs-a", PROBE_SERVER_PORT, ProbeStatus::Ok)
            .exports("fs-a", 0);

        let diagnoses = world.evaluate(&StorageServiceFailure);
        assert_eq!(diagnoses.len(), 1, "{diagnoses:#?}");
        assert!(
            diagnoses[0].summary.contains("exports nothing"),
            "{}",
            diagnoses[0].summary
        );
    }

    #[test]
    fn a_port_failure_on_a_host_nobody_uses_is_also_ignored() {
        // The same reasoning applies to the other branch. A stopped export
        // service on a machine nothing mounts from harms no one.
        let world = World::new()
            .fileserver("head")
            .host("c1")
            .observe("head", PROBE_SERVER_PORT, ProbeStatus::Failed)
            .host_is_up("head");

        assert!(world.evaluate(&StorageServiceFailure).is_empty());
    }

    #[test]
    fn a_host_that_simply_is_not_a_fileserver_is_not_diagnosed() {
        // The capability gating the export probe is detected from
        // `/etc/exports` existing or `exportfs` being installed, which is true
        // of any host with the NFS packages -- every client included. Without
        // something listening on 2049, "exports nothing" describes an ordinary
        // compute node, and a head node mounting five shares and exporting
        // none was reported as a broken fileserver at critical.
        let world = two_domains().host_is_up("fs-a").exports("fs-a", 0);

        assert!(
            world.evaluate(&StorageServiceFailure).is_empty(),
            "nothing is listening; this host does not serve NFS"
        );
    }

    #[test]
    fn a_fileserver_with_exports_is_not_diagnosed() {
        let world = two_domains()
            .host_is_up("fs-a")
            .observe("fs-a", PROBE_SERVER_PORT, ProbeStatus::Ok)
            .exports("fs-a", 3);

        assert!(world.evaluate(&StorageServiceFailure).is_empty());
    }

    #[test]
    fn the_summary_reads_as_one_sentence() {
        // It read "parent is up but the port answers but nothing is exported".
        let world = two_domains()
            .host_is_up("fs-a")
            .observe("fs-a", PROBE_SERVER_PORT, ProbeStatus::Ok)
            .exports("fs-a", 0);
        let summary = world.evaluate(&StorageServiceFailure)[0].summary.clone();

        assert!(!summary.contains("but the port answers but"), "{summary}");
        assert_eq!(summary.matches(" but ").count(), 1, "{summary}");
    }

    #[test]
    fn an_unreachable_fileserver_is_not_diagnosed_as_a_service_failure() {
        // The machine is gone; blaming the export service would send someone
        // to restart a daemon on a host that is not there.
        let world = two_domains()
            .host_is_down("fs-a")
            .observe("fs-a", PROBE_SERVER_PORT, ProbeStatus::Failed);
        assert!(world.evaluate(&StorageServiceFailure).is_empty());
    }

    #[test]
    fn a_healthy_fileserver_is_not_diagnosed() {
        let world = two_domains()
            .host_is_up("fs-a")
            .observe("fs-a", PROBE_SERVER_PORT, ProbeStatus::Ok)
            .observe("fs-a", PROBE_SERVER_EXPORTS, ProbeStatus::Ok);
        assert!(world.evaluate(&StorageServiceFailure).is_empty());
    }

    // --- SHARED_STORAGE_FAILURE ------------------------------------------

    #[test]
    fn clients_of_one_storage_failing_together_is_a_shared_failure() {
        // SPEC.md §173.
        let world = two_domains()
            .storage_health("c1", Health::Unavailable)
            .storage_health("c2", Health::Unavailable);

        let diagnoses = world.evaluate(&SharedStorageFailure);
        assert_eq!(diagnoses.len(), 1, "{diagnoses:#?}");

        let diagnosis = &diagnoses[0];
        assert!(diagnosis.is(kind::SHARED_STORAGE_FAILURE));
        assert_eq!(
            diagnosis.confidence,
            Confidence::High,
            "every client of this storage is affected"
        );
        assert_eq!(
            diagnosis.suspected_root_entities,
            vec![host_id("fs-a")],
            "the cause is the fileserver"
        );
        assert!(diagnosis.affected_entities.contains(&host_id("c1")));
        assert!(diagnosis.affected_entities.contains(&host_id("c2")));
        assert!(
            !diagnosis.affected_entities.contains(&host_id("c3")),
            "the other domain is untouched"
        );
    }

    #[test]
    fn one_failing_client_is_not_a_shared_failure() {
        let world = two_domains().storage_health("c1", Health::Unavailable);
        assert!(
            world.evaluate(&SharedStorageFailure).is_empty(),
            "one client is not a pattern"
        );
    }

    #[test]
    fn clients_of_different_storages_failing_is_not_one_shared_failure() {
        // They share no storage, so there is no single cause to report.
        let world = two_domains()
            .storage_health("c1", Health::Unavailable)
            .storage_health("c3", Health::Unavailable);
        assert!(world.evaluate(&SharedStorageFailure).is_empty());
    }

    #[test]
    fn a_partially_affected_storage_is_reported_with_lower_confidence() {
        // Two of three clients affected is weaker evidence than all of them.
        let world = two_domains()
            .host("c5")
            .uses("c5", "storage-a")
            .storage_health("c1", Health::Unavailable)
            .storage_health("c2", Health::Unavailable);

        let diagnoses = world.evaluate(&SharedStorageFailure);
        assert_eq!(diagnoses.len(), 1);
        assert_eq!(diagnoses[0].confidence, Confidence::Medium);
    }

    #[test]
    fn a_shared_scheduler_is_not_mistaken_for_shared_storage() {
        // Every node in a cluster shares a scheduler; that is not a storage
        // fault, and the rule must only group on storage entities.
        let mut world = two_domains();
        world
            .inventory
            .insert_entity(ManagedEntity::new("lab", EntityType::Scheduler, "sched"));
        let scheduler = EntityKey::new("lab", EntityType::Scheduler, "sched").entity_id();
        for client in ["c1", "c3"] {
            world.inventory.insert_dependency(DependencyEdge::new(
                host_id(client),
                scheduler,
                DependencyType::UsesScheduler,
            ));
        }

        let world = world
            .storage_health("c1", Health::Unavailable)
            .storage_health("c3", Health::Unavailable);

        assert!(
            world.evaluate(&SharedStorageFailure).is_empty(),
            "sharing a scheduler is not sharing storage"
        );
    }

    #[test]
    fn degraded_clients_count_as_impaired() {
        // Slow storage is still a shared storage problem.
        let world = two_domains()
            .storage_health("c1", Health::Degraded)
            .storage_health("c2", Health::Degraded);
        assert_eq!(world.evaluate(&SharedStorageFailure).len(), 1);
    }

    // --- NFS_CLIENT_FAILURE ----------------------------------------------

    #[test]
    fn one_impaired_client_among_healthy_peers_is_a_client_local_failure() {
        // SPEC.md §174, and the mistake that would send someone to reboot a
        // fileserver five other nodes are happily using.
        let world = two_domains().storage_health("c1", Health::Unavailable);

        let diagnoses = world.evaluate(&ClientLocalStorageFailure);
        assert_eq!(diagnoses.len(), 1);

        let diagnosis = &diagnoses[0];
        assert!(diagnosis.is(kind::NFS_CLIENT_FAILURE));
        assert_eq!(
            diagnosis.suspected_root_entities,
            vec![host_id("c1")],
            "the client, not the server"
        );
        assert!(diagnosis.summary.contains("c2"), "{}", diagnosis.summary);
    }

    #[test]
    fn a_client_local_failure_is_never_reported_as_a_shared_one() {
        // The two rules must partition: an operator told both learns nothing.
        let world = two_domains().storage_health("c1", Health::Unavailable);
        assert_eq!(world.evaluate(&ClientLocalStorageFailure).len(), 1);
        assert!(world.evaluate(&SharedStorageFailure).is_empty());
    }

    #[test]
    fn a_shared_failure_is_never_reported_as_client_local() {
        let world = two_domains()
            .storage_health("c1", Health::Unavailable)
            .storage_health("c2", Health::Unavailable);
        assert_eq!(world.evaluate(&SharedStorageFailure).len(), 1);
        assert!(
            world.evaluate(&ClientLocalStorageFailure).is_empty(),
            "when peers are affected too, it is not client-local"
        );
    }

    #[test]
    fn a_client_with_no_peers_is_not_diagnosed_as_client_local() {
        // With nothing to compare against, "only this client" is not a claim
        // the evidence supports.
        let world = World::new()
            .fileserver("fs-a")
            .storage("storage-a", "fs-a")
            .host("lonely")
            .uses("lonely", "storage-a")
            .storage_health("lonely", Health::Unavailable);

        assert!(world.evaluate(&ClientLocalStorageFailure).is_empty());
    }

    #[test]
    fn a_stuck_mount_gets_an_extra_investigation_hint() {
        let world =
            two_domains()
                .storage_health("c1", Health::Unavailable)
                .observe("c1", PROBE_CLIENT_IO, ProbeStatus::Stuck);

        let actions = &world.evaluate(&ClientLocalStorageFailure)[0].recommended_actions;
        assert!(actions.iter().any(|a| a.contains("stack")), "{actions:?}");
    }

    #[test]
    fn storage_diagnoses_never_recommend_changing_anything() {
        let world = two_domains()
            .host_is_up("fs-a")
            .observe("fs-a", PROBE_SERVER_PORT, ProbeStatus::Failed)
            .storage_health("c1", Health::Unavailable);

        let mut diagnoses = world.evaluate(&StorageServiceFailure);
        diagnoses.extend(world.evaluate(&ClientLocalStorageFailure));
        assert!(!diagnoses.is_empty());

        for diagnosis in &diagnoses {
            for action in &diagnosis.recommended_actions {
                for mutating in ["restart", "mount ", "umount", "reboot", "exportfs -r"] {
                    assert!(!action.contains(mutating), "recommended a mutating command: {action}");
                }
            }
        }
    }

    #[test]
    fn a_healthy_cluster_produces_no_storage_diagnoses() {
        let world = two_domains().host_is_up("fs-a").host_is_up("fs-b");
        assert!(world.evaluate(&StorageServiceFailure).is_empty());
        assert!(world.evaluate(&SharedStorageFailure).is_empty());
        assert!(world.evaluate(&ClientLocalStorageFailure).is_empty());
    }

    #[test]
    fn the_rules_name_no_storage_technology() {
        // SPEC.md §78: swapping NFS for something else must not need changes
        // here. The diagnosis *names* are NFS-flavoured for continuity with the
        // specification, but the logic must not be.
        let implementation = include_str!("storage.rs")
            .split("#[cfg(test)]")
            .next()
            .expect("implementation");
        for technology in ["nfsd", "mount -t", "exportfs -o", "rpcbind", "showmount"] {
            assert!(
                !implementation.contains(technology),
                "storage rules must not depend on {technology}"
            );
        }
    }
}
