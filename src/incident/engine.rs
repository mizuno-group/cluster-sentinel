//! Correlating diagnoses into incidents.
//!
//! An operator does not want five alerts about five nodes that share one
//! fileserver; they want one incident that names the fileserver. Equally, they
//! do not want two unrelated faults merged because they happened in the same
//! minute.
//!
//! The correlation principle here is **shared cause, not shared symptom**: two
//! diagnoses belong to the same incident when they blame the same thing. That
//! falls out of the dependency graph rather than from timing heuristics, and it
//! is why a shared-storage failure and the export-service failure behind it end
//! up as one incident while two independent node faults stay separate.
//!
//! Time proximity is used only to *reopen* a recently resolved incident rather
//! than to group unrelated ones (IMPLEMENTATION.md §74).

use std::collections::{BTreeMap, BTreeSet};

use crate::dependency::DependencyGraph;
use crate::diagnosis::{kind, Confidence, Diagnosis};
use crate::entity::EntityId;
use crate::state::EntityState;
use crate::time::now;

use super::{Incident, IncidentStatus, Severity, TimelineEvent};

/// How long after resolution a returning fault is treated as the same incident.
///
/// Long enough that a flapping service does not spawn a new incident every
/// cycle; short enough that a fault next week is genuinely new.
pub const REOPEN_WINDOW_MINUTES: i64 = 30;

/// What one reconciliation changed.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct IncidentUpdate {
    /// Incidents opened for the first time.
    pub opened: Vec<Incident>,
    /// Incidents that already existed and were refreshed.
    pub updated: Vec<Incident>,
    /// Incidents whose root recovered but whose dependents have not.
    pub recovering: Vec<Incident>,
    /// Incidents fully resolved.
    pub resolved: Vec<Incident>,
}

impl IncidentUpdate {
    /// Whether anything changed.
    pub fn is_empty(&self) -> bool {
        self.opened.is_empty() && self.updated.is_empty() && self.recovering.is_empty() && self.resolved.is_empty()
    }

    /// Every incident touched.
    pub fn all(&self) -> Vec<&Incident> {
        self.opened
            .iter()
            .chain(&self.updated)
            .chain(&self.recovering)
            .chain(&self.resolved)
            .collect()
    }
}

/// The fingerprint that decides which incident a diagnosis belongs to.
///
/// Derived from the suspected cause, so everything blaming one fileserver
/// becomes one incident whatever mix of symptoms it produced. A diagnosis with
/// no suspected cause falls back to what it affects, which at least keeps two
/// unrelated faults apart.
pub fn fingerprint(diagnosis: &Diagnosis) -> String {
    let roots: BTreeSet<String> = if diagnosis.suspected_root_entities.is_empty() {
        diagnosis.affected_entities.iter().map(|e| e.to_string()).collect()
    } else {
        diagnosis
            .suspected_root_entities
            .iter()
            .map(|e| e.to_string())
            .collect()
    };

    let scope = if diagnosis.suspected_root_entities.is_empty() {
        "affects"
    } else {
        "cause"
    };
    format!("{scope}:{}", roots.into_iter().collect::<Vec<_>>().join(","))
}

/// How serious a diagnosis is on its own, before fan-out is considered.
fn base_severity(diagnosis: &Diagnosis) -> Severity {
    match diagnosis.diagnosis_type.as_str() {
        // Something is unusable and nobody can reach it.
        kind::HOST_UNREACHABLE | kind::SHARED_STORAGE_FAILURE | kind::SLURM_CONTROL_PLANE_FAILURE => Severity::Critical,
        // A configuration disagreement is worth knowing about, but nothing is
        // broken and nobody needs waking.
        kind::RESOURCE_CONFIGURATION_MISMATCH | kind::GPU_CONFIGURATION_MISMATCH | kind::CLOCK_SKEW => Severity::Info,
        // A node the scheduler will not use is a degradation, not an outage.
        kind::SLURM_ONLY_DEGRADATION | kind::HOST_REBOOTED => Severity::Info,
        _ => Severity::Warning,
    }
}

