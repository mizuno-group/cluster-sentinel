//! M7 acceptance: GPUs are inventoried, and a configuration disagreement is
//! reported without calling the node broken.
//!
//! Containers have no GPUs, so the probe path is exercised with recorded
//! `nvidia-smi` output and the mismatch rule with synthetic hardware. Real GPU
//! validation is a level 4 concern (`docs/DEVELOPMENT.md`); nothing here
//! pretends otherwise.

use std::collections::HashMap;

use sentinel::capability::CapabilitySet;
use sentinel::diagnosis::{builtin_rules, kind, DiagnosisContext, ObservationIndex};
use sentinel::entity::{EntityId, EntityKey, EntityType, ManagedEntity};
use sentinel::integrations::slurm::observe::PROBE_NODE;
use sentinel::inventory::Inventory;
use sentinel::observation::{Observation, ProbeStatus};
use sentinel::probes::gpu::{parse_query, NvidiaGpuProbe};
use sentinel::probes::{Probe, ProbeContext, ProbeId};
use sentinel::state::EntityState;

/// Recorded output from `nvidia-smi --query-gpu=... --format=csv,noheader,nounits`.
const FOUR_A100S: &str = "\
0, GPU-aaaaaaaa-0000-0000-0000-000000000000, NVIDIA A100-SXM4-40GB, 38, 40960, 0, 0, 65.32
1, GPU-bbbbbbbb-0000-0000-0000-000000000000, NVIDIA A100-SXM4-40GB, 41, 40960, 20480, 78, 310.11
2, GPU-cccccccc-0000-0000-0000-000000000000, NVIDIA A100-SXM4-40GB, 39, 40960, 0, 0, 64.87
3, GPU-dddddddd-0000-0000-0000-000000000000, NVIDIA A100-SXM4-40GB, 40, 40960, 0, 0, 66.02
";

fn host_id(name: &str) -> EntityId {
    EntityKey::new("lab", EntityType::Host, name).entity_id()
}

/// Diagnose a host whose Slurm view and reported hardware are given.
fn diagnose_hardware(
    slurm_gpus: Option<u64>,
    observed_gpus: Option<u64>,
    slurm_cpus: Option<u64>,
    observed_cpus: Option<u64>,
) -> Vec<String> {
    let mut inventory = Inventory::new();

    let mut hardware = serde_json::Map::new();
    if let Some(gpus) = observed_gpus {
        hardware.insert("gpus".into(), gpus.into());
    }
    if let Some(cpus) = observed_cpus {
        hardware.insert("cpus".into(), cpus.into());
    }

    let mut entity = ManagedEntity::new("lab", EntityType::Host, "node-a")
        .with_capabilities(CapabilitySet::from_iter(["gpu.nvidia", "slurm.compute"]));
    entity.metadata = serde_json::json!({ "hardware": serde_json::Value::Object(hardware) });
    inventory.insert_entity(entity);

    let mut payload = serde_json::Map::new();
    payload.insert("schedulable".into(), true.into());
    if let Some(gpus) = slurm_gpus {
        payload.insert("configured_gpu_count".into(), gpus.into());
    }
    if let Some(cpus) = slurm_cpus {
        payload.insert("cpu_total".into(), cpus.into());
    }

    let observations = ObservationIndex::from_observations([Observation::new(
        ProbeId::new(PROBE_NODE),
        host_id("node-a"),
        ProbeStatus::Ok,
    )
    .with_payload(serde_json::Value::Object(payload))]);

    let states: HashMap<EntityId, EntityState> = HashMap::new();
    let context = DiagnosisContext {
        environment: "lab",
        inventory: &inventory,
        states: &states,
        observations: &observations,
    };

    builtin_rules()
        .diagnose(&context)
        .into_iter()
        .map(|d| d.diagnosis_type.to_string())
        .collect()
}

