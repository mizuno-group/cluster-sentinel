//! `sentinel audit` — probes that should be reporting and are not.
//!
//! Rendered grouped by probe rather than by host, because the two shapes mean
//! different things and only one of them is easy to miss. One host quiet on
//! many probes is that host's problem, and `status` already says so. **One
//! probe quiet on every host is a probe that is not wired up**, and nothing
//! else in the system will ever mention it.

use crate::audit::{silence_horizon, Finding};
use crate::config::Config;
use crate::notification::MaintenanceWindow;

use super::{run_cmd::open_store, Cli};

/// How long an open-ended maintenance window may stand before the audit treats
/// it as forgotten.
///
/// Longer than any single session at a rack, short enough that a window left
/// over from yesterday's work is caught before it eats a real outage. A window
/// with an end time is never reported, however old, because it closes itself.
const STALE_MAINTENANCE_HOURS: i64 = 24;

/// `sentinel audit`.
pub async fn audit(cli: &Cli, json: bool) -> anyhow::Result<i32> {
    let config = Config::load(&cli.config)?;
    let store = open_store(&config).await?;
    let inventory = store.load_inventory(&config.environment).await?;
    let last_seen = store.probe_last_seen(&config.environment).await?;
    let observed = crate::audit::observed_entities(&inventory, &config);
    let findings = crate::audit::silent_probes(&inventory, &config, &observed, &last_seen, crate::time::now());

    // A maintenance window nobody closed is the same class of defect as a probe
    // nobody runs: the monitoring is intact and its output goes nowhere. It
    // belongs here rather than only in `status` because this is the command
    // that runs unattended, and being forgotten is the whole failure mode.
    let windows = store.load_maintenance_windows(&config.environment).await?;
    let forgotten = forgotten_maintenance(&windows, crate::time::now());
    let named: Vec<(String, String)> = forgotten
        .iter()
        .map(|w| {
            let what = match w.entity {
                Some(id) => inventory
                    .get(id)
                    .map(|e| e.canonical_name.clone())
                    .unwrap_or_else(|| id.to_string()),
                None => format!("(all of {})", config.environment),
            };
            (what, w.reason.clone())
        })
        .collect();

    if json {
        let mut value = as_json(&findings);
        value["forgotten_maintenance"] = serde_json::json!(forgotten
            .iter()
            .map(|w| serde_json::json!({
                "id": w.id,
                "entity": w.entity.map(|id| id.to_string()),
                "reason": w.reason,
                "started_at": crate::time::to_rfc3339(w.starts_at),
            }))
            .collect::<Vec<_>>());
        println!("{}", serde_json::to_string_pretty(&value)?);
    } else {
        print!("{}", render(&findings));
        print!("{}", render_maintenance(&named, &forgotten));
    }

    // Non-zero so this can sit in cron or CI. A probe that is not running is a
    // hole in the monitoring, and a hole in the monitoring is worth the same
    // attention as a fault it would have found.
    Ok(if findings.is_empty() && forgotten.is_empty() {
        0
    } else {
        2
    })
}

/// Open-ended maintenance windows old enough to look forgotten.
fn forgotten_maintenance(windows: &[MaintenanceWindow], now: crate::time::Timestamp) -> Vec<&MaintenanceWindow> {
    let cutoff = now - chrono::Duration::hours(STALE_MAINTENANCE_HOURS);
    windows
        .iter()
        .filter(|w| w.is_active_at(now) && w.ends_at.is_none() && w.starts_at < cutoff)
        .collect()
}

