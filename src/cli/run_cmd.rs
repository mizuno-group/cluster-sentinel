//! Command wiring for the verbs that need a database or a controller.

use crate::config::Config;
use crate::controller::Controller;
use crate::persistence::{PruneMode, PruneOutcome, SqliteStore};

use super::{status_cmd, Cli, DependencyCommand, EntityCommand, IncidentCommand};
use crate::diagnosis::Diagnosis;

/// Open the configured database, applying migrations.
async fn open_store(config: &Config) -> anyhow::Result<SqliteStore> {
    Ok(SqliteStore::open(&config.database.path).await?)
}

/// `sentinel status`.
pub async fn status(cli: &Cli, json: bool) -> anyhow::Result<i32> {
    let config = Config::load(&cli.config)?;
    let store = open_store(&config).await?;
    let report = status_cmd::load_report(&store, &config.environment).await?;

    if json {
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else {
        print!("{}", status_cmd::render(&report));
    }

    // Exit non-zero when something is wrong, so `sentinel status` composes with
    // shell scripts and health checks.
    Ok(if report.is_healthy() { 0 } else { 2 })
}

/// `sentinel discover`: run one discovery cycle now.
pub async fn discover(cli: &Cli, json: bool) -> anyhow::Result<i32> {
    let config = Config::load(&cli.config)?;
    let store = open_store(&config).await?;
    let mut controller = Controller::new(config, store).await?;
    let report = controller.discover_once().await?;

    if json {
        let providers: Vec<_> = report
            .providers
            .iter()
            .map(|p| {
                serde_json::json!({
                    "provider": p.provider,
                    "entities": p.entities,
                    "dependencies": p.dependencies,
                    "error": p.error,
                })
            })
            .collect();
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "providers": providers,
                "entities": report.entities,
                "dependencies": report.dependencies,
                "observations": report.observations,
                "transitions": report.transitions.len(),
            }))?
        );
    } else {
        for provider in &report.providers {
            match &provider.error {
                Some(error) => println!("{:<16} FAILED  {error}", provider.provider),
                None => println!(
                    "{:<16} ok      {} entities, {} dependencies",
                    provider.provider, provider.entities, provider.dependencies
                ),
            }
        }
        println!(
            "\ninventory: {} entities, {} dependencies\nobservations: {} new, {} state transitions",
            report.entities,
            report.dependencies,
            report.observations,
            report.transitions.len()
        );
    }

    // A provider failing is worth a non-zero exit: discovery ran, but the
    // picture is incomplete and a caller should know.
    Ok(if report.all_providers_ok() { 0 } else { 1 })
}