#[test]
fn a_real_nvidia_smi_output_parses_into_an_inventory() {
    let gpus = parse_query(FOUR_A100S);

    assert_eq!(gpus.len(), 4);
    assert_eq!(gpus[0].name, "NVIDIA A100-SXM4-40GB");
    assert_eq!(gpus[1].memory_used_mb, Some(20480));
    assert_eq!(gpus[1].utilization_percent, Some(78.0));

    // Every UUID is distinct, which is what makes a card identifiable across
    // reboots and reseats.
    let uuids: std::collections::BTreeSet<_> = gpus.iter().map(|g| g.uuid.as_str()).collect();
    assert_eq!(uuids.len(), 4);
}

#[tokio::test]
async fn a_host_with_no_gpus_and_no_expectation_is_not_a_fault() {
    // The common case, and the one that would produce a permanent false alarm
    // on every non-GPU machine if it were reported as a failure.
    let observation = NvidiaGpuProbe::new()
        .collect(&ProbeContext::local(host_id("node-a"), CapabilitySet::new()))
        .await;

    assert_eq!(observation.status, ProbeStatus::Unsupported);
    assert!(!observation.status.is_bad());
}

#[tokio::test]
async fn a_host_that_should_have_gpus_but_shows_none_is_a_failure() {
    let observation = NvidiaGpuProbe::new()
        .collect(
            &ProbeContext::local(host_id("node-a"), CapabilitySet::new())
                .with_parameters(serde_json::json!({"expected_gpu_count": 8})),
        )
        .await;

    assert_eq!(observation.status, ProbeStatus::Failed);
    assert_eq!(observation.payload["expected_gpu_count"], 8);
}

#[test]
fn a_gpu_count_disagreement_is_diagnosed() {
    // SPEC.md §82: Slurm's GRES expectation against what the host reports.
    let diagnoses = diagnose_hardware(Some(4), Some(2), None, None);
    assert!(
        diagnoses.contains(&kind::GPU_CONFIGURATION_MISMATCH.to_string()),
        "{diagnoses:?}"
    );
}

#[test]
fn matching_gpu_counts_produce_nothing() {
    assert!(diagnose_hardware(Some(4), Some(4), None, None).is_empty());
}

#[test]
fn a_node_slurm_has_no_gres_line_for_is_not_a_mismatch() {
    // "Not stated" is not "zero". A GPU node that Slurm has not been told
    // about is a configuration gap, but it is not a count disagreement, and
    // reporting one would be inventing an expectation.
    assert!(diagnose_hardware(None, Some(4), None, None).is_empty());
}

#[test]
fn a_host_that_has_never_reported_its_hardware_is_not_diagnosed() {
    // Before an agent registers there is nothing to compare against.
    assert!(diagnose_hardware(Some(4), None, None, None).is_empty());
}

#[test]
fn a_cpu_and_gpu_disagreement_together_is_reported_as_a_resource_mismatch() {
    // Two disagreements is a broader configuration problem than a GPU one.
    let diagnoses = diagnose_hardware(Some(4), Some(2), Some(64), Some(16));
    assert!(
        diagnoses.contains(&kind::RESOURCE_CONFIGURATION_MISMATCH.to_string()),
        "{diagnoses:?}"
    );
}

#[test]
fn slurm_configured_with_fewer_cpus_than_the_machine_has_is_not_a_mismatch() {
    // A deliberate and common choice; flagging it would be noise.
    assert!(diagnose_hardware(Some(4), Some(4), Some(16), Some(64)).is_empty());
}

#[test]
fn the_gpu_probe_is_read_only() {
    // nvidia-smi can reset devices, set persistence mode and change power
    // limits. v1 does none of that (SPEC.md §113).
    let source = include_str!("../src/probes/gpu.rs");
    let implementation = source.split("#[cfg(test)]").next().expect("implementation");
    for forbidden in ["--gpu-reset", "--persistence-mode", "-pl ", "--applications-clocks"] {
        assert!(
            !implementation.contains(forbidden),
            "the GPU probe must never pass {forbidden}"
        );
    }
}