/// Report windows that are suppressing notifications with no end in sight.
fn render_maintenance(named: &[(String, String)], windows: &[&MaintenanceWindow]) -> String {
    if named.is_empty() {
        return String::new();
    }

    let mut out = String::from("\nMAINTENANCE STILL SUPPRESSING NOTIFICATIONS\n");
    out.push_str(&"\u{2500}".repeat(100));
    out.push('\n');
    out.push_str(&format!(
        "\nDeclared more than {STALE_MAINTENANCE_HOURS}h ago with no end time. Until each is ended,\n         faults affecting it are diagnosed and recorded but never announced.\n\n"
    ));

    for ((what, reason), window) in named.iter().zip(windows) {
        out.push_str(&format!("  {what}\n"));
        out.push_str(&format!("    reason  {reason}\n"));
        out.push_str(&format!(
            "    since   {}{}\n",
            crate::time::to_rfc3339(window.starts_at),
            match &window.created_by {
                Some(who) => format!(" by {who}"),
                None => String::new(),
            }
        ));
        out.push_str(&format!("    end it  sentinel maintenance end {}\n", window.id));
    }

    out
}

fn as_json(findings: &[Finding]) -> serde_json::Value {
    serde_json::json!({
        "silent": findings
            .iter()
            .map(|f| serde_json::json!({
                "probe": f.probe,
                "entity": f.entity.to_string(),
                "entity_name": f.entity_name,
                "runs": f.expectation.as_str(),
                "interval_seconds": f.interval.as_secs(),
                "last_seen": f.last_seen.map(crate::time::to_rfc3339),
                "never_observed": f.never_observed(),
            }))
            .collect::<Vec<_>>(),
        "silent_count": findings.len(),
        "never_observed_count": findings.iter().filter(|f| f.never_observed()).count(),
    })
}