/// `sentinel incident ...`.
pub async fn incident(cli: &Cli, command: &IncidentCommand) -> anyhow::Result<i32> {
    let config = Config::load(&cli.config)?;
    let store = open_store(&config).await?;
    let inventory = store.load_inventory(&config.environment).await?;

    let name_of = |id: crate::entity::EntityId| {
        inventory
            .get(id)
            .map(|e| format!("{}/{}", e.entity_type, e.canonical_name))
            .unwrap_or_else(|| id.to_string())
    };

    match command {
        IncidentCommand::List { all, json } => {
            let incidents = if *all {
                store.load_incidents(&config.environment, 200).await?
            } else {
                store.load_active_incidents(&config.environment).await?
            };

            if *json {
                println!("{}", serde_json::to_string_pretty(&incidents)?);
                return Ok(if incidents.is_empty() { 0 } else { 2 });
            }

            if incidents.is_empty() {
                println!("No {}incidents.", if *all { "" } else { "active " });
                return Ok(0);
            }

            for incident in &incidents {
                let cause = if incident.suspected_root_entities.is_empty() {
                    "(unattributed)".to_string()
                } else {
                    incident
                        .suspected_root_entities
                        .iter()
                        .map(|id| name_of(*id))
                        .collect::<Vec<_>>()
                        .join(", ")
                };
                println!(
                    "{:<38} {:<9} {:<11} {}",
                    incident.id,
                    incident.severity.to_string().to_uppercase(),
                    incident.status,
                    cause
                );
                if let Some(diagnosis) = incident.primary_diagnosis() {
                    println!("{:<38} {}", "", diagnosis.summary);
                }
            }

            Ok(if incidents.iter().any(|i| i.status.is_active()) {
                2
            } else {
                0
            })
        }
        IncidentCommand::Show { id, json } => {
            let Some(incident) = store.load_incident(id).await? else {
                eprintln!("error: no incident with id {id}");
                return Ok(1);
            };

            if *json {
                println!("{}", serde_json::to_string_pretty(&incident)?);
                return Ok(0);
            }

            println!("Incident:  {}", incident.id);
            println!("Status:    {}", incident.status);
            println!("Severity:  {}", incident.severity.to_string().to_uppercase());
            println!("Started:   {}", crate::time::to_rfc3339(incident.started_at));
            if let Some(ended_at) = incident.ended_at {
                println!("Ended:     {}", crate::time::to_rfc3339(ended_at));
            }
            println!("Duration:  {}", format_duration(incident.duration()));

            if !incident.suspected_root_entities.is_empty() {
                println!(
                    "\nSuspected cause:\n  {}",
                    incident
                        .suspected_root_entities
                        .iter()
                        .map(|id| name_of(*id))
                        .collect::<Vec<_>>()
                        .join("\n  ")
                );
            }
            if !incident.affected_entities.is_empty() {
                println!(
                    "\nAffected:\n  {}",
                    incident
                        .affected_entities
                        .iter()
                        .map(|id| name_of(*id))
                        .collect::<Vec<_>>()
                        .join("\n  ")
                );
            }

            println!("\nDiagnoses:");
            for diagnosis in &incident.diagnoses {
                println!("  {} [{}]", diagnosis.diagnosis_type, diagnosis.confidence);
                println!("    {}", diagnosis.summary);
                println!("    rule: {}", diagnosis.rule_id);
                if !diagnosis.recommended_actions.is_empty() {
                    println!("    suggested investigation (read-only):");
                    for action in &diagnosis.recommended_actions {
                        println!("      {action}");
                    }
                }
            }

            println!("\nTimeline:");
            for event in &incident.timeline {
                println!(
                    "  {}  {:<20} {}",
                    crate::time::to_rfc3339(event.at),
                    event.kind,
                    event.detail
                );
            }

            // The evidence is the point: an incident an operator cannot check
            // is an assertion (SPEC.md §103).
            println!("\nEvidence: {} observation(s) preserved", incident.evidence.len());

            Ok(0)
        }
    }
}

/// Render a duration the way an operator reads one.
fn format_duration(duration: chrono::Duration) -> String {
    let seconds = duration.num_seconds().max(0);
    match seconds {
        s if s < 60 => format!("{s}s"),
        s if s < 3600 => format!("{}m {}s", s / 60, s % 60),
        s => format!("{}h {}m", s / 3600, (s % 3600) / 60),
    }
}

