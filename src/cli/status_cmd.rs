//! `sentinel status` and `sentinel entity`.
//!
//! The point of these commands is to answer "what is wrong, and what does
//! Sentinel actually know" — not to render a dashboard. Every health verdict
//! shown here can be followed down to the observations behind it.

use std::collections::BTreeMap;

use serde::Serialize;

use crate::entity::{EntityId, EntityType, LifecycleState, ManagedEntity};
use crate::incident::Incident;
use crate::inventory::Inventory;
use crate::persistence::SqliteStore;
use crate::state::{EntityState, Health};

/// One entity as `status` presents it.
#[derive(Debug, Clone, Serialize)]
pub struct EntityStatus {
    /// Entity id.
    pub id: String,
    /// Entity type.
    pub entity_type: String,
    /// Canonical name.
    pub name: String,
    /// Display name.
    pub display_name: String,
    /// Rolled-up health.
    pub health: String,
    /// Machine-readable refinements.
    pub classifications: Vec<String>,
    /// Per-component health, excluding components that do not apply.
    pub components: BTreeMap<String, String>,
    /// Where remote probes will reach this entity, and how that was decided.
    ///
    /// The single most useful thing when an entity is unreachable and looks
    /// like it should not be: a host that registered no address is probed by
    /// name, and a name that resolves slowly fails the probes with short
    /// timeouts while leaving the longer ones passing.
    pub endpoint: Option<String>,
    /// What the host reported about itself: CPUs, memory, GPUs.
    ///
    /// Shown because it is one half of every comparison against the
    /// scheduler's configuration, and without it "Slurm expects 1 GPU, the
    /// host reports 0" cannot be checked against what the host actually said.
    /// `null` for a count means not stated, which is different from zero.
    pub hardware: Option<serde_json::Value>,
    /// Inventory lifecycle.
    pub lifecycle: String,
    /// Capabilities in force.
    pub capabilities: Vec<String>,
}

/// One open incident, as `status` presents it.
///
/// `status` used to show entity health and nothing else, which meant a whole
/// class of fault was invisible here: a network path that is broken between
/// two specific hosts belongs to no single entity, so nothing turns red and
/// the summary line says everything is healthy while an open CRITICAL sits in
/// `sentinel incident list`. Health answers "is this thing working"; an
/// incident answers "is something wrong", and they are not the same question.
#[derive(Debug, Clone, Serialize)]
pub struct IncidentSummary {
    /// Incident id, for `sentinel incident show`.
    pub id: String,
    /// How serious it is.
    pub severity: String,
    /// Open, recovering or resolved.
    pub status: String,
    /// What it says, in one line.
    pub summary: String,
    /// Names of the entities suspected of causing it.
    pub suspected_root_entities: Vec<String>,
}

/// The whole environment, as `status` presents it.
#[derive(Debug, Clone, Serialize)]
pub struct StatusReport {
    /// Environment name.
    pub environment: String,
    /// Entities, grouped by type.
    pub entities: Vec<EntityStatus>,
    /// Incidents that are still open.
    #[serde(default)]
    pub incidents: Vec<IncidentSummary>,
    /// Count per health value.
    pub totals: BTreeMap<String, usize>,
}

impl StatusReport {
    /// Build a report from an inventory and the derived states.
    pub fn build(
        environment: &str,
        inventory: &Inventory,
        states: &BTreeMap<crate::entity::EntityId, EntityState>,
    ) -> Self {
        let mut entities: Vec<EntityStatus> = inventory.entities().map(|e| describe(e, states.get(&e.id))).collect();

        // Group by type, then by name: an operator scanning the list wants
        // infrastructure and compute in predictable places.
        entities.sort_by(|a, b| {
            type_order(&a.entity_type)
                .cmp(&type_order(&b.entity_type))
                .then(a.name.cmp(&b.name))
        });

        let mut totals: BTreeMap<String, usize> = BTreeMap::new();
        for entity in &entities {
            *totals.entry(entity.health.clone()).or_default() += 1;
        }

        Self {
            environment: environment.to_string(),
            entities,
            incidents: Vec::new(),
            totals,
        }
    }

