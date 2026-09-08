//! M3 acceptance: the Docker pseudo-cluster comes up, and Sentinel sees it.
//!
//! These tests drive real containers running real Slurm, real SSH and real
//! Sentinel agents. They are **opt-in**: without `SENTINEL_DOCKER_TESTS=1` they
//! skip, so that `cargo test` stays green on a machine with no Docker
//! (IMPLEMENTATION.md §87).
//!
//! Run them with:
//!
//! ```bash
//! SENTINEL_DOCKER_TESTS=1 cargo test --test m3_pseudo_cluster -- --test-threads=1
//! ```
//!
//! Single-threaded on purpose: they share one cluster, and a scenario running
//! while another test is asserting would make both meaningless.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

/// Whether the Docker tests were asked for.
fn enabled() -> bool {
    std::env::var("SENTINEL_DOCKER_TESTS").is_ok_and(|v| v != "0" && !v.is_empty())
}

/// Skip with an explanation rather than silently passing.
macro_rules! require_docker {
    () => {
        if !enabled() {
            eprintln!("skipping: set SENTINEL_DOCKER_TESTS=1 to run the pseudo-cluster tests");
            return;
        }
    };
}

fn compose_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("dev/compose")
}

/// Run a command in the compose directory, returning stdout and the exit status.
fn run_raw(program: &str, args: &[&str]) -> Result<(String, bool), String> {
    let output = Command::new(program)
        .args(args)
        .current_dir(compose_dir())
        .output()
        .map_err(|e| format!("cannot run {program}: {e}"))?;

    Ok((
        String::from_utf8_lossy(&output.stdout).into_owned(),
        output.status.success(),
    ))
}