/// `sentinel peers`: show who observes whom.
///
/// Worth being able to see directly: an entity watched by only one observer
/// cannot have `HOST_UNREACHABLE` concluded about it, and an operator wondering
/// why should be able to find out in one command.
pub async fn peers(cli: &Cli, json: bool) -> anyhow::Result<i32> {
    let config = Config::load(&cli.config)?;
    let store = open_store(&config).await?;
    let controller = Controller::new(config.clone(), store).await?;

    let plan = controller.assignment_plan().await?;
    let inventory = controller.store().load_inventory(&config.environment).await?;
    let name_of = |id: crate::entity::EntityId| {
        inventory
            .get(id)
            .map(|e| e.canonical_name.clone())
            .unwrap_or_else(|| id.to_string())
    };

    if json {
        let rendered: Vec<_> = plan
            .assignments
            .iter()
            .map(|assignment| {
                serde_json::json!({
                    "target": name_of(assignment.target),
                    "independent_viewpoints": assignment.independent_viewpoints(),
                    "observers": assignment
                        .observers
                        .iter()
                        .map(|o| serde_json::json!({"observer": name_of(o.entity), "role": o.role}))
                        .collect::<Vec<_>>(),
                })
            })
            .collect();
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "revision": plan.revision,
                "degree": config.peer_monitoring.degree,
                "assignments": rendered,
            }))?
        );
        return Ok(0);
    }

    if plan.assignments.is_empty() {
        println!("No entities to observe yet.");
        return Ok(0);
    }

    println!(
        "Peer assignment (revision {}, degree {})
",
        plan.revision, config.peer_monitoring.degree
    );

    let mut unwatched = Vec::new();
    for assignment in &plan.assignments {
        let target = name_of(assignment.target);
        if assignment.observers.is_empty() {
            unwatched.push(target);
            continue;
        }
        let observers: Vec<String> = assignment
            .observers
            .iter()
            .map(|o| format!("{} ({:?})", name_of(o.entity), o.role))
            .collect();
        println!("{target:<20} <- {}", observers.join(", "));
    }

    if !unwatched.is_empty() {
        // Said plainly, because it is the reason a host will never be
        // diagnosed as unreachable.
        println!(
            "\n{} entity/entities have no observer, so no reachability conclusion can be drawn about them:",
            unwatched.len()
        );
        for target in unwatched {
            println!("  {target}");
        }
        println!("\nGive hosts the `observer.peer` capability to fix this.");
    }

    Ok(0)
}

/// `sentinel diagnose`: explain what is wrong.
pub async fn diagnose(cli: &Cli, json: bool) -> anyhow::Result<i32> {
    let config = Config::load(&cli.config)?;
    let store = open_store(&config).await?;
    let controller = Controller::new(config, store).await?;
    let diagnoses = controller.diagnose().await?;

    if json {
        println!("{}", serde_json::to_string_pretty(&diagnoses)?);
    } else {
        print!("{}", render_diagnoses(&controller, &diagnoses).await?);
    }

    Ok(if diagnoses.is_empty() { 0 } else { 2 })
}

/// `sentinel prune`.
///
/// The same pass the controller runs on its own, exposed so an operator can
/// choose when the first one happens on a database that has been growing for
/// months, and so lowering a retention period can be tried before it is meant.
pub async fn prune(
    cli: &Cli,
    dry_run: bool,
    vacuum: bool,
    observations: Option<&str>,
    json: bool,
) -> anyhow::Result<i32> {
    let config = Config::load(&cli.config)?;
    let mut retention = config.retention.clone();

    // An override applies to this run only. Pruning is irreversible, so it is
    // reached through an explicit flag rather than by editing the config file
    // and restarting, which would also change what the daemon does forever.
    if let Some(period) = observations {
        retention.observations = parse_retention_period(period)?;
    }
    // `prune` was asked for, so it happens: a config that has switched
    // retention off should not silently turn this into a no-op.
    retention.enabled = true;

    let store = open_store(&config).await?;
    let before = store.database_bytes().await?;
    let mode = if dry_run { PruneMode::DryRun } else { PruneMode::Delete };
    let outcome = store.prune_at(&retention, crate::time::now(), mode).await?;

    if vacuum && !dry_run {
        store.vacuum().await?;
    }
    let after = store.database_bytes().await?;

    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "dry_run": dry_run,
                "vacuumed": vacuum && !dry_run,
                "deleted": {
                    "observations": outcome.observations,
                    "transitions": outcome.transitions,
                    "incidents": outcome.incidents,
                    "diagnoses": outcome.diagnoses,
                },
                "database_bytes_before": before,
                "database_bytes_after": after,
            }))?
        );
    } else {
        print!("{}", render_prune(&outcome, dry_run, vacuum, before, after));
    }

    Ok(0)
}