    /// Builder: attach the incidents that are still open.
    pub fn with_incidents(mut self, incidents: &[Incident], inventory: &Inventory) -> Self {
        let name_of = |id: EntityId| {
            inventory
                .get(id)
                .map(|e| e.canonical_name.clone())
                .unwrap_or_else(|| id.to_string())
        };

        self.incidents = incidents
            .iter()
            .filter(|incident| incident.status.is_active())
            .map(|incident| IncidentSummary {
                id: incident.id.to_string(),
                severity: incident.severity.to_string(),
                status: incident.status.to_string(),
                summary: incident
                    .primary_diagnosis()
                    .map(|d| d.summary.clone())
                    .unwrap_or_else(|| incident.fingerprint.clone()),
                suspected_root_entities: incident.suspected_root_entities.iter().copied().map(name_of).collect(),
            })
            .collect();
        self
    }

    /// Entities that an operator should look at.
    pub fn problems(&self) -> impl Iterator<Item = &EntityStatus> {
        self.entities
            .iter()
            .filter(|e| Health::parse(&e.health).is_some_and(|h| h.is_problem()))
    }

    /// Whether anything is wrong.
    ///
    /// An open incident counts even when every entity reads healthy: that
    /// combination is not a contradiction but a fault that belongs to a path
    /// or a relationship rather than to a machine, and exiting zero on it
    /// hides exactly the findings this system exists to make.
    pub fn is_healthy(&self) -> bool {
        self.problems().count() == 0 && self.incidents.is_empty()
    }
}

fn describe(entity: &ManagedEntity, state: Option<&EntityState>) -> EntityStatus {
    let health = state.map(|s| s.overall).unwrap_or(Health::Unknown);
    EntityStatus {
        id: entity.id.to_string(),
        entity_type: entity.entity_type.as_str().to_string(),
        name: entity.canonical_name.clone(),
        display_name: entity.display_name.clone(),
        health: health.as_str().to_string(),
        classifications: state
            .map(|s| s.classifications.iter().map(|c| c.as_str().to_string()).collect())
            .unwrap_or_default(),
        endpoint: crate::controller::endpoint_for(entity).map(|e| {
            let address = e.address.clone();
            let source = if entity
                .metadata
                .get("host")
                .and_then(|h| h.get("addresses"))
                .and_then(|a| a.as_array())
                .is_some_and(|a| !a.is_empty())
            {
                "reported by its agent"
            } else {
                "this entity's name — no address was registered, so it is resolved on every probe"
            };
            format!("{address} ({source})")
        }),
        components: state
            .map(|s| {
                s.components
                    .iter()
                    .filter(|(_, c)| c.health != Health::NotApplicable)
                    .map(|(component, c)| (component.as_str().to_string(), c.health.as_str().to_string()))
                    .collect()
            })
            .unwrap_or_default(),
        hardware: entity.metadata.get("hardware").filter(|v| !v.is_null()).cloned(),
        lifecycle: entity.lifecycle_state.as_str().to_string(),
        capabilities: entity.capabilities.iter().map(|c| c.as_str().to_string()).collect(),
    }
}

/// Ordering of entity types in the human-readable output.
fn type_order(entity_type: &str) -> u8 {
    match EntityType::parse(entity_type) {
        Some(EntityType::Scheduler) => 0,
        Some(EntityType::Storage) => 1,
        Some(EntityType::Host) => 2,
        Some(EntityType::Service) => 3,
        Some(EntityType::ExternalDependency) => 4,
        None => 5,
    }
}

/// Load the current picture from the database.
pub async fn load_report(store: &SqliteStore, environment: &str) -> anyhow::Result<StatusReport> {
    let inventory = store.load_inventory(environment).await?;
    let states = store.load_entity_states(environment).await?;
    let incidents = store.load_active_incidents(environment).await?;
    Ok(StatusReport::build(environment, &inventory, &states).with_incidents(&incidents, &inventory))
}

