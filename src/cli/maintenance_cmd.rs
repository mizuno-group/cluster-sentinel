//! `sentinel maintenance` — declare planned work, so it does not page anyone.
//!
//! Suppression is of **notification only**. Probes keep running, state keeps
//! changing and diagnosis keeps concluding; the record of what happened during
//! the window stays complete and readable afterwards. A window that made the
//! cluster *look* healthy would hide a genuine fault that began during it, and
//! leave nobody able to say when it started.
//!
//! The alternative operators reach for otherwise is stopping the controller,
//! which loses exactly the history that makes the post-mortem possible.

use crate::config::Config;
use crate::entity::EntityId;
use crate::notification::MaintenanceWindow;
use crate::time::{now, to_rfc3339};

use super::{run_cmd::open_store, Cli, MaintenanceCommand};

/// `sentinel maintenance ...`.
pub async fn run(cli: &Cli, command: &MaintenanceCommand) -> anyhow::Result<i32> {
    let config = Config::load(&cli.config)?;
    let store = open_store(&config).await?;

    match command {
        MaintenanceCommand::Start {
            target,
            reason,
            duration,
            json,
        } => start(&store, &config, target.as_deref(), reason, duration.as_deref(), *json).await,
        MaintenanceCommand::List { all, json } => list(&store, &config, *all, *json).await,
        MaintenanceCommand::End { id, json } => end(&store, &config, id, *json).await,
    }
}

async fn start(
    store: &crate::persistence::SqliteStore,
    config: &Config,
    target: Option<&str>,
    reason: &str,
    duration: Option<&str>,
    json: bool,
) -> anyhow::Result<i32> {
    let mut window = match target {
        Some(name) => {
            let inventory = store.load_inventory(&config.environment).await?;
            let matches: Vec<&crate::entity::ManagedEntity> =
                inventory.entities().filter(|e| matches_target(e, name)).collect();

            match matches.as_slice() {
                [one] => MaintenanceWindow::for_entity(one.id, reason),
                [] => {
                    eprintln!(
                        "error: no entity named {name:?} in environment {:?}",
                        config.environment
                    );
                    eprintln!("\nsentinel status lists what is known.");
                    return Ok(1);
                }
                many => {
                    eprintln!("error: {name:?} names {} entities:\n", many.len());
                    for entity in many {
                        eprintln!("  {}/{}", entity.entity_type, entity.canonical_name);
                    }
                    eprintln!("\nQualify it as <type>/<name>.");
                    return Ok(1);
                }
            }
        }
        None => MaintenanceWindow::for_environment(reason),
    };

    if let Some(text) = duration {
        let span = humantime::parse_duration(text).map_err(|e| anyhow::anyhow!("--for {text:?}: {e}"))?;
        window = window.until(now() + chrono::Duration::from_std(span)?);
    }
    if let Some(actor) = declaring_user() {
        window = window.by(actor);
    }

    store.save_maintenance_window(&config.environment, &window).await?;

    if json {
        println!("{}", serde_json::to_string_pretty(&window)?);
    } else {
        let scope = match target {
            Some(name) => name.to_string(),
            None => format!("the whole {} environment", config.environment),
        };
        println!("Maintenance started on {scope}.");
        println!("  reason   {}", window.reason);
        println!(
            "  until    {}",
            match window.ends_at {
                // An open-ended window is the honest default -- nobody knows
                // how long a disk swap takes -- but it is also the one that
                // can be forgotten, so say how it ends in the same breath.
                Some(at) => to_rfc3339(at),
                None => "open-ended — ends when you run the command below".to_string(),
            }
        );
        println!("  id       {}", window.id);
        println!("\nNotifications about it are suppressed. Observation, state and");
        println!("diagnosis continue, so the history stays complete.");
        println!("\n  sentinel maintenance end {}", window.id);
    }

    Ok(0)
}