/// Parse a retention period given on the command line.
fn parse_retention_period(text: &str) -> anyhow::Result<crate::config::RetentionPeriod> {
    // Deserialize rather than reimplement, so the command line and the config
    // file accept exactly the same spellings, "never" included.
    let quoted = format!("period = {}", serde_json::to_string(text)?);
    #[derive(serde::Deserialize)]
    struct Wrapper {
        period: crate::config::RetentionPeriod,
    }
    let parsed: Wrapper =
        toml::from_str(&quoted).map_err(|e| anyhow::anyhow!("--observations {text:?}: {}", e.message()))?;
    Ok(parsed.period)
}

/// Render a pruning outcome for a human.
fn render_prune(outcome: &PruneOutcome, dry_run: bool, vacuum: bool, before: u64, after: u64) -> String {
    let mut out = String::new();
    if dry_run {
        out.push_str("Dry run: nothing was deleted.\n\n");
    }
    let verb = if dry_run { "would delete" } else { "deleted" };
    out.push_str(&format!("{} {}\n", capitalise(verb), outcome));

    out.push_str(&format!("\nDatabase: {}\n", human_bytes(before)));
    if !dry_run {
        if vacuum {
            out.push_str(&format!("After vacuum: {}\n", human_bytes(after)));
        } else if outcome.total() > 0 {
            // Saying "still 900 MB" without saying why invites a bug report.
            out.push_str("Freed pages are reused by new records. Pass --vacuum to return them\nto the filesystem.\n");
        }
    }
    out
}

fn capitalise(s: &str) -> String {
    let mut chars = s.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
        None => String::new(),
    }
}