/// Render the findings for a human.
pub fn render(findings: &[Finding]) -> String {
    let mut out = String::from("PROBE AUDIT — what should be reporting and is not\n");
    out.push_str(&"\u{2500}".repeat(100));
    out.push('\n');

    if findings.is_empty() {
        out.push_str(
            "\nEvery probe that applies to an entity has reported within its own\n\
             schedule. Nothing is silently switched off.\n",
        );
        return out;
    }

    out.push_str(
        "\nA probe listed here is enabled, applies to the entity, and something is\n\
         supposed to be running it. Silence is not a passing result: a probe that\n\
         never runs produces no observation, so nothing fails and nothing is\n\
         diagnosed, and the entity reads healthy because nothing ever said\n\
         otherwise.\n",
    );

    let mut current = "";
    for finding in findings {
        if finding.probe != current {
            current = &finding.probe;
            let same: Vec<&Finding> = findings.iter().filter(|f| f.probe == current).collect();
            let never = same.iter().filter(|f| f.never_observed()).count();

            out.push_str(&format!("\n{current}\n"));
            out.push_str(&format!(
                "  expected     every {}, run {}\n",
                humantime::format_duration(finding.interval),
                finding.expectation.as_str()
            ));
            out.push_str(&format!(
                "  quiet on     {} entit{}\n",
                same.len(),
                if same.len() == 1 { "y" } else { "ies" }
            ));
            if never == same.len() {
                out.push_str("  **never observed anywhere it applies — most likely nothing runs it**\n");
            } else if never > 0 {
                out.push_str(&format!("  never ran on {never} of them\n"));
            }
            out.push_str(&format!(
                "  reported if  nothing arrives for {}\n",
                humantime::format_duration(silence_horizon(finding.interval))
            ));
        }

        let when = match finding.last_seen {
            Some(at) => format!("last {}", crate::time::to_rfc3339(at)),
            None => "never".to_string(),
        };
        out.push_str(&format!("    {:<24} {}\n", finding.entity_name, when));
    }

    out.push_str("\nWhat to check, in order:\n");
    out.push_str("  sentinel entity show <name>      the capability that gates it is in force?\n");
    out.push_str("  sentinel explain probes          what it would run, and how often\n");
    out.push_str("  sentinel doctor                  on that host: why it decided what it did\n");

    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audit::Expectation;
    use crate::entity::{EntityKey, EntityType};
    use std::time::Duration;

    fn finding(probe: &str, name: &str, last_seen: Option<crate::time::Timestamp>) -> Finding {
        Finding {
            probe: probe.to_string(),
            entity: EntityKey::new("lab", EntityType::Host, name).entity_id(),
            entity_name: name.to_string(),
            expectation: Expectation::Locally,
            interval: Duration::from_secs(60),
            last_seen,
        }
    }

    fn window(hours_ago: i64, ends: bool) -> MaintenanceWindow {
        let started = crate::time::now() - chrono::Duration::hours(hours_ago);
        let w = MaintenanceWindow::for_entity(EntityKey::new("lab", EntityType::Host, "fs1").entity_id(), "disk swap")
            .from(started);
        if ends {
            w.until(started + chrono::Duration::hours(2))
        } else {
            w
        }
    }

    #[test]
    fn a_maintenance_window_left_open_for_a_day_is_reported() {
        // The failure mode: somebody declares a window for an afternoon's work,
        // the work ends, the window does not, and a real outage next week is
        // diagnosed and silently dropped.
        let windows = vec![window(30, false)];
        let forgotten = forgotten_maintenance(&windows, crate::time::now());
        assert_eq!(forgotten.len(), 1);

        let text = render_maintenance(&[("fs1".into(), "disk swap".into())], &forgotten);
        assert!(text.contains("STILL SUPPRESSING"), "{text}");
        assert!(text.contains("sentinel maintenance end"), "{text}");
    }

    #[test]
    fn a_window_declared_this_morning_is_not_yet_a_problem() {
        let windows = vec![window(3, false)];
        assert!(forgotten_maintenance(&windows, crate::time::now()).is_empty());
    }

    #[test]
    fn a_window_with_an_end_time_is_never_reported() {
        // It closes itself, so it cannot be forgotten however old it is.
        let windows = vec![window(500, true)];
        assert!(forgotten_maintenance(&windows, crate::time::now()).is_empty());
    }

    #[test]
    fn nothing_forgotten_prints_nothing() {
        assert!(render_maintenance(&[], &[]).is_empty());
    }

    #[test]
    fn a_clean_audit_says_so_plainly() {
        let text = render(&[]);
        assert!(text.contains("Nothing is silently switched off"), "{text}");
    }

    #[test]
    fn a_probe_missing_everywhere_is_called_out_as_such() {
        // The shape that matters. One host quiet on many probes is that host's
        // problem; one probe quiet on every host is a probe nothing runs, and
        // it is the one nothing else in the system will mention.
        let findings = vec![
            finding("nfs.server.exports", "fs1", None),
            finding("nfs.server.exports", "fs2", None),
        ];
        let text = render(&findings);

        assert!(text.contains("never observed anywhere it applies"), "{text}");
        assert!(text.contains("quiet on     2 entities"), "{text}");
        assert!(text.contains("fs1") && text.contains("fs2"), "{text}");
    }

    #[test]
    fn a_probe_that_stopped_on_one_host_is_not_described_as_never_run() {
        let findings = vec![finding(
            "host.metrics",
            "node01",
            Some(crate::time::now() - chrono::Duration::hours(2)),
        )];
        let text = render(&findings);

        assert!(!text.contains("never observed anywhere"), "{text}");
        assert!(text.contains("last "), "{text}");
    }

    #[test]
    fn a_mixed_probe_says_how_many_never_ran() {
        let findings = vec![
            finding("host.metrics", "node01", None),
            finding(
                "host.metrics",
                "node02",
                Some(crate::time::now() - chrono::Duration::hours(2)),
            ),
        ];
        let text = render(&findings);

        assert!(text.contains("never ran on 1 of them"), "{text}");
        assert!(!text.contains("never observed anywhere"), "{text}");
    }

    #[test]
    fn the_report_says_what_to_do_next() {
        let text = render(&[finding("nfs.server.exports", "fs1", None)]);
        assert!(text.contains("sentinel entity show"), "{text}");
        assert!(text.contains("sentinel doctor"), "{text}");
    }
}