/// Render a status report as text.
pub fn render(report: &StatusReport) -> String {
    let mut out = String::new();
    out.push_str(&format!("ENVIRONMENT: {}\n", report.environment));

    if report.entities.is_empty() {
        // An empty board is the normal state of a controller that has just
        // been installed, and saying only "nothing here" leaves an operator
        // with no idea which of the two sources is missing.
        out.push_str(
            "\nNo entities known yet.\n\n\
             Two things put entities here:\n\n\
            \x20 * Slurm discovery. Check that the configuration has\n\
            \x20     [discovery.slurm]\n\
            \x20     enabled = true\n\
            \x20   then run: sentinel discover\n\n\
            \x20 * Agents registering. Deploy them (docs/DEPLOYMENT.md section 6),\n\
            \x20   or declare hosts that will not run one with [[entities]].\n",
        );
        return out;
    }

    out.push_str(&render_incidents(&report.incidents));

    let mut current_type = String::new();
    for entity in &report.entities {
        if entity.entity_type != current_type {
            current_type.clone_from(&entity.entity_type);
            out.push_str(&format!("\n{}\n", heading(&current_type)));
            out.push_str(&"\u{2500}".repeat(60));
            out.push('\n');
        }

        let width = report
            .entities
            .iter()
            .map(|e| e.name.len())
            .max()
            .unwrap_or(12)
            .clamp(12, 32);
        out.push_str(&format!(
            "{:<width$}  {}",
            entity.name,
            entity.health.to_uppercase(),
            width = width
        ));

        if !entity.classifications.is_empty() {
            out.push_str(&format!("  [{}]", entity.classifications.join(", ")));
        }
        if entity.lifecycle != LifecycleState::Active.as_str() {
            out.push_str(&format!("  ({})", entity.lifecycle));
        }
        out.push('\n');
    }

    out.push('\n');
    let summary: Vec<String> = report
        .totals
        .iter()
        .map(|(health, count)| format!("{count} {health}"))
        .collect();
    out.push_str(&format!("{}\n", summary.join(", ")));
    out
}

/// The open incidents, above the entity list because they are the answer to
/// the question the operator actually asked.
fn render_incidents(incidents: &[IncidentSummary]) -> String {
    if incidents.is_empty() {
        return String::new();
    }

    let mut out = String::from("\nOpen incidents\n");
    out.push_str(&"\u{2500}".repeat(60));
    out.push('\n');

    for incident in incidents {
        out.push_str(&format!(
            "{} [{}]  {}\n",
            incident.severity.to_uppercase(),
            incident.status,
            incident.summary
        ));
        if !incident.suspected_root_entities.is_empty() {
            out.push_str(&format!(
                "  suspected cause: {}\n",
                incident.suspected_root_entities.join(", ")
            ));
        }
        out.push_str(&format!("  sentinel incident show {}\n", incident.id));
    }

    out
}

fn heading(entity_type: &str) -> String {
    match EntityType::parse(entity_type) {
        Some(EntityType::Scheduler) => "Schedulers".into(),
        Some(EntityType::Storage) => "Storage".into(),
        Some(EntityType::Host) => "Hosts".into(),
        Some(EntityType::Service) => "Services".into(),
        Some(EntityType::ExternalDependency) => "External dependencies".into(),
        None => entity_type.to_string(),
    }
}