/// Bytes in the units an operator thinks in.
fn human_bytes(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

/// Render diagnoses for a human, resolving entity ids to names.
async fn render_diagnoses(controller: &Controller, diagnoses: &[Diagnosis]) -> anyhow::Result<String> {
    if diagnoses.is_empty() {
        return Ok("No problems diagnosed.\n".to_string());
    }

    let inventory = controller
        .store()
        .load_inventory(&controller.config().environment)
        .await?;
    let name_of = |id: crate::entity::EntityId| {
        inventory
            .get(id)
            .map(|e| format!("{}/{}", e.entity_type, e.canonical_name))
            .unwrap_or_else(|| id.to_string())
    };

    let mut out = String::new();
    for diagnosis in diagnoses {
        out.push_str(&format!("{} [{}]\n", diagnosis.diagnosis_type, diagnosis.confidence));
        out.push_str(&format!("  {}\n", diagnosis.summary));

        if !diagnosis.suspected_root_entities.is_empty() {
            let roots: Vec<_> = diagnosis
                .suspected_root_entities
                .iter()
                .map(|id| name_of(*id))
                .collect();
            out.push_str(&format!("  suspected cause: {}\n", roots.join(", ")));
        }
        if !diagnosis.affected_entities.is_empty() {
            let affected: Vec<_> = diagnosis.affected_entities.iter().map(|id| name_of(*id)).collect();
            out.push_str(&format!("  affected:        {}\n", affected.join(", ")));
        }

        out.push_str(&format!("  rule:            {}\n", diagnosis.rule_id));
        out.push_str(&format!(
            "  evidence:        {} observation(s)\n",
            diagnosis.evidence.len()
        ));

        if !diagnosis.recommended_actions.is_empty() {
            out.push_str("  suggested investigation (read-only):\n");
            for action in &diagnosis.recommended_actions {
                out.push_str(&format!("    {action}\n"));
            }
        }
        out.push('\n');
    }

    Ok(out)
}

/// `sentinel entity ...`.
pub async fn entity(cli: &Cli, command: &EntityCommand) -> anyhow::Result<i32> {
    let config = Config::load(&cli.config)?;
    let store = open_store(&config).await?;
    let report = status_cmd::load_report(&store, &config.environment).await?;

    match command {
        EntityCommand::List { json, entity_type } => {
            let entities: Vec<_> = report
                .entities
                .iter()
                .filter(|e| entity_type.as_ref().is_none_or(|t| &e.entity_type == t))
                .collect();

            if *json {
                println!("{}", serde_json::to_string_pretty(&entities)?);
            } else if entities.is_empty() {
                println!("No matching entities.");
            } else {
                for entity in entities {
                    println!(
                        "{:<20} {:<10} {}",
                        entity.name,
                        entity.entity_type,
                        entity.health.to_uppercase()
                    );
                }
            }
            Ok(0)
        }
        EntityCommand::Show { name, json } => {
            // Accept either the canonical name or the entity id, so an id
            // copied out of a diagnosis can be pasted straight back in.
            let Some(entity) = report.entities.iter().find(|e| &e.name == name || &e.id == name) else {
                eprintln!(
                    "error: no entity named {name:?} in environment {:?}",
                    config.environment
                );
                return Ok(1);
            };
            if *json {
                println!("{}", serde_json::to_string_pretty(entity)?);
            } else {
                print!("{}", status_cmd::render_entity(entity));
            }
            Ok(0)
        }
    }
}

/// `sentinel dependency ...`.
pub async fn dependency(cli: &Cli, command: &DependencyCommand) -> anyhow::Result<i32> {
    let config = Config::load(&cli.config)?;
    let store = open_store(&config).await?;
    let inventory = store.load_inventory(&config.environment).await?;

    let name_of = |id: crate::entity::EntityId| {
        inventory
            .get(id)
            .map(|e| format!("{}/{}", e.entity_type, e.canonical_name))
            .unwrap_or_else(|| id.to_string())
    };

    match command {
        DependencyCommand::List { json } => {
            let edges: Vec<_> = inventory
                .graph()
                .edges()
                .iter()
                .map(|edge| {
                    serde_json::json!({
                        "from": name_of(edge.source),
                        "to": name_of(edge.target),
                        "type": edge.dependency_type.as_str(),
                        "criticality": edge.criticality.as_str(),
                        "discovery_source": edge.discovery_source.as_str(),
                    })
                })
                .collect();

            if *json {
                println!("{}", serde_json::to_string_pretty(&edges)?);
            } else if edges.is_empty() {
                println!("No dependencies known.");
            } else {
                for edge in &edges {
                    println!(
                        "{} --{}--> {}  ({})",
                        edge["from"].as_str().unwrap_or_default(),
                        edge["type"].as_str().unwrap_or_default(),
                        edge["to"].as_str().unwrap_or_default(),
                        edge["criticality"].as_str().unwrap_or_default()
                    );
                }
            }
            Ok(0)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::Command;
    use std::io::Write;

    fn write_config(dir: &std::path::Path, extra: &str) -> std::path::PathBuf {
        let path = dir.join("config.toml");
        let mut file = std::fs::File::create(&path).expect("create");
        // `observe = false`: these tests exercise the CLI plumbing, not remote
        // probing, and probing unresolvable fixture names would spend a
        // connection timeout each while asserting nothing.
        write!(
            file,
            "config_version = 1\nenvironment = \"lab\"\n\n[controller]\nobserve = false\n\n[database]\npath = \"{}\"\n{extra}",
            dir.join("sentinel.db").display()
        )
        .expect("write");
        path
    }

    fn cli_for(path: &std::path::Path) -> Cli {
        Cli {
            config: path.to_path_buf(),
            verbose: 0,
            log_json: false,
            command: Command::Status { json: false },
        }
    }

    #[tokio::test]
    async fn status_on_an_empty_environment_succeeds() {
        let dir = tempfile::tempdir().expect("tempdir");
        let cli = cli_for(&write_config(dir.path(), ""));
        assert_eq!(status(&cli, false).await.expect("status"), 0);
        assert_eq!(status(&cli, true).await.expect("status json"), 0);
    }

    #[tokio::test]
    async fn discover_then_status_reports_the_configured_entities() {
        let dir = tempfile::tempdir().expect("tempdir");
        let cli = cli_for(&write_config(
            dir.path(),
            "\n[[entities]]\ntype = \"host\"\nname = \"fileserver-a\"\ncapabilities = [\"storage.nfs.server\"]\n",
        ));

        assert_eq!(discover(&cli, false).await.expect("discover"), 0);

        let config = Config::load(&cli.config).expect("config");
        let store = SqliteStore::open(&config.database.path).await.expect("store");
        let report = status_cmd::load_report(&store, "lab").await.expect("report");

        assert_eq!(report.entities.len(), 1);
        assert_eq!(report.entities[0].name, "fileserver-a");
        // Nothing has observed it yet, so it must read unknown, not healthy.
        assert_eq!(report.entities[0].health, "unknown");
    }

    #[tokio::test]
    async fn status_exits_non_zero_when_something_is_degraded() {
        use crate::state::{ComponentState, EntityState, Health, StateComponent};

        let dir = tempfile::tempdir().expect("tempdir");
        let cli = cli_for(&write_config(
            dir.path(),
            "\n[[entities]]\ntype = \"host\"\nname = \"node-a\"\n",
        ));
        discover(&cli, false).await.expect("discover");

        let config = Config::load(&cli.config).expect("config");
        let store = SqliteStore::open(&config.database.path).await.expect("store");
        let id = crate::entity::EntityKey::new("lab", crate::entity::EntityType::Host, "node-a").entity_id();
        let mut state = EntityState::unknown(id);
        state.set_component(StateComponent::Scheduler, ComponentState::new(Health::Degraded));
        store.save_entity_state(&state).await.expect("save state");
        store.close().await;

        assert_eq!(status(&cli, false).await.expect("status"), 2);
    }

    #[tokio::test]
    async fn showing_an_unknown_entity_fails_cleanly() {
        let dir = tempfile::tempdir().expect("tempdir");
        let cli = cli_for(&write_config(dir.path(), ""));
        let command = EntityCommand::Show {
            name: "nope".into(),
            json: false,
        };
        assert_eq!(entity(&cli, &command).await.expect("entity show"), 1);
    }

    #[tokio::test]
    async fn an_entity_can_be_shown_by_name_or_by_id() {
        let dir = tempfile::tempdir().expect("tempdir");
        let cli = cli_for(&write_config(
            dir.path(),
            "\n[[entities]]\ntype = \"host\"\nname = \"node-a\"\n",
        ));
        discover(&cli, false).await.expect("discover");

        let id = crate::entity::EntityKey::new("lab", crate::entity::EntityType::Host, "node-a")
            .entity_id()
            .to_string();
        for name in ["node-a".to_string(), id] {
            let command = EntityCommand::Show { name, json: true };
            assert_eq!(entity(&cli, &command).await.expect("entity show"), 0);
        }
    }

    #[tokio::test]
    async fn dependencies_are_listed_with_readable_endpoint_names() {
        let dir = tempfile::tempdir().expect("tempdir");
        let cli = cli_for(&write_config(
            dir.path(),
            r#"
[[entities]]
type = "host"
name = "node-a"

[[entities]]
type = "storage"
name = "shared-a"

[[dependencies]]
from = "host/node-a"
to = "storage/shared-a"
type = "uses_storage"
"#,
        ));
        discover(&cli, false).await.expect("discover");
        assert_eq!(
            dependency(&cli, &DependencyCommand::List { json: true })
                .await
                .expect("list"),
            0
        );
    }

    #[tokio::test]
    async fn a_failing_provider_makes_discover_exit_non_zero() {
        let dir = tempfile::tempdir().expect("tempdir");
        let cli = cli_for(&write_config(
            dir.path(),
            "\n[discovery.slurm]\nenabled = true\nscontrol_path = \"/nonexistent/scontrol\"\n",
        ));
        assert_eq!(discover(&cli, false).await.expect("discover"), 1);
    }
}