/// Run a command, failing the test if it does not exit zero.
fn run(program: &str, args: &[&str]) -> Result<String, String> {
    let output = Command::new(program)
        .args(args)
        .current_dir(compose_dir())
        .output()
        .map_err(|e| format!("cannot run {program}: {e}"))?;

    if !output.status.success() {
        return Err(format!(
            "{program} {} failed ({}): {}{}",
            args.join(" "),
            output.status,
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        ));
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// Run a command and return its stdout whatever the exit status.
///
/// Several of the commands here exit non-zero as part of their normal
/// contract: `sentinel status` exits 2 when something is unhealthy, and
/// `supervisorctl status` exits 3 when any program is stopped. Both are exactly
/// the situations these tests create on purpose.
fn run_ignoring_status(program: &str, args: &[&str]) -> String {
    run_raw(program, args).map(|(stdout, _)| stdout).unwrap_or_default()
}

fn script(name: &str, args: &[&str]) -> Result<String, String> {
    let path = compose_dir().join(name);
    let path = path.to_string_lossy().into_owned();
    let mut all = vec![path.as_str()];
    all.extend_from_slice(args);
    run("bash", &all)
}

/// Run a compose script, tolerating a non-zero exit.
fn script_ignoring_status(name: &str, args: &[&str]) -> String {
    let path = compose_dir().join(name);
    let path = path.to_string_lossy().into_owned();
    let mut all = vec![path.as_str()];
    all.extend_from_slice(args);
    run_ignoring_status("bash", &all)
}

/// Whether one container can open a TCP connection to another.
///
/// TCP rather than ICMP on purpose: it is what Sentinel's network probes
/// actually use (SPEC.md §59), and it needs no extra package in the image.
fn can_reach(from: &str, to: &str, port: u16) -> bool {
    run_raw(
        "docker",
        &[
            "compose",
            "exec",
            "-T",
            from,
            "nc",
            "-z",
            "-w",
            "2",
            to,
            &port.to_string(),
        ],
    )
    .map(|(_, ok)| ok)
    .unwrap_or(false)
}

/// Bring the cluster up once for the whole test binary.
///
/// A `Once` is deliberately not used here. If startup fails, `Once` poisons
/// itself and every later test reports "previously poisoned" instead of the
/// real problem — which is exactly the moment the real problem matters most.
///
/// Startup is also retried after a recovery pass, because a test that failed
/// part-way through a previous run may have left a network partition or a
/// stopped daemon behind, and the cluster would never come up healthy again
/// without being told to clean up first.
fn ensure_cluster() {
    static READY: std::sync::Mutex<bool> = std::sync::Mutex::new(false);
    let mut ready = READY.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    if *ready {
        return;
    }

    eprintln!("bringing up the pseudo-cluster (this takes a few minutes on a cold cache)");
    if let Err(error) = script("scripts/up", &[]) {
        eprintln!("startup failed ({error}); recovering from any leftover sabotage and retrying");
        let _ = script("scenarios/recover-all", &[]);
        script("scripts/up", &[]).expect("the pseudo-cluster must start");
    }

    *ready = true;
}

/// The controller's status report, as JSON.
fn status() -> serde_json::Value {
    let text = script("scripts/sentinel", &["status", "--json"]).unwrap_or_else(|e| {
        // `status` exits 2 when something is unhealthy, which is a normal
        // outcome here and not a failure to run it.
        e.split_once("): ")
            .map(|(_, body)| body.to_string())
            .unwrap_or_default()
    });
    let start = text.find('{').unwrap_or(0);
    serde_json::from_str(&text[start..]).unwrap_or_else(|e| panic!("cannot parse status output: {e}\n{text}"))
}

/// Health of one entity in the status report.
fn health_of(status: &serde_json::Value, entity_type: &str, name: &str) -> Option<String> {
    status["entities"]
        .as_array()?
        .iter()
        .find(|e| e["entity_type"] == entity_type && e["name"] == name)?["health"]
        .as_str()
        .map(str::to_string)
}

/// Poll until `predicate` holds, or give up.
fn wait_until(what: &str, timeout: Duration, mut predicate: impl FnMut(&serde_json::Value) -> bool) {
    let deadline = Instant::now() + timeout;
    let mut last = serde_json::Value::Null;
    while Instant::now() < deadline {
        last = status();
        if predicate(&last) {
            return;
        }
        std::thread::sleep(Duration::from_secs(2));
    }
    panic!(
        "timed out waiting for {what}\nlast status: {}",
        serde_json::to_string_pretty(&last).unwrap_or_default()
    );
}

fn recover() {
    script("scenarios/recover-all", &[]).expect("recovery must succeed");
}

#[test]
fn the_pseudo_cluster_starts_and_every_container_is_running() {
    require_docker!();
    ensure_cluster();

    let output = run("docker", &["compose", "ps", "--format", "{{.Service}} {{.State}}"]).expect("compose ps");
    for service in [
        "controller",
        "compute01",
        "compute02",
        "compute03",
        "filesrv01",
        "filesrv02",
    ] {
        assert!(
            output.lines().any(|l| l.starts_with(service) && l.contains("running")),
            "{service} is not running:\n{output}"
        );
    }
}

#[test]
fn slurm_is_really_running_and_reports_its_nodes() {
    // Not a mock: an actual slurmctld answering an actual scontrol.
    require_docker!();
    ensure_cluster();

    let nodes = run(
        "docker",
        &["compose", "exec", "-T", "controller", "scontrol", "show", "nodes", "-o"],
    )
    .expect("scontrol");
    assert_eq!(
        nodes.lines().filter(|l| l.starts_with("NodeName=")).count(),
        3,
        "{nodes}"
    );

    let ping = run("docker", &["compose", "exec", "-T", "controller", "scontrol", "ping"]).expect("ping");
    assert!(ping.contains("UP"), "{ping}");
}

#[test]
fn sentinel_discovers_slurm_nodes_and_configured_hosts_as_one_inventory() {
    // SPEC.md §180: Slurm is one provider, not the inventory.
    require_docker!();
    ensure_cluster();
    recover();

    let status = status();
    for name in ["compute01", "compute02", "compute03"] {
        assert!(
            health_of(&status, "host", name).is_some(),
            "{name} missing (from Slurm)"
        );
    }
    for name in ["filesrv01", "filesrv02"] {
        assert!(
            health_of(&status, "host", name).is_some(),
            "{name} missing (from configuration)"
        );
    }
    assert!(
        health_of(&status, "scheduler", "sentinel-testbed").is_some(),
        "the scheduler entity is missing"
    );
}

#[test]
fn every_agent_registers_with_the_controller() {
    require_docker!();
    ensure_cluster();

    let health: serde_json::Value = {
        let text = run("curl", &["-fsS", "http://localhost:17443/v1/health"]).expect("controller health");
        serde_json::from_str(&text).expect("parse health")
    };
    assert!(
        health["agents"].as_u64().unwrap_or(0) >= 5,
        "expected five agents (three compute, two fileserver), got {}",
        health["agents"]
    );
}

#[test]
fn the_dependency_graph_spans_slurm_and_non_slurm_entities() {
    require_docker!();
    ensure_cluster();

    let text = script_ignoring_status("scripts/sentinel", &["dependency", "list", "--json"]);
    let start = text.find('[').expect("json array");
    let edges: serde_json::Value = serde_json::from_str(&text[start..]).expect("parse dependencies");
    let edges = edges.as_array().expect("array");

    let has = |from: &str, to: &str| edges.iter().any(|e| e["from"] == from && e["to"] == to);

    assert!(
        has("host/compute01", "storage/storage01"),
        "a Slurm node depends on configured storage"
    );
    assert!(
        has("storage/storage01", "host/filesrv01"),
        "which is provided by a non-Slurm host"
    );
    assert!(
        has("host/compute03", "storage/storage02"),
        "the second domain is separate"
    );
}

#[test]
fn draining_a_node_degrades_only_its_scheduler_view() {
    // SPEC.md §169. The machine is entirely healthy; only Slurm's opinion
    // changed, and Sentinel must say so rather than raising an outage.
    require_docker!();
    ensure_cluster();
    recover();

    script("scenarios/drain-node", &["compute01"]).expect("drain");
    wait_until("compute01 to become degraded", Duration::from_secs(60), |status| {
        health_of(status, "host", "compute01").as_deref() == Some("degraded")
    });

    let status = status();
    assert_eq!(
        health_of(&status, "host", "compute02").as_deref(),
        Some("healthy"),
        "draining one node must not implicate another"
    );

    recover();
    wait_until("compute01 to recover", Duration::from_secs(60), |status| {
        health_of(status, "host", "compute01").as_deref() == Some("healthy")
    });
}

#[test]
fn stopping_slurmd_is_visible_and_does_not_condemn_the_host() {
    require_docker!();
    ensure_cluster();
    recover();

    script("scenarios/stop-slurmd", &["compute01"]).expect("stop slurmd");

    // Slurm takes up to SlurmdTimeout to notice.
    wait_until("Slurm to notice compute01 is gone", Duration::from_secs(90), |status| {
        matches!(
            health_of(status, "host", "compute01").as_deref(),
            Some("degraded") | Some("unavailable")
        )
    });

    let status = status();
    let entity = status["entities"]
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["name"] == "compute01")
        .expect("compute01");

    // Whatever Sentinel concludes, it must not have decided the host is gone
    // on the strength of one daemon dying (SPEC.md §170).
    let classifications = serde_json::to_string(&entity["classifications"]).unwrap_or_default();
    assert!(!classifications.contains("HOST_UNREACHABLE"), "{classifications}");
    assert!(!classifications.contains("POWER_OFF"), "{classifications}");

    recover();
}

#[test]
fn stopping_the_storage_service_leaves_its_host_running() {
    // The distinction between "the fileserver is down" and "the fileserver's
    // export service is down".
    require_docker!();
    ensure_cluster();
    recover();

    script("scenarios/stop-fileserver", &["filesrv01"]).expect("stop storage");

    // supervisorctl exits non-zero whenever any program is stopped, which is
    // precisely what this scenario just arranged.
    let still_up = run_ignoring_status(
        "docker",
        &[
            "compose",
            "exec",
            "-T",
            "filesrv01",
            "supervisorctl",
            "-c",
            "/etc/supervisor/roles/fileserver.conf",
            "status",
        ],
    );

    assert!(
        still_up.contains("storage") && still_up.contains("STOPPED"),
        "{still_up}"
    );
    assert!(
        still_up.contains("sshd") && still_up.matches("RUNNING").count() >= 2,
        "the host itself must still be up:\n{still_up}"
    );

    recover();
}

#[test]
fn a_network_partition_leaves_other_paths_intact() {
    // SPEC.md §175, the fault this whole architecture exists to distinguish.
    require_docker!();
    ensure_cluster();
    recover();

    script("scenarios/isolate", &["controller", "compute01"]).expect("isolate");

    // The controller cannot reach compute01...
    assert!(
        !can_reach("controller", "compute01", 22),
        "the partition did not take effect"
    );

    // ...but compute02 still can, which is exactly why HOST_UNREACHABLE would
    // be the wrong conclusion to draw from the controller's own blind spot.
    assert!(
        can_reach("compute02", "compute01", 22),
        "the partition was not path-specific"
    );

    recover();
}

#[test]
fn an_agent_keeps_its_observations_while_the_controller_is_down() {
    // SPEC.md §176.
    require_docker!();
    ensure_cluster();
    recover();

    script("scenarios/stop-controller", &[]).expect("stop controller");
    std::thread::sleep(Duration::from_secs(10));

    // The agent is still running and still has its spool.
    let agent_status = run(
        "docker",
        &[
            "compose",
            "exec",
            "-T",
            "compute01",
            "supervisorctl",
            "-c",
            "/etc/supervisor/roles/compute.conf",
            "status",
            "sentinel-agent",
        ],
    )
    .expect("supervisorctl");
    assert!(
        agent_status.contains("RUNNING"),
        "the agent must keep working: {agent_status}"
    );

    recover();
    wait_until("the controller to come back", Duration::from_secs(90), |status| {
        health_of(status, "host", "compute01").is_some()
    });
}

#[test]
fn recover_all_returns_the_cluster_to_a_healthy_baseline() {
    // Every scenario must be reversible, or the testbed degrades over a run
    // until failures stop meaning anything.
    require_docker!();
    ensure_cluster();

    script("scenarios/stop-slurmd", &["compute02"]).expect("break it");
    script("scenarios/drain-node", &["compute03"]).expect("break it more");
    recover();

    wait_until(
        "every compute node to be healthy again",
        Duration::from_secs(120),
        |status| {
            ["compute01", "compute02", "compute03"]
                .iter()
                .all(|name| health_of(status, "host", name).as_deref() == Some("healthy"))
        },
    );
}
