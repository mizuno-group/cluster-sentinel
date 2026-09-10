//! `sentinel audit` — probes that should be reporting and are not.
//!
//! Rendered grouped by probe rather than by host, because the two shapes mean
//! different things and only one of them is easy to miss. One host quiet on
//! many probes is that host's problem, and `status` already says so. **One
//! probe quiet on every host is a probe that is not wired up**, and nothing
//! else in the system will ever mention it.

use crate::audit::{silence_horizon, Finding};
use crate::config::Config;

use super::{run_cmd::open_store, Cli};

/// `sentinel audit`.
pub async fn audit(cli: &Cli, json: bool) -> anyhow::Result<i32> {
    let config = Config::load(&cli.config)?;
    let store = open_store(&config).await?;
    let inventory = store.load_inventory(&config.environment).await?;
    let last_seen = store.probe_last_seen(&config.environment).await?;
    let observed = crate::audit::observed_entities(&inventory, &config);
    let findings = crate::audit::silent_probes(&inventory, &config, &observed, &last_seen, crate::time::now());

    if json {
        println!("{}", serde_json::to_string_pretty(&as_json(&findings))?);
    } else {
        print!("{}", render(&findings));
    }

    // Non-zero so this can sit in cron or CI. A probe that is not running is a
    // hole in the monitoring, and a hole in the monitoring is worth the same
    // attention as a fault it would have found.
    Ok(if findings.is_empty() { 0 } else { 2 })
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
