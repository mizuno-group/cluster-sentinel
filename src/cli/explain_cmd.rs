//! `sentinel explain` — how this cluster is being watched, and by what.
//!
//! Written for the person who did not build it. `status` says what Sentinel
//! concluded and `entity show` says what it concluded about one host, but
//! neither says *how* any of it is known: which capability enables which
//! probe, what that probe actually runs, and which machine runs it against
//! which other machine.
//!
//! Without that, the output has to be taken on trust, and a monitoring system
//! that has to be taken on trust is one nobody can correct.

use crate::capability::catalog as capabilities;
use crate::config::Config;
use crate::controller::endpoint_for;
use crate::entity::{EntityId, EntityType, ManagedEntity};
use crate::inventory::Inventory;
use crate::probes::catalog as probes;
use crate::probes::ExecutionMode;

use super::{run_cmd::open_store, Cli};

/// Render the capability table: what each means and how it is decided.
pub fn render_capabilities() -> String {
    let mut out = String::from("CAPABILITIES — what enables a probe, and how it is decided\n");
    out.push_str(&"\u{2500}".repeat(100));
    out.push('\n');

    for entry in capabilities::catalog() {
        let enabled_by: Vec<&str> = probes::catalog()
            .iter()
            .filter(|p| {
                p.definition
                    .required_capabilities
                    .iter()
                    .any(|c| c.as_str() == entry.name)
            })
            .map(|p| p.id())
            .collect::<Vec<_>>()
            .iter()
            .map(|s| Box::leak(s.to_string().into_boxed_str()) as &str)
            .collect();

        out.push_str(&format!("\n{}\n", entry.name));
        out.push_str(&format!("  meaning    {}\n", entry.meaning));
        out.push_str(&format!("  detected   {}\n", entry.detection));
        out.push_str(&format!(
            "  enables    {}\n",
            if enabled_by.is_empty() {
                "(no probe; used by diagnosis rules or peer assignment)".to_string()
            } else {
                enabled_by.join(", ")
            }
        ));
    }
    out
}

/// Render the probe table: what each runs, how often, and from where.
pub fn render_probes(config: &Config) -> String {
    let mut out = String::from("PROBES — what is actually run\n");
    out.push_str(&"\u{2500}".repeat(100));
    out.push('\n');

    for entry in probes::catalog() {
        let mut definition = entry.definition.clone();
        config.probes.apply(&mut definition);
        let enabled = config.probes.is_enabled(definition.id.as_str());

        let where_run = match definition.execution_mode {
            ExecutionMode::Local => "on the host itself, by its agent",
            ExecutionMode::Remote => "from another host only",
            ExecutionMode::Either => "by the controller and by peer observers",
        };
        let gate = if definition.required_capabilities.is_empty() {
            "(none — applies to every host with an address)".to_string()
        } else {
            definition
                .required_capabilities
                .iter()
                .map(|c| c.as_str().to_string())
                .collect::<Vec<_>>()
                .join(", ")
        };

        out.push_str(&format!(
            "\n{}{}\n",
            definition.id,
            if enabled { "" } else { "   [DISABLED by configuration]" }
        ));
        out.push_str(&format!("  measures   {}\n", entry.description));
        out.push_str(&format!("  runs       {}\n", entry.mechanism));
        out.push_str(&format!("  needs      {gate}\n"));
        out.push_str(&format!("  where      {where_run}\n"));
        out.push_str(&format!(
            "  cadence    every {}, timeout {}\n",
            crate::time::format_duration(definition.interval),
            crate::time::format_duration(definition.timeout)
        ));
        if let Some(caution) = entry.caution {
            out.push_str(&format!("  note       {caution}\n"));
        }
    }
    out
}