/// Raise severity according to how much depends on the suspected cause.
///
/// SPEC.md §101: a fileserver with five dependent nodes matters more than one
/// compute node, and the graph already knows the difference.
pub fn severity_for(diagnosis: &Diagnosis, graph: &DependencyGraph) -> Severity {
    let mut severity = base_severity(diagnosis);

    // A low-confidence guess is not worth a critical alert whatever its
    // fan-out; it is worth showing to someone already looking.
    if diagnosis.confidence == Confidence::Low {
        return Severity::Info;
    }

    let fan_out: usize = diagnosis
        .suspected_root_entities
        .iter()
        .map(|root| graph.downstream(*root, None).len())
        .max()
        .unwrap_or(0);

    if fan_out >= 3 && severity < Severity::Critical {
        severity = Severity::Critical;
    } else if fan_out >= 1 && severity < Severity::Warning {
        severity = Severity::Warning;
    }

    severity
}

/// Correlates diagnoses into incidents and tracks their lifecycle.
#[derive(Debug, Default)]
pub struct IncidentEngine {
    incidents: BTreeMap<String, Incident>,
}

impl IncidentEngine {
    /// An engine with no incidents.
    pub fn new() -> Self {
        Self::default()
    }

    /// Load existing incidents, for resuming after a restart.
    pub fn seed(&mut self, incidents: impl IntoIterator<Item = Incident>) {
        for incident in incidents {
            self.incidents.insert(incident.fingerprint.clone(), incident);
        }
    }

    /// Every incident the engine knows about.
    pub fn incidents(&self) -> impl Iterator<Item = &Incident> {
        self.incidents.values()
    }

    /// Incidents that still need attention.
    pub fn active(&self) -> impl Iterator<Item = &Incident> {
        self.incidents.values().filter(|i| i.status.is_active())
    }

    /// An incident by fingerprint.
    pub fn get(&self, fingerprint: &str) -> Option<&Incident> {
        self.incidents.get(fingerprint)
    }

    /// How many incidents are held.
    pub fn len(&self) -> usize {
        self.incidents.len()
    }

    /// Whether no incident is held.
    pub fn is_empty(&self) -> bool {
        self.incidents.is_empty()
    }

    /// Fold the current diagnoses in, opening, updating and resolving.
    pub fn reconcile(
        &mut self,
        diagnoses: &[Diagnosis],
        graph: &DependencyGraph,
        states: &BTreeMap<EntityId, EntityState>,
    ) -> IncidentUpdate {
        let mut update = IncidentUpdate::default();
        let mut grouped: BTreeMap<String, Vec<&Diagnosis>> = BTreeMap::new();
        for diagnosis in diagnoses {
            grouped.entry(fingerprint(diagnosis)).or_default().push(diagnosis);
        }

        for (fingerprint, diagnoses) in &grouped {
            let severity = diagnoses
                .iter()
                .map(|d| severity_for(d, graph))
                .max()
                .unwrap_or(Severity::Warning);

            match self.incidents.get_mut(fingerprint) {
                // A returning fault within the window is the same incident,
                // not a new one; a flapping service must not produce a fresh
                // alert every cycle.
                Some(existing) if existing.status == IncidentStatus::Resolved && !within_reopen_window(existing) => {
                    let incident = open_incident(fingerprint, severity, diagnoses);
                    self.incidents.insert(fingerprint.clone(), incident.clone());
                    update.opened.push(incident);
                }
                Some(existing) => {
                    if existing.status == IncidentStatus::Resolved {
                        existing.status = IncidentStatus::Open;
                        existing.ended_at = None;
                        existing
                            .timeline
                            .push(TimelineEvent::new("reopened", "the fault returned"));
                    }
                    existing.escalate(severity);
                    refresh(existing, diagnoses);
                    update.updated.push(existing.clone());
                }
                None => {
                    let incident = open_incident(fingerprint, severity, diagnoses);
                    self.incidents.insert(fingerprint.clone(), incident.clone());
                    update.opened.push(incident);
                }
            }
        }

        // Anything no longer diagnosed may be recovering or resolved.
        for (fingerprint, incident) in self.incidents.iter_mut() {
            if grouped.contains_key(fingerprint) || !incident.status.is_active() {
                continue;
            }

            let unrecovered = unrecovered_entities(incident, states);

            if unrecovered.is_empty() {
                incident.resolve();
                update.resolved.push(incident.clone());
            } else if incident.status != IncidentStatus::Recovering {
                // IMPLEMENTATION.md §75: the root coming back is not the end of
                // the incident while things that depend on it are still broken.
                // Closing here would tell an operator it was over while people
                // still could not work.
                incident.status = IncidentStatus::Recovering;
                incident.timeline.push(TimelineEvent::new(
                    "recovering",
                    format!(
                        "the cause is no longer diagnosed; {} entity/entities still impaired",
                        unrecovered.len()
                    ),
                ));
                update.recovering.push(incident.clone());
            }
        }

        update
    }
}

