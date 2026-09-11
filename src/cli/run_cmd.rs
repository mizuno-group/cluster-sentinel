//! Command wiring for the verbs that need a database or a controller.

use crate::config::Config;
use crate::controller::Controller;
use crate::persistence::{PruneMode, PruneOutcome, SqliteStore};

use super::{status_cmd, Cli, DependencyCommand, EntityCommand, IncidentCommand};
use crate::diagnosis::Diagnosis;

/// Open the configured database, applying migrations.
pub(super) async fn open_store(config: &Config) -> anyhow::Result<SqliteStore> {
    Ok(SqliteStore::open(&config.database.path).await?)
}

/// `sentinel status`.
pub async fn status(cli: &Cli, json: bool) -> anyhow::Result<i32> {
    let config = Config::load(&cli.config)?;
    let store = open_store(&config).await?;
    let report = status_cmd::load_report_with(&store, &config.environment, Some(&config)).await?;

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
                "storage_edges": report.storage_edges,
                "unresolved_storage_servers": report
                    .unresolved_storage_servers
                    .iter()
                    .map(|u| serde_json::json!({"server": u.server, "clients": u.clients}))
                    .collect::<Vec<_>>(),
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

        // Silence here would be the worst outcome: the operator would believe
        // the storage graph is complete while some of it was skipped.
        if !report.unresolved_storage_servers.is_empty() {
            println!("\nNFS mounts that could not be tied to a known host:");
            for unresolved in &report.unresolved_storage_servers {
                println!("  {}  mounted by {}", unresolved.server, unresolved.clients.join(", "));
            }
            println!(
                "\nAn address is not an identity, so no entity is created from one.\n\
                 Declare the host it belongs to, and the rest of its topology follows:\n\n\
                \x20 [[entities]]\n\
                \x20 type = \"host\"\n\
                \x20 name = \"the-fileserver\"\n\
                \x20 addresses = [\"{}\"]",
                report.unresolved_storage_servers[0].server
            );
        }
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
            let found: Vec<&status_cmd::EntityStatus> = report
                .entities
                .iter()
                .filter(|e| entity_matches(&e.entity_type, &e.name, &e.id, name))
                .collect();
            let entity = match found.as_slice() {
                [one] => *one,
                [] => {
                    eprintln!(
                        "error: no entity named {name:?} in environment {:?}",
                        config.environment
                    );
                    return Ok(1);
                }
                many => {
                    let names: Vec<String> = many.iter().map(|e| format!("{}/{}", e.entity_type, e.name)).collect();
                    return Ok(ambiguous(name, &names));
                }
            };
            if *json {
                println!("{}", serde_json::to_string_pretty(entity)?);
            } else {
                print!("{}", status_cmd::render_entity(entity));
            }
            Ok(0)
        }
        EntityCommand::Observations {
            name,
            probe,
            limit,
            json,
        } => {
            let inventory = store.load_inventory(&config.environment).await?;
            let found: Vec<_> = inventory
                .entities()
                .filter(|e| entity_matches(e.entity_type.as_str(), &e.canonical_name, &e.id.to_string(), name))
                .collect();
            let target = match found.as_slice() {
                [one] => *one,
                [] => {
                    eprintln!(
                        "error: no entity named {name:?} in environment {:?}",
                        config.environment
                    );
                    return Ok(1);
                }
                many => {
                    let names: Vec<String> = many
                        .iter()
                        .map(|e| format!("{}/{}", e.entity_type.as_str(), e.canonical_name))
                        .collect();
                    return Ok(ambiguous(name, &names));
                }
            };

            // Filtered in the query when a probe is named. Loading the last
            // N and then filtering makes the limit apply to every probe at
            // once, so a probe running once a minute is invisible beside one
            // running every five seconds from three observers.
            let observations = match probe.as_deref() {
                Some(probe) => store.recent_observations_for_probe(target.id, probe, *limit).await?,
                None => store.recent_observations(target.id, *limit).await?,
            };

            if *json {
                println!("{}", serde_json::to_string_pretty(&observations)?);
            } else if observations.is_empty() {
                // Which of the two it is matters: "this probe has never run"
                // sends someone to look at capabilities, and "this entity is
                // unmonitored" sends them somewhere else entirely.
                match probe.as_deref() {
                    Some(probe) => println!(
                        "No {probe} observations recorded for {}.\n\n\
                         Run `sentinel entity show {}` to see whether the capability that\n\
                         gates this probe is in force, and `sentinel explain probes` for what\n\
                         it would run.",
                        target.canonical_name, target.canonical_name
                    ),
                    None => println!("No observations recorded for this entity yet."),
                }
            } else {
                print!("{}", render_observations(&inventory, &observations));
            }
            Ok(0)
        }
    }
}

