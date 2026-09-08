//! Parser tests against saved real `scontrol` output (IMPLEMENTATION.md §78).
//!
//! Hand-written test strings drift towards what the parser already handles.
//! These fixtures are the real output shape, including the awkward parts:
//! values containing spaces, `#` and `:`; free-form reasons; compressed
//! hostlists; and `(null)` / `N/A` placeholders.

use sentinel::entity::EntityType;
use sentinel::integrations::slurm::parser::{self, NodeBaseState, NodeStateFlag};
use sentinel::inventory::slurm::{snapshot_from_view, SlurmView};

fn fixture(name: &str) -> String {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("fixtures")
        .join(name);
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()))
}

fn nodes() -> Vec<parser::SlurmNode> {
    parser::parse_nodes(&fixture("slurm/show_nodes.txt"))
}

#[test]
fn every_node_in_the_fixture_parses() {
    let nodes = nodes();
    assert_eq!(nodes.len(), 4);
    let names: Vec<&str> = nodes.iter().map(|n| n.node_name.as_str()).collect();
    assert_eq!(names, ["compute-01", "compute-02", "compute-03", "compute-04"]);
}

#[test]
fn a_value_containing_spaces_and_punctuation_does_not_derail_the_line() {
    // `OS=Linux 5.15.0-91-generic #101-Ubuntu SMP Tue Nov 14 13:30:08 UTC 2023`
    // sits in the middle of the record.
    let node = &nodes()[0];
    assert!(node
        .raw
        .get("OS")
        .unwrap()
        .starts_with("Linux 5.15.0-91-generic #101-Ubuntu"));
    assert_eq!(node.cpu_total, Some(64), "fields after the spacey one still parse");
    assert_eq!(node.real_memory_mb, Some(257000));
}

#[test]
fn tres_fields_keep_their_internal_equals_signs() {
    let node = &nodes()[0];
    assert_eq!(
        node.cfg_tres.as_deref(),
        Some("cpu=64,mem=257000M,billing=64,gres/gpu=2")
    );
    assert_eq!(
        node.alloc_tres, None,
        "an empty AllocTRES is absent, not an empty string"
    );
}

#[test]
fn gpu_counts_survive_both_gres_spellings_used_in_the_fixture() {
    let nodes = nodes();
    assert_eq!(nodes[0].configured_gpu_count(), Some(2), "gpu:rtx3090:2");
    assert_eq!(nodes[1].configured_gpu_count(), Some(4), "gpu:a100:4(IDX:0-3)");
    assert_eq!(
        nodes[2].configured_gpu_count(),
        None,
        "Gres=(null) means no expectation"
    );
}

#[test]
fn the_four_states_in_the_fixture_are_told_apart() {
    let nodes = nodes();

    assert_eq!(nodes[0].state.base, NodeBaseState::Idle);
    assert!(nodes[0].state.is_schedulable());

    assert_eq!(nodes[1].state.base, NodeBaseState::Mixed);
    assert!(nodes[1].state.is_schedulable(), "partly allocated is still schedulable");

    assert_eq!(nodes[2].state.base, NodeBaseState::Idle);
    assert!(nodes[2].state.is_drained());
    assert!(
        !nodes[2].state.is_schedulable(),
        "healthy but drained is not schedulable"
    );

    assert_eq!(nodes[3].state.base, NodeBaseState::Down);
    assert!(nodes[3].state.has(&NodeStateFlag::NotResponding));
}

#[test]
fn free_form_reasons_are_captured_whole() {
    let nodes = nodes();
    assert_eq!(nodes[0].reason, None, "Reason=none means no reason");
    assert_eq!(
        nodes[2].reason.as_deref(),
        Some("scheduled maintenance [operator@2026-09-05T14:00:00]")
    );
    assert_eq!(
        nodes[3].reason.as_deref(),
        Some("Not responding [slurm@2026-09-07T03:15:22]")
    );
}

#[test]
fn partitions_parse_and_expand_their_compressed_hostlists() {
    let partitions = parser::parse_partitions(&fixture("slurm/show_partitions.txt"));
    assert_eq!(partitions.len(), 2);

    assert_eq!(partitions[0].name, "compute");
    assert!(partitions[0].is_default);
    assert_eq!(
        partitions[0].nodes,
        ["compute-01", "compute-02", "compute-03", "compute-04"]
    );

    assert_eq!(partitions[1].name, "debug");
    assert!(!partitions[1].is_default);
    assert_eq!(partitions[1].nodes, ["compute-02"]);
}

#[test]
fn a_node_in_two_partitions_reports_both() {
    assert_eq!(nodes()[1].partitions, ["compute", "debug"]);
}

#[test]
fn controller_ping_output_parses_in_both_shapes() {
    let single = parser::parse_ping(&fixture("slurm/ping_up.txt"));
    assert_eq!(single.len(), 1);
    assert!(single[0].up);

    let ha = parser::parse_ping(&fixture("slurm/ping_ha.txt"));
    assert_eq!(ha.len(), 2);
    assert!(ha[0].up && !ha[1].up);
}

#[test]
fn an_unreachable_controller_yields_no_false_reachability() {
    // A parse failure must not read as "the controller is up".
    assert!(parser::parse_ping(&fixture("slurm/ping_down.txt")).is_empty());
}

#[test]
fn the_whole_fixture_maps_into_a_coherent_inventory() {
    let view = SlurmView {
        nodes: nodes(),
        partitions: parser::parse_partitions(&fixture("slurm/show_partitions.txt")),
        controllers: parser::parse_ping(&fixture("slurm/ping_up.txt")),
    };
    let snapshot = snapshot_from_view("lab", "test-scheduler", &view);

    let count = |ty: EntityType| snapshot.entities.iter().filter(|e| e.entity_type == ty).count();
    assert_eq!(count(EntityType::Scheduler), 1);
    assert_eq!(count(EntityType::Host), 5, "four compute nodes and one controller host");
    assert_eq!(count(EntityType::Service), 5, "four slurmd and one slurmctld");

    // Every edge must point at an entity in the same snapshot.
    let ids: std::collections::BTreeSet<_> = snapshot.entities.iter().map(|e| e.id).collect();
    for edge in &snapshot.dependencies {
        assert!(ids.contains(&edge.source), "dangling edge source");
        assert!(ids.contains(&edge.target), "dangling edge target");
    }
}

#[test]
fn an_unknown_future_slurm_field_does_not_break_the_parse() {
    // IMPLEMENTATION.md §155: a version bump must not blind the parser.
    // Slurm emits Reason last, so a new field arrives before it.
    let line = fixture("slurm/show_nodes.txt")
        .lines()
        .next()
        .unwrap()
        .replace(" Reason=", " BrandNewSlurmField=surprising Another=1 Reason=");

    let node = parser::SlurmNode::parse_line(&line).expect("still parses");
    assert_eq!(node.node_name, "compute-01");
    assert_eq!(node.cpu_total, Some(64));
    assert_eq!(node.raw.get("BrandNewSlurmField").unwrap(), "surprising");
}