/// Render the monitoring graph: who watches whom, with what, at which address.
pub fn render_paths(inventory: &Inventory, plan: &crate::controller::AssignmentPlan) -> String {
    let name_of = |id: EntityId| {
        inventory
            .entities()
            .find(|e| e.id == id)
            .map(|e| e.canonical_name.clone())
            .unwrap_or_else(|| id.to_string())
    };

    let mut out = String::from("MONITORING PATHS — who watches whom\n");
    out.push_str(&"\u{2500}".repeat(100));
    out.push('\n');
    out.push_str(
        "\nEach host is watched from two directions: its own agent measures what only\n\
         a local process can see, and other hosts check that it answers at all. A\n\
         host is never called unreachable on one observer's word (SPEC.md §50).\n",
    );

    let hosts: Vec<&ManagedEntity> = inventory
        .entities()
        .filter(|e| e.entity_type == EntityType::Host)
        .collect();

    for host in hosts {
        let endpoint = endpoint_for(host);
        out.push_str(&format!("\n{}\n", host.canonical_name));
        out.push_str(&format!(
            "  reached at   {}\n",
            endpoint
                .as_ref()
                .map(|e| e.address.clone())
                .unwrap_or_else(|| "(unknown — no address and no usable name)".into())
        ));

        // What the agent on this host runs. `Either` probes are not in this
        // list: they are run *at* the host from elsewhere, which is a
        // different claim -- an agent asking itself whether it answers can
        // only say yes.
        let applies = |mode: ExecutionMode, p: &probes::CatalogEntry| {
            p.definition.execution_mode == mode && p.definition.applies_to(EntityType::Host, &host.capabilities)
        };
        let names = |list: Vec<String>| {
            if list.is_empty() {
                None
            } else {
                Some(list.join(", "))
            }
        };

        let local = names(
            probes::catalog()
                .iter()
                .filter(|p| applies(ExecutionMode::Local, p))
                .map(|p| p.id().to_string())
                .collect(),
        );
        let remote = names(
            probes::catalog()
                .iter()
                .filter(|p| {
                    (applies(ExecutionMode::Either, p) || applies(ExecutionMode::Remote, p)) && endpoint.is_some()
                })
                .map(|p| p.id().to_string())
                .collect(),
        );

        out.push_str(&format!(
            "  from itself  {}\n",
            local.unwrap_or_else(|| "(nothing — no agent has registered here)".into())
        ));
        out.push_str(&format!(
            "  from others  {}\n",
            remote.unwrap_or_else(|| "(nothing — no address is known)".into())
        ));

        let observers: Vec<String> = plan
            .assignments
            .iter()
            .find(|a| a.target == host.id)
            .map(|a| {
                a.observers
                    .iter()
                    .map(|o| format!("{} ({:?})", name_of(o.entity), o.role))
                    .collect()
            })
            .unwrap_or_default();
        out.push_str(&format!(
            "  watched by   {}\n",
            if observers.is_empty() {
                "(nobody — reachability cannot be diagnosed for this host)".to_string()
            } else {
                observers.join(", ")
            }
        ));
    }

    out
}

/// Run `sentinel explain`.
pub async fn run(cli: &Cli, topic: Option<&str>) -> anyhow::Result<i32> {
    let config = Config::load(&cli.config)?;

    let want = |name: &str| topic.is_none() || topic == Some(name);
    let mut printed = false;

    if want("capabilities") {
        print!("{}", render_capabilities());
        printed = true;
    }
    if want("probes") {
        if printed {
            println!();
        }
        print!("{}", render_probes(&config));
        printed = true;
    }
    if want("paths") {
        // Only this one needs the database: the other two describe the binary
        // and the configuration, and are answerable anywhere.
        let store = open_store(&config).await?;
        let controller = crate::controller::Controller::new(config.clone(), store).await?;
        let inventory = controller.store().load_inventory(&config.environment).await?;
        let plan = controller.assignment_plan().await?;
        if printed {
            println!();
        }
        print!("{}", render_paths(&inventory, &plan));
        printed = true;
    }

    if !printed {
        eprintln!("error: unknown topic {topic:?}; expected capabilities, probes or paths");
        return Ok(1);
    }
    Ok(0)
}