/// Whether an entity answers to what the operator typed.
///
/// Accepts an id, a `type/name` pair as used everywhere else (`host/node01`),
/// or a bare name.
///
/// The `type/name` form stopped being a convenience the moment storage domains
/// started being derived: a domain takes the canonical name of the host that
/// serves it, so `david02` now names two entities, and each command silently
/// picked whichever its own ordering put first. `entity show david02` answered
/// about the storage domain while `entity observations david02` answered about
/// the host, which is worse than either answer alone.
fn entity_matches(entity_type: &str, name: &str, id: &str, query: &str) -> bool {
    if id == query {
        return true;
    }
    match query.split_once('/') {
        Some((wanted_type, wanted_name)) => wanted_type == entity_type && wanted_name == name,
        None => name == query,
    }
}

/// Report a reference that names more than one entity.
fn ambiguous(query: &str, candidates: &[String]) -> i32 {
    eprintln!(
        "error: {query:?} names {} entities in this environment:\n",
        candidates.len()
    );
    for candidate in candidates {
        eprintln!("  {candidate}");
    }
    eprintln!(
        "\nName one of them, for example: sentinel entity show {}",
        candidates[0]
    );
    1
}

/// Render observations for a human, resolving observers to names.
///
/// Grouped by probe, because the question being asked is almost always
/// "why does this one disagree with that one", and the answer is the error
/// each observer recorded.
fn render_observations(
    inventory: &crate::inventory::Inventory,
    observations: &[crate::observation::Observation],
) -> String {
    if observations.is_empty() {
        return "No observations recorded for this entity yet.\n".to_string();
    }

    let name_of = |id: crate::entity::EntityId| {
        inventory
            .entities()
            .find(|e| e.id == id)
            .map(|e| e.canonical_name.clone())
            .unwrap_or_else(|| id.to_string())
    };

    let mut out = String::new();
    out.push_str(&format!(
        "{:<19}  {:<18}  {:<14}  {:<12}  {}\n",
        "WHEN", "PROBE", "OBSERVER", "STATUS", "DETAIL"
    ));
    out.push_str(&"\u{2500}".repeat(100));
    out.push('\n');

    for observation in observations {
        let observer = match observation.observer_entity {
            Some(id) => name_of(id),
            // No observer means the target measured itself: a local probe
            // reported by its own agent.
            None => "(itself)".to_string(),
        };

        // What was seen, in the order it is useful: the error if there was
        // one, otherwise where the probe went and what came back.
        let detail = observation
            .error_message
            .clone()
            .or_else(|| observation.error_code.clone())
            .unwrap_or_else(|| {
                let address = observation.payload.get("address").and_then(|v| v.as_str());
                let port = observation.payload.get("port").and_then(|v| v.as_u64());
                let outcome = observation.payload.get("outcome").and_then(|v| v.as_str());
                match (address, port, outcome) {
                    // "ok / refused" reads as a contradiction, and an operator
                    // who reads it as a bug stops trusting the column. Say what
                    // the refusal proved, since that is why it is an ok.
                    (Some(a), Some(p), Some("refused")) => {
                        format!("{a}:{p} refused the connection (so the host answered)")
                    }
                    (Some(a), Some(p), Some(o)) => format!("{a}:{p} {o}"),
                    (Some(a), Some(p), None) => format!("{a}:{p}"),
                    _ => String::new(),
                }
            });

        out.push_str(&format!(
            "{:<19}  {:<18}  {:<14}  {:<12}  {}\n",
            observation.finished_at.format("%m-%d %H:%M:%S"),
            observation.probe_id.as_str(),
            observer,
            observation.status.as_str(),
            detail
        ));
    }
    out
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
mod observation_rendering_tests {
    use super::*;
    use crate::entity::{EntityKey, EntityType, ManagedEntity};
    use crate::observation::{Observation, ProbeStatus};

    fn inventory() -> crate::inventory::Inventory {
        let mut inventory = crate::inventory::Inventory::new();
        for name in ["node01", "node02"] {
            inventory.insert_entity(ManagedEntity::new("lab", EntityType::Host, name));
        }
        inventory
    }

    fn target() -> crate::entity::EntityId {
        EntityKey::new("lab", EntityType::Host, "node01").entity_id()
    }

    fn observer() -> crate::entity::EntityId {
        EntityKey::new("lab", EntityType::Host, "node02").entity_id()
    }

    #[test]
    fn each_line_says_who_saw_what() {
        // The question this exists for: two observers disagreeing about one
        // host. Without the observer on the line there is no way to see that
        // is what is happening.
        let observations = vec![
            Observation::new("network.tcp".into(), target(), ProbeStatus::Ok)
                .with_observer(observer())
                .with_payload(serde_json::json!({"address": "192.0.2.22", "port": 22, "outcome": "connected"})),
            Observation::new("network.tcp".into(), target(), ProbeStatus::Failed)
                .with_error("timed_out", "no answer within 3s"),
        ];

        let text = render_observations(&inventory(), &observations);
        assert!(text.contains("node02"), "{text}");
        assert!(text.contains("192.0.2.22:22 connected"), "{text}");
        assert!(text.contains("no answer within 3s"), "{text}");
    }

    #[test]
    fn a_local_probe_is_marked_as_the_host_measuring_itself() {
        // No observer is not missing information: it means the agent on that
        // host reported it, which is a different kind of evidence from a
        // remote view and should not look like a gap.
        let observations = vec![Observation::new("host.metrics".into(), target(), ProbeStatus::Ok)];
        let text = render_observations(&inventory(), &observations);
        assert!(text.contains("(itself)"), "{text}");
    }

    #[test]
    fn nothing_recorded_says_so_rather_than_printing_a_bare_header() {
        let text = render_observations(&inventory(), &[]);
        assert!(text.contains("No observations"), "{text}");
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
    async fn an_open_incident_is_visible_in_status_even_when_every_entity_is_healthy() {
        // Found on a live cluster: a broken network path between two specific
        // hosts belongs to no entity, so every host read HEALTHY, the summary
        // said "31 healthy", and an open CRITICAL sat in `incident list` that
        // nothing in `status` mentioned. An operator watching `status` had no
        // way to know.
        use crate::diagnosis::{kind, Confidence, Diagnosis};
        use crate::incident::{Incident, Severity};

        let dir = tempfile::tempdir().expect("tempdir");
        let cli = cli_for(&write_config(
            dir.path(),
            "\n[[entities]]\ntype = \"host\"\nname = \"node-a\"\n",
        ));
        discover(&cli, false).await.expect("discover");

        let config = Config::load(&cli.config).expect("config");
        let store = SqliteStore::open(&config.database.path).await.expect("store");
        let node = crate::entity::EntityKey::new("lab", crate::entity::EntityType::Host, "node-a").entity_id();

        let mut incident = Incident::open("cause:path", Severity::Critical);
        incident.add_diagnosis(
            Diagnosis::new(
                kind::PATH_SPECIFIC_NETWORK_FAILURE,
                "reachability.path_failure",
                Confidence::High,
            )
            .with_summary("node-a is unreachable from one observer but reachable from others")
            .rooted_at([node]),
        );
        store.save_incident("lab", &incident).await.expect("save incident");

        let report = status_cmd::load_report(&store, "lab").await.expect("report");
        store.close().await;

        assert!(
            report.entities.iter().all(|e| e.health != "degraded"),
            "the point of this test is that no entity is unhealthy"
        );
        assert_eq!(report.incidents.len(), 1, "{report:#?}");
        assert!(report.incidents[0].summary.contains("unreachable"));
        assert_eq!(report.incidents[0].suspected_root_entities, vec!["node-a"]);
        assert!(!report.is_healthy(), "an open incident is not health");

        let rendered = status_cmd::render(&report);
        assert!(rendered.contains("Open incidents"), "{rendered}");
        assert!(rendered.contains("CRITICAL"), "{rendered}");

        assert_eq!(status(&cli, false).await.expect("status"), 2);
    }

    #[tokio::test]
    async fn a_resolved_incident_does_not_linger_in_status() {
        use crate::incident::{Incident, Severity};

        let dir = tempfile::tempdir().expect("tempdir");
        let cli = cli_for(&write_config(dir.path(), ""));
        discover(&cli, false).await.expect("discover");
        let config = Config::load(&cli.config).expect("config");
        let store = SqliteStore::open(&config.database.path).await.expect("store");

        let mut incident = Incident::open("cause:path", Severity::Critical);
        incident.resolve();
        store.save_incident("lab", &incident).await.expect("save incident");

        let report = status_cmd::load_report(&store, "lab").await.expect("report");
        store.close().await;

        assert!(report.incidents.is_empty(), "{report:#?}");
    }

    #[tokio::test]
    async fn what_a_host_reported_about_its_hardware_is_visible() {
        // Half of every comparison against the scheduler's configuration. It
        // was not shown anywhere, so "Slurm expects 1 GPU, the host reports 0"
        // could not be checked against what the host actually said -- and the
        // obvious command to reach for returned null for everyone, which reads
        // as a fault rather than as a missing field.
        let dir = tempfile::tempdir().expect("tempdir");
        let cli = cli_for(&write_config(
            dir.path(),
            "\n[[entities]]\ntype = \"host\"\nname = \"node-a\"\n",
        ));
        discover(&cli, false).await.expect("discover");

        let config = Config::load(&cli.config).expect("config");
        let store = SqliteStore::open(&config.database.path).await.expect("store");
        let id = crate::entity::EntityKey::new("lab", crate::entity::EntityType::Host, "node-a").entity_id();

        let mut inventory = store.load_inventory("lab").await.expect("inventory");
        let mut entity = inventory.get(id).expect("node-a").clone();
        entity.metadata = serde_json::json!({"hardware": {"cpus": 64, "gpus": serde_json::Value::Null}});
        inventory.insert_entity(entity);
        store.save_inventory(&inventory).await.expect("save");

        let report = status_cmd::load_report(&store, "lab").await.expect("report");
        store.close().await;

        let shown = report.entities.iter().find(|e| e.name == "node-a").expect("node-a");
        let hardware = shown.hardware.as_ref().expect("hardware is reported");
        assert_eq!(hardware["cpus"], 64);

        let rendered = status_cmd::render_entity(shown);
        assert!(rendered.contains("Reported hardware"), "{rendered}");
        assert!(rendered.contains("cpus: 64"), "{rendered}");
        assert!(
            rendered.contains("gpus: (not stated)"),
            "not stated is not zero, and the difference is the whole point: {rendered}"
        );
    }

    #[tokio::test]
    async fn the_agent_version_is_visible_and_skew_is_called_out() {
        // "Did that rollout actually reach the nodes" was not answerable from
        // the CLI. The version has been stored at registration since the first
        // release and displayed nowhere -- which cost an afternoon when an
        // Ansible run reported no changes because it was pinned to the version
        // already installed.
        let dir = tempfile::tempdir().expect("tempdir");
        let cli = cli_for(&write_config(
            dir.path(),
            "\n[[entities]]\ntype = \"host\"\nname = \"node-a\"\n\n[[entities]]\ntype = \"host\"\nname = \"node-b\"\n",
        ));
        discover(&cli, false).await.expect("discover");

        let config = Config::load(&cli.config).expect("config");
        let store = SqliteStore::open(&config.database.path).await.expect("store");
        let mut inventory = store.load_inventory("lab").await.expect("inventory");

        for (name, version) in [("node-a", "0.3.23"), ("node-b", "0.3.22")] {
            let id = crate::entity::EntityKey::new("lab", crate::entity::EntityType::Host, name).entity_id();
            let mut entity = inventory.get(id).expect(name).clone();
            entity.metadata = serde_json::json!({"agent": {"version": version}});
            inventory.insert_entity(entity);
        }
        store.save_inventory(&inventory).await.expect("save");

        let report = status_cmd::load_report(&store, "lab").await.expect("report");
        store.close().await;

        let shown = report.entities.iter().find(|e| e.name == "node-a").expect("node-a");
        assert_eq!(shown.agent_version.as_deref(), Some("0.3.23"));
        assert!(status_cmd::render_entity(shown).contains("0.3.23"));

        assert_eq!(report.agent_versions.len(), 2, "{:?}", report.agent_versions);
        let rendered = status_cmd::render(&report);
        assert!(rendered.contains("バージョンが混在"), "{rendered}");
        assert!(rendered.contains("0.3.22") && rendered.contains("0.3.23"), "{rendered}");
    }

    #[tokio::test]
    async fn a_fleet_on_one_version_says_nothing_about_versions() {
        let dir = tempfile::tempdir().expect("tempdir");
        let cli = cli_for(&write_config(
            dir.path(),
            "\n[[entities]]\ntype = \"host\"\nname = \"node-a\"\n",
        ));
        discover(&cli, false).await.expect("discover");

        let config = Config::load(&cli.config).expect("config");
        let store = SqliteStore::open(&config.database.path).await.expect("store");
        let report = status_cmd::load_report(&store, "lab").await.expect("report");
        store.close().await;

        assert!(!status_cmd::render(&report).contains("バージョンが混在"));
    }

    #[tokio::test]
    async fn a_host_that_reported_no_hardware_shows_no_hardware_section() {
        let dir = tempfile::tempdir().expect("tempdir");
        let cli = cli_for(&write_config(
            dir.path(),
            "\n[[entities]]\ntype = \"host\"\nname = \"node-a\"\n",
        ));
        discover(&cli, false).await.expect("discover");

        let config = Config::load(&cli.config).expect("config");
        let store = SqliteStore::open(&config.database.path).await.expect("store");
        let report = status_cmd::load_report(&store, "lab").await.expect("report");
        store.close().await;

        let shown = report.entities.iter().find(|e| e.name == "node-a").expect("node-a");
        assert!(shown.hardware.is_none());
        assert!(!status_cmd::render_entity(shown).contains("Reported hardware"));
    }

    #[test]
    fn a_reference_can_name_a_type_an_id_or_a_bare_name() {
        assert!(entity_matches("host", "david02", "id-1", "david02"));
        assert!(entity_matches("host", "david02", "id-1", "host/david02"));
        assert!(entity_matches("host", "david02", "id-1", "id-1"));
        assert!(!entity_matches("host", "david02", "id-1", "storage/david02"));
        assert!(!entity_matches("host", "david02", "id-1", "david01"));
    }

    #[tokio::test]
    async fn a_name_shared_by_two_entities_is_refused_rather_than_guessed() {
        // A derived storage domain takes the canonical name of the host that
        // serves it, so a bare name stopped being unique. Each command picked
        // whichever its own ordering put first, which meant `entity show
        // david02` answered about the storage domain while `entity
        // observations david02` answered about the host.
        let dir = tempfile::tempdir().expect("tempdir");
        let cli = cli_for(&write_config(
            dir.path(),
            "\n[[entities]]\ntype = \"host\"\nname = \"fs1\"\n\n[[entities]]\ntype = \"storage\"\nname = \"fs1\"\n",
        ));
        discover(&cli, false).await.expect("discover");

        let ambiguous = EntityCommand::Show {
            name: "fs1".into(),
            json: false,
        };
        assert_eq!(entity(&cli, &ambiguous).await.expect("show"), 1);

        // Qualified, both are reachable and they are different entities.
        for reference in ["host/fs1", "storage/fs1"] {
            let command = EntityCommand::Show {
                name: reference.into(),
                json: false,
            };
            assert_eq!(entity(&cli, &command).await.expect("show"), 0, "{reference}");
        }
    }

    #[tokio::test]
    async fn observations_resolve_a_reference_the_same_way_show_does() {
        // The two disagreeing is what made the bug hard to see.
        let dir = tempfile::tempdir().expect("tempdir");
        let cli = cli_for(&write_config(
            dir.path(),
            "\n[[entities]]\ntype = \"host\"\nname = \"fs1\"\n\n[[entities]]\ntype = \"storage\"\nname = \"fs1\"\n",
        ));
        discover(&cli, false).await.expect("discover");

        let command = EntityCommand::Observations {
            name: "fs1".into(),
            probe: None,
            limit: 40,
            json: false,
        };
        assert_eq!(entity(&cli, &command).await.expect("observations"), 1);

        let qualified = EntityCommand::Observations {
            name: "storage/fs1".into(),
            probe: None,
            limit: 40,
            json: false,
        };
        assert_eq!(entity(&cli, &qualified).await.expect("observations"), 0);
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