async fn list(store: &crate::persistence::SqliteStore, config: &Config, all: bool, json: bool) -> anyhow::Result<i32> {
    let inventory = store.load_inventory(&config.environment).await?;
    let windows = store.load_maintenance_windows(&config.environment).await?;
    let at = now();
    let shown: Vec<&MaintenanceWindow> = windows.iter().filter(|w| all || w.is_active_at(at)).collect();

    let name_of = |id: Option<EntityId>| match id {
        Some(id) => inventory
            .get(id)
            .map(|e| format!("{}/{}", e.entity_type, e.canonical_name))
            .unwrap_or_else(|| id.to_string()),
        None => format!("(all of {})", config.environment),
    };

    if json {
        let rows: Vec<serde_json::Value> = shown
            .iter()
            .map(|w| {
                serde_json::json!({
                    "id": w.id,
                    "entity": w.entity.map(|id| id.to_string()),
                    "entity_name": name_of(w.entity),
                    "reason": w.reason,
                    "starts_at": to_rfc3339(w.starts_at),
                    "ends_at": w.ends_at.map(to_rfc3339),
                    "active": w.is_active_at(at),
                })
            })
            .collect();
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({ "windows": rows }))?
        );
        return Ok(0);
    }

    if shown.is_empty() {
        println!("No {}maintenance windows.", if all { "" } else { "active " });
        if !all && !windows.is_empty() {
            println!("\n{} past one(s); --all to see them.", windows.len());
        }
        return Ok(0);
    }

    println!("{:<38} {:<24} UNTIL", "ID", "UNDER MAINTENANCE");
    for window in shown {
        let until = match window.ends_at {
            Some(at) => to_rfc3339(at),
            None => "open-ended".to_string(),
        };
        // `to_string` because uuid's Display writes straight to the formatter
        // and ignores the width, which silently broke the column.
        println!("{:<38} {:<24} {}", window.id.to_string(), name_of(window.entity), until);
        println!("{:<38} {}", "", window.reason);
    }

    Ok(0)
}

async fn end(store: &crate::persistence::SqliteStore, config: &Config, id: &str, json: bool) -> anyhow::Result<i32> {
    let windows = store.load_maintenance_windows(&config.environment).await?;
    let at = now();

    // Accept a prefix, because the full UUID is what start printed and nobody
    // retypes one of those correctly.
    let matches: Vec<&MaintenanceWindow> = windows
        .iter()
        .filter(|w| w.id.to_string().starts_with(id) && w.is_active_at(at))
        .collect();

    let window = match matches.as_slice() {
        [one] => *one,
        [] => {
            eprintln!("error: no active maintenance window matching {id:?}");
            eprintln!("\n  sentinel maintenance list");
            return Ok(1);
        }
        many => {
            eprintln!("error: {id:?} matches {} windows; give more of the id.", many.len());
            return Ok(1);
        }
    };

    store.end_maintenance_window(window.id, at).await?;

    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({ "id": window.id, "ended_at": to_rfc3339(at) }))?
        );
    } else {
        println!("Maintenance ended: {}", window.reason);
        println!("\nNotifications resume on the next diagnosis pass. Anything still");
        println!("broken will be announced then — including faults that began during");
        println!("the window, which were diagnosed all along and simply not sent.");
    }

    Ok(0)
}

fn matches_target(entity: &crate::entity::ManagedEntity, query: &str) -> bool {
    if entity.id.to_string() == query {
        return true;
    }
    match query.split_once('/') {
        Some((entity_type, name)) => entity.entity_type.to_string() == entity_type && entity.canonical_name == name,
        None => entity.canonical_name == query,
    }
}

/// Who is declaring the window, for the record.
///
/// `SUDO_USER` first: the command needs the service user's database, so it is
/// usually run through `sudo -u sentinel`, and recording "sentinel" would name
/// the same account every time and say nothing.
fn declaring_user() -> Option<String> {
    std::env::var("SUDO_USER")
        .or_else(|_| std::env::var("USER"))
        .ok()
        .filter(|name| !name.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_target_matches_a_bare_name_or_a_qualified_one() {
        let entity = crate::entity::ManagedEntity::new("lab", crate::entity::EntityType::Host, "fs1");
        assert!(matches_target(&entity, "fs1"));
        assert!(matches_target(&entity, "host/fs1"));
        assert!(matches_target(&entity, &entity.id.to_string()));
        assert!(!matches_target(&entity, "service/fs1"));
        assert!(!matches_target(&entity, "fs2"));
    }

    #[test]
    fn a_window_with_a_duration_ends_by_itself() {
        // The failure mode this guards against is a window that outlives the
        // work and silences a real fault a week later.
        let span = humantime::parse_duration("2h").expect("duration");
        let window = MaintenanceWindow::for_entity(
            crate::entity::EntityKey::new("lab", crate::entity::EntityType::Host, "fs1").entity_id(),
            "disk swap",
        )
        .until(now() + chrono::Duration::from_std(span).expect("span"));

        assert!(window.is_active_at(now() + chrono::Duration::hours(1)));
        assert!(!window.is_active_at(now() + chrono::Duration::hours(3)));
    }
}