/// Entities in an incident that are still not healthy.
fn unrecovered_entities(incident: &Incident, states: &BTreeMap<EntityId, EntityState>) -> Vec<EntityId> {
    incident
        .affected_entities
        .iter()
        .copied()
        .filter(|entity| {
            states
                .get(entity)
                .map(|state| state.overall.is_problem())
                // No state at all is not evidence of a problem.
                .unwrap_or(false)
        })
        .collect()
}

/// Whether a resolved incident is recent enough to reopen.
fn within_reopen_window(incident: &Incident) -> bool {
    let Some(ended_at) = incident.ended_at else {
        return true;
    };
    now() - ended_at < chrono::Duration::minutes(REOPEN_WINDOW_MINUTES)
}

/// Open a new incident from a group of diagnoses.
fn open_incident(fingerprint: &str, severity: Severity, diagnoses: &[&Diagnosis]) -> Incident {
    let mut incident = Incident::open(fingerprint, severity);
    incident
        .timeline
        .push(TimelineEvent::new("opened", format!("severity {severity}")));
    for diagnosis in diagnoses {
        incident.add_diagnosis((*diagnosis).clone());
    }
    incident
}

/// Refresh an existing incident with the current diagnoses.
///
/// Diagnoses are replaced rather than appended: an incident that accumulated a
/// diagnosis per cycle would grow without bound and bury the current picture
/// under a history of itself. The timeline keeps the history.
fn refresh(incident: &mut Incident, diagnoses: &[&Diagnosis]) {
    let previous: BTreeSet<String> = incident
        .diagnoses
        .iter()
        .map(|d| d.diagnosis_type.to_string())
        .collect();
    let current: BTreeSet<String> = diagnoses.iter().map(|d| d.diagnosis_type.to_string()).collect();

    if previous != current {
        incident.timeline.push(TimelineEvent::new(
            "diagnosis_changed",
            format!("{} -> {}", join(&previous), join(&current)),
        ));
    }

    incident.diagnoses.clear();
    for diagnosis in diagnoses {
        // Merged without a timeline entry: the timeline records what changed,
        // and `diagnosis_changed` above has already covered anything that did.
        incident.merge_diagnosis((*diagnosis).clone());
    }
}