/// Render one entity in detail.
pub fn render_entity(entity: &EntityStatus) -> String {
    let mut out = String::new();
    out.push_str(&format!("Entity:\n{}({})\n\n", entity.entity_type, entity.name));

    if entity.display_name != entity.name {
        out.push_str(&format!("Display name:\n{}\n\n", entity.display_name));
    }

    out.push_str("Capabilities:\n");
    if entity.capabilities.is_empty() {
        out.push_str("(none)\n");
    } else {
        for capability in &entity.capabilities {
            out.push_str(&format!("{capability}\n"));
        }
    }

    if let Some(hardware) = &entity.hardware {
        out.push_str("\nReported hardware:\n");
        match hardware.as_object() {
            Some(fields) => {
                for (name, value) in fields {
                    let shown = if value.is_null() {
                        "(not stated)".to_string()
                    } else {
                        value.to_string()
                    };
                    out.push_str(&format!("{name}: {shown}\n"));
                }
            }
            None => out.push_str(&format!("{hardware}\n")),
        }
    }

    out.push_str("\nProbed at:\n");
    match &entity.endpoint {
        Some(endpoint) => out.push_str(&format!("{endpoint}\n")),
        None => out.push_str("(nowhere — no address and no usable name)\n"),
    }

    out.push_str(&format!("\nOverall:\n{}\n", entity.health.to_uppercase()));

    if !entity.classifications.is_empty() {
        out.push_str(&format!("\nClassification:\n{}\n", entity.classifications.join("\n")));
    }

    if !entity.components.is_empty() {
        out.push_str("\nComponents:\n");
        for (component, health) in &entity.components {
            out.push_str(&format!("{component:<14} {}\n", health.to_uppercase()));
        }
    }

    out.push_str(
        "\nHow these are decided and what each probe runs:\n  sentinel explain capabilities\n  sentinel explain paths\n",
    );
    out.push_str(&format!("\nLifecycle:\n{}\n", entity.lifecycle));
    out.push_str(&format!("\nID:\n{}\n", entity.id));
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::capability::CapabilitySet;
    use crate::entity::EntityId;
    use crate::state::{classification, ComponentState, StateComponent};

    fn inventory_with(entities: Vec<ManagedEntity>) -> Inventory {
        let mut inventory = Inventory::new();
        for entity in entities {
            inventory.insert_entity(entity);
        }
        inventory
    }

    fn state_of(entity: EntityId, component: StateComponent, health: Health) -> EntityState {
        let mut state = EntityState::unknown(entity);
        state.set_component(component, ComponentState::new(health));
        state
    }

    #[test]
    fn an_empty_board_says_what_would_fill_it() {
        // The normal state of a freshly installed controller. "Nothing here"
        // alone leaves an operator with no idea which source is missing.
        let report = StatusReport::build("lab", &Inventory::new(), &BTreeMap::new());
        let text = render(&report);

        assert!(text.contains("discovery.slurm"), "{text}");
        assert!(text.contains("sentinel discover"), "{text}");
        assert!(text.contains("Agents registering"), "{text}");
    }

    #[test]
    fn an_entity_with_no_observations_reads_as_unknown_not_healthy() {
        // Never having looked is not the same as having looked and found
        // nothing wrong.
        let entity = ManagedEntity::new("lab", EntityType::Host, "node-a");
        let report = StatusReport::build("lab", &inventory_with(vec![entity]), &BTreeMap::new());

        assert_eq!(report.entities[0].health, "unknown");
        assert!(
            report.is_healthy(),
            "unknown is not a problem to escalate, but it is not health either"
        );
    }

    #[test]
    fn health_comes_from_the_derived_state() {
        let entity = ManagedEntity::new("lab", EntityType::Host, "node-a");
        let id = entity.id;
        let mut states = BTreeMap::new();
        states.insert(id, state_of(id, StateComponent::Scheduler, Health::Degraded));

        let report = StatusReport::build("lab", &inventory_with(vec![entity]), &states);
        assert_eq!(report.entities[0].health, "degraded");
        assert_eq!(report.entities[0].components.get("scheduler").unwrap(), "degraded");
        assert!(!report.is_healthy());
        assert_eq!(report.problems().count(), 1);
    }

    #[test]
    fn inapplicable_components_are_not_shown() {
        // A node without GPUs should not have an accelerator row.
        let entity = ManagedEntity::new("lab", EntityType::Host, "node-a");
        let id = entity.id;
        let mut state = state_of(id, StateComponent::Scheduler, Health::Healthy);
        state.set_component(StateComponent::Accelerator, ComponentState::new(Health::NotApplicable));

        let report = StatusReport::build("lab", &inventory_with(vec![entity]), &BTreeMap::from([(id, state)]));
        assert!(report.entities[0].components.contains_key("scheduler"));
        assert!(!report.entities[0].components.contains_key("accelerator"));
    }

    #[test]
    fn entities_are_grouped_by_type_with_infrastructure_first() {
        let report = StatusReport::build(
            "lab",
            &inventory_with(vec![
                ManagedEntity::new("lab", EntityType::Host, "node-a"),
                ManagedEntity::new("lab", EntityType::Scheduler, "sched"),
                ManagedEntity::new("lab", EntityType::Storage, "shared"),
                ManagedEntity::new("lab", EntityType::Service, "slurmd@node-a"),
            ]),
            &BTreeMap::new(),
        );

        let order: Vec<&str> = report.entities.iter().map(|e| e.entity_type.as_str()).collect();
        assert_eq!(order, ["scheduler", "storage", "host", "service"]);
    }

    #[test]
    fn entities_of_one_type_are_sorted_by_name() {
        let report = StatusReport::build(
            "lab",
            &inventory_with(vec![
                ManagedEntity::new("lab", EntityType::Host, "node-c"),
                ManagedEntity::new("lab", EntityType::Host, "node-a"),
                ManagedEntity::new("lab", EntityType::Host, "node-b"),
            ]),
            &BTreeMap::new(),
        );
        let names: Vec<&str> = report.entities.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, ["node-a", "node-b", "node-c"]);
    }

    #[test]
    fn the_rendered_output_names_the_environment_and_every_entity() {
        let entity = ManagedEntity::new("lab", EntityType::Host, "node-a");
        let id = entity.id;
        let mut state = state_of(id, StateComponent::Scheduler, Health::Degraded);
        state.classify(classification::SCHEDULER_DEGRADED);

        let report = StatusReport::build("lab", &inventory_with(vec![entity]), &BTreeMap::from([(id, state)]));
        let text = render(&report);

        assert!(text.contains("ENVIRONMENT: lab"));
        assert!(text.contains("node-a"));
        assert!(text.contains("DEGRADED"));
        assert!(
            text.contains(classification::SCHEDULER_DEGRADED),
            "the classification must be visible"
        );
    }

    #[test]
    fn an_empty_environment_says_so_instead_of_printing_nothing() {
        let text = render(&StatusReport::build("lab", &Inventory::new(), &BTreeMap::new()));
        assert!(text.contains("No entities known yet"));
    }

    #[test]
    fn a_stale_entity_is_still_listed_and_marked() {
        let mut entity = ManagedEntity::new("lab", EntityType::Host, "node-a");
        entity.lifecycle_state = LifecycleState::Stale;

        let report = StatusReport::build("lab", &inventory_with(vec![entity]), &BTreeMap::new());
        assert_eq!(report.entities.len(), 1);
        assert!(
            render(&report).contains("(stale)"),
            "an operator must see that this is no longer reported"
        );
    }

    #[test]
    fn entity_detail_says_where_probes_will_go_and_why() {
        // The question an unreachable-but-apparently-fine host raises. A host
        // that registered no address is probed by name, resolved on every
        // attempt, and that difference decides whether a three-second probe
        // finishes while a five-second one does.
        let named = ManagedEntity::new("lab", EntityType::Host, "node01");
        let mut reported = ManagedEntity::new("lab", EntityType::Host, "node02");
        reported.metadata = serde_json::json!({ "host": { "addresses": ["192.0.2.20"] } });

        let inventory = inventory_with(vec![named, reported]);
        let report = StatusReport::build("lab", &inventory, &BTreeMap::new());

        let by_name = render_entity(report.entities.iter().find(|e| e.name == "node01").unwrap());
        assert!(by_name.contains("no address was registered"), "{by_name}");

        let by_address = render_entity(report.entities.iter().find(|e| e.name == "node02").unwrap());
        assert!(by_address.contains("192.0.2.20"), "{by_address}");
        assert!(by_address.contains("reported by its agent"), "{by_address}");
    }

    #[test]
    fn entity_detail_shows_capabilities_and_components() {
        let entity = ManagedEntity::new("lab", EntityType::Host, "node-a")
            .with_capabilities(CapabilitySet::from_iter(["slurm.compute", "gpu.nvidia"]));
        let id = entity.id;
        let mut state = state_of(id, StateComponent::Scheduler, Health::Degraded);
        state.classify(classification::SCHEDULER_DEGRADED);

        let report = StatusReport::build("lab", &inventory_with(vec![entity]), &BTreeMap::from([(id, state)]));
        let text = render_entity(&report.entities[0]);

        assert!(text.contains("host(node-a)"));
        assert!(text.contains("slurm.compute"));
        assert!(text.contains("gpu.nvidia"));
        assert!(text.contains("DEGRADED"));
        assert!(text.contains(classification::SCHEDULER_DEGRADED));
        assert!(text.contains(&id.to_string()));
    }

    #[test]
    fn totals_count_every_entity_once() {
        let report = StatusReport::build(
            "lab",
            &inventory_with(vec![
                ManagedEntity::new("lab", EntityType::Host, "a"),
                ManagedEntity::new("lab", EntityType::Host, "b"),
            ]),
            &BTreeMap::new(),
        );
        assert_eq!(report.totals.get("unknown"), Some(&2));
        assert_eq!(report.totals.values().sum::<usize>(), report.entities.len());
    }
}