fn join(values: &BTreeSet<String>) -> String {
    if values.is_empty() {
        "nothing".to_string()
    } else {
        values.iter().cloned().collect::<Vec<_>>().join(", ")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dependency::{DependencyEdge, DependencyType};
    use crate::diagnosis::Diagnosis;
    use crate::entity::{EntityKey, EntityType};
    use crate::state::{ComponentState, Health, StateComponent};

    fn host(name: &str) -> EntityId {
        EntityKey::new("lab", EntityType::Host, name).entity_id()
    }

    fn storage(name: &str) -> EntityId {
        EntityKey::new("lab", EntityType::Storage, name).entity_id()
    }

    fn diagnosis(diagnosis_type: &str, root: &str, affected: &[&str]) -> Diagnosis {
        Diagnosis::new(diagnosis_type, "test.rule", Confidence::High)
            .rooted_at([host(root)])
            .affecting(affected.iter().map(|n| host(n)).collect::<Vec<_>>())
            .with_evidence([crate::observation::ObservationId::new()])
    }

    /// A fileserver with three dependent clients.
    fn graph_with_fan_out() -> DependencyGraph {
        DependencyGraph::from_edges([
            DependencyEdge::new(host("c1"), storage("s1"), DependencyType::UsesStorage),
            DependencyEdge::new(host("c2"), storage("s1"), DependencyType::UsesStorage),
            DependencyEdge::new(host("c3"), storage("s1"), DependencyType::UsesStorage),
            DependencyEdge::new(storage("s1"), host("fs1"), DependencyType::Provides),
        ])
    }

    fn healthy(entities: &[&str]) -> BTreeMap<EntityId, EntityState> {
        entities
            .iter()
            .map(|name| {
                let id = host(name);
                let mut state = EntityState::unknown(id);
                state.set_component(StateComponent::Network, ComponentState::new(Health::Healthy));
                (id, state)
            })
            .collect()
    }

    fn impaired(entities: &[&str]) -> BTreeMap<EntityId, EntityState> {
        entities
            .iter()
            .map(|name| {
                let id = host(name);
                let mut state = EntityState::unknown(id);
                state.set_component(StateComponent::Storage, ComponentState::new(Health::Unavailable));
                (id, state)
            })
            .collect()
    }

    #[test]
    fn diagnoses_blaming_the_same_cause_become_one_incident() {
        // Five alerts about five nodes behind one fileserver is what this
        // prevents.
        let mut engine = IncidentEngine::new();
        let diagnoses = vec![
            diagnosis(kind::NFS_SERVICE_FAILURE, "fs1", &["fs1"]),
            diagnosis(kind::SHARED_STORAGE_FAILURE, "fs1", &["c1", "c2", "c3"]),
        ];

        let update = engine.reconcile(&diagnoses, &graph_with_fan_out(), &BTreeMap::new());

        assert_eq!(update.opened.len(), 1, "one cause, one incident");
        assert_eq!(engine.len(), 1);
        let incident = &update.opened[0];
        assert_eq!(incident.diagnoses.len(), 2, "both symptoms are recorded");
        assert!(incident.has_diagnosis(kind::SHARED_STORAGE_FAILURE));
        assert!(incident.has_diagnosis(kind::NFS_SERVICE_FAILURE));
    }

    #[test]
    fn unrelated_faults_stay_separate() {
        // Two things breaking in the same minute is not one incident.
        let mut engine = IncidentEngine::new();
        let diagnoses = vec![
            diagnosis(kind::SSH_SERVICE_FAILURE, "a", &["a"]),
            diagnosis(kind::SSH_SERVICE_FAILURE, "b", &["b"]),
        ];

        let update = engine.reconcile(&diagnoses, &DependencyGraph::new(), &BTreeMap::new());
        assert_eq!(update.opened.len(), 2);
    }

    #[test]
    fn a_repeated_diagnosis_updates_rather_than_duplicates() {
        let mut engine = IncidentEngine::new();
        let diagnoses = vec![diagnosis(kind::SSH_SERVICE_FAILURE, "a", &["a"])];

        engine.reconcile(&diagnoses, &DependencyGraph::new(), &BTreeMap::new());
        let second = engine.reconcile(&diagnoses, &DependencyGraph::new(), &BTreeMap::new());

        assert!(second.opened.is_empty());
        assert_eq!(second.updated.len(), 1);
        assert_eq!(engine.len(), 1, "one incident, not two");
    }

    #[test]
    fn an_incident_does_not_accumulate_a_diagnosis_per_cycle() {
        // Otherwise the current picture is buried under a history of itself.
        let mut engine = IncidentEngine::new();
        let diagnoses = vec![diagnosis(kind::SSH_SERVICE_FAILURE, "a", &["a"])];

        for _ in 0..5 {
            engine.reconcile(&diagnoses, &DependencyGraph::new(), &BTreeMap::new());
        }

        let incident = engine.incidents().next().expect("incident");
        assert_eq!(incident.diagnoses.len(), 1);
        assert!(incident.timeline.len() > 1, "but the history survives on the timeline");
    }

    #[test]
    fn fan_out_raises_severity() {
        // SPEC.md §101: a fileserver with three dependents matters more than a
        // single node.
        let graph = graph_with_fan_out();

        let wide = diagnosis(kind::NFS_SERVICE_FAILURE, "fs1", &["fs1"]);
        assert_eq!(severity_for(&wide, &graph), Severity::Critical);

        let narrow = diagnosis(kind::SSH_SERVICE_FAILURE, "lonely", &["lonely"]);
        assert_eq!(severity_for(&narrow, &graph), Severity::Warning);
    }

    #[test]
    fn an_administrative_state_is_not_an_outage() {
        // A drained node is information, not an emergency.
        let drained = diagnosis(kind::SLURM_ONLY_DEGRADATION, "a", &["a"]);
        assert_eq!(severity_for(&drained, &DependencyGraph::new()), Severity::Info);
    }

    #[test]
    fn a_configuration_mismatch_is_informational() {
        let mismatch = diagnosis(kind::GPU_CONFIGURATION_MISMATCH, "a", &["a"]);
        assert_eq!(severity_for(&mismatch, &DependencyGraph::new()), Severity::Info);
    }

    #[test]
    fn a_low_confidence_guess_never_becomes_critical() {
        // Whatever its fan-out, a hypothesis is not worth waking someone for.
        let guess = Diagnosis::new(kind::SHARED_STORAGE_FAILURE, "test.rule", Confidence::Low)
            .rooted_at([host("fs1")])
            .affecting([host("c1"), host("c2"), host("c3")]);
        assert_eq!(severity_for(&guess, &graph_with_fan_out()), Severity::Info);
    }

    #[test]
    fn severity_escalates_but_never_silently_drops() {
        let mut engine = IncidentEngine::new();
        let graph = graph_with_fan_out();

        engine.reconcile(
            &[diagnosis(kind::SSH_SERVICE_FAILURE, "fs1", &["fs1"])],
            &graph,
            &BTreeMap::new(),
        );
        let before = engine.incidents().next().expect("incident").severity;

        engine.reconcile(
            &[diagnosis(kind::NFS_SERVICE_FAILURE, "fs1", &["fs1"])],
            &graph,
            &BTreeMap::new(),
        );
        let after = engine.incidents().next().expect("incident").severity;

        assert!(after >= before);
    }

    #[test]
    fn an_incident_resolves_when_the_fault_and_its_effects_are_gone() {
        let mut engine = IncidentEngine::new();
        let diagnoses = vec![diagnosis(kind::SHARED_STORAGE_FAILURE, "fs1", &["c1", "c2"])];
        engine.reconcile(&diagnoses, &graph_with_fan_out(), &impaired(&["c1", "c2"]));

        let update = engine.reconcile(&[], &graph_with_fan_out(), &healthy(&["c1", "c2"]));

        assert_eq!(update.resolved.len(), 1);
        assert_eq!(update.resolved[0].status, IncidentStatus::Resolved);
        assert!(update.resolved[0].ended_at.is_some());
        assert_eq!(engine.active().count(), 0);
    }

    #[test]
    fn an_incident_does_not_resolve_while_dependents_are_still_broken() {
        // IMPLEMENTATION.md §75. Closing here would tell an operator it was
        // over while people still could not work.
        let mut engine = IncidentEngine::new();
        let diagnoses = vec![diagnosis(kind::SHARED_STORAGE_FAILURE, "fs1", &["c1", "c2"])];
        engine.reconcile(&diagnoses, &graph_with_fan_out(), &impaired(&["c1", "c2"]));

        // The cause is no longer diagnosed, but one client has not recovered.
        let mut states = healthy(&["c1"]);
        states.extend(impaired(&["c2"]));
        let update = engine.reconcile(&[], &graph_with_fan_out(), &states);

        assert!(update.resolved.is_empty());
        assert_eq!(update.recovering.len(), 1);
        assert_eq!(update.recovering[0].status, IncidentStatus::Recovering);
        assert_eq!(engine.active().count(), 1, "still active");
    }

    #[test]
    fn a_recovering_incident_resolves_once_everything_comes_back() {
        let mut engine = IncidentEngine::new();
        let diagnoses = vec![diagnosis(kind::SHARED_STORAGE_FAILURE, "fs1", &["c1", "c2"])];
        engine.reconcile(&diagnoses, &graph_with_fan_out(), &impaired(&["c1", "c2"]));

        let mut states = healthy(&["c1"]);
        states.extend(impaired(&["c2"]));
        engine.reconcile(&[], &graph_with_fan_out(), &states);

        let update = engine.reconcile(&[], &graph_with_fan_out(), &healthy(&["c1", "c2"]));
        assert_eq!(update.resolved.len(), 1);
    }

    #[test]
    fn a_returning_fault_reopens_rather_than_spawning_a_new_incident() {
        // A flapping service must not produce a fresh alert every cycle.
        let mut engine = IncidentEngine::new();
        let diagnoses = vec![diagnosis(kind::SSH_SERVICE_FAILURE, "a", &["a"])];

        engine.reconcile(&diagnoses, &DependencyGraph::new(), &BTreeMap::new());
        engine.reconcile(&[], &DependencyGraph::new(), &healthy(&["a"]));
        let update = engine.reconcile(&diagnoses, &DependencyGraph::new(), &BTreeMap::new());

        assert!(update.opened.is_empty(), "not a new incident");
        assert_eq!(update.updated.len(), 1);
        assert_eq!(engine.len(), 1);

        let incident = engine.incidents().next().expect("incident");
        assert_eq!(incident.status, IncidentStatus::Open);
        assert!(incident.timeline.iter().any(|e| e.kind == "reopened"));
    }

    #[test]
    fn a_fault_returning_long_afterwards_is_a_new_incident() {
        let mut engine = IncidentEngine::new();
        let diagnoses = vec![diagnosis(kind::SSH_SERVICE_FAILURE, "a", &["a"])];

        engine.reconcile(&diagnoses, &DependencyGraph::new(), &BTreeMap::new());
        engine.reconcile(&[], &DependencyGraph::new(), &healthy(&["a"]));

        // Age the resolution beyond the window.
        let fingerprint = engine.incidents().next().expect("incident").fingerprint.clone();
        engine.incidents.get_mut(&fingerprint).expect("incident").ended_at = Some(now() - chrono::Duration::hours(4));

        let update = engine.reconcile(&diagnoses, &DependencyGraph::new(), &BTreeMap::new());
        assert_eq!(update.opened.len(), 1, "a fault next week is genuinely new");
    }

    #[test]
    fn an_incident_preserves_the_evidence_of_every_diagnosis_in_it() {
        // SPEC.md §103: the evidence has to outlive the moment.
        let mut engine = IncidentEngine::new();
        let first = diagnosis(kind::NFS_SERVICE_FAILURE, "fs1", &["fs1"]);
        let second = diagnosis(kind::SHARED_STORAGE_FAILURE, "fs1", &["c1"]);
        let expected: BTreeSet<_> = first.evidence.iter().chain(&second.evidence).copied().collect();

        engine.reconcile(&[first, second], &graph_with_fan_out(), &BTreeMap::new());

        let incident = engine.incidents().next().expect("incident");
        let held: BTreeSet<_> = incident.evidence.iter().copied().collect();
        assert_eq!(held, expected);
    }

    #[test]
    fn seeded_incidents_are_resumed_rather_than_reopened() {
        // A controller restart must not re-alert on everything already open.
        let mut engine = IncidentEngine::new();
        let diagnoses = vec![diagnosis(kind::SSH_SERVICE_FAILURE, "a", &["a"])];
        engine.reconcile(&diagnoses, &DependencyGraph::new(), &BTreeMap::new());
        let existing: Vec<Incident> = engine.incidents().cloned().collect();

        let mut restarted = IncidentEngine::new();
        restarted.seed(existing);
        let update = restarted.reconcile(&diagnoses, &DependencyGraph::new(), &BTreeMap::new());

        assert!(update.opened.is_empty(), "an already-open incident must not re-alert");
        assert_eq!(update.updated.len(), 1);
    }

    #[test]
    fn an_empty_reconciliation_of_an_empty_engine_changes_nothing() {
        let mut engine = IncidentEngine::new();
        assert!(engine
            .reconcile(&[], &DependencyGraph::new(), &BTreeMap::new())
            .is_empty());
    }

    #[test]
    fn a_diagnosis_with_no_suspected_cause_is_still_grouped_sensibly() {
        let mut engine = IncidentEngine::new();
        let orphan = Diagnosis::new(kind::CLOCK_SKEW, "test.rule", Confidence::Medium).affecting([host("a")]);
        let other = Diagnosis::new(kind::CLOCK_SKEW, "test.rule", Confidence::Medium).affecting([host("b")]);

        let update = engine.reconcile(&[orphan, other], &DependencyGraph::new(), &BTreeMap::new());
        assert_eq!(update.opened.len(), 2, "different entities, different incidents");
    }
}
