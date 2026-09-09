//! `sentinel notify test` — prove the delivery path works.
//!
//! An operator configures a webhook and then has no way to find out whether it
//! works short of waiting for a real outage, which is the worst possible time
//! to discover a typo in a URL. This sends one synthetic notification to each
//! configured destination and reports what happened to each.
//!
//! It deliberately touches nothing else: no database, no deduplicator, no
//! incident. It answers one question -- can this controller reach the places
//! it is supposed to shout at -- and that question has to be answerable on the
//! real cluster, because a webhook that works from a laptop proves nothing
//! about one that has to leave a management network.

use std::sync::Arc;

use crate::config::Config;
use crate::controller::providers_from_config;
use crate::incident::Severity;
use crate::notification::{Notification, NotificationProvider, Trigger};
use crate::time::now;

use super::Cli;

/// The notification a test sends.
///
/// Unmistakably a test in every field a human or a filter might read. A
/// destination that pages someone must not page them for this, and someone
/// who does see it must know within a second that nothing is wrong.
fn test_notification(severity: Severity) -> Notification {
    Notification {
        incident_id: "00000000-0000-0000-0000-000000000000".to_string(),
        fingerprint: "sentinel-test-notification".to_string(),
        trigger: Trigger::Opened,
        severity,
        title: "[TEST] Cluster Sentinel notification test".to_string(),
        body: "This is a test sent by `sentinel notify test`. \
               No incident exists and nothing is wrong. \
               It confirms that this controller can reach this destination."
            .to_string(),
        recommended_actions: vec!["Nothing. This is a test.".to_string()],
        created_at: now(),
    }
}

/// Run `sentinel notify test`.
pub async fn test(cli: &Cli, provider_name: Option<&str>, severity: Option<&str>) -> anyhow::Result<i32> {
    let config = Config::load(&cli.config)?;

    let severity = match severity {
        Some(text) => Severity::parse(text)
            .ok_or_else(|| anyhow::anyhow!("unknown severity {text:?}; expected info, warning or critical"))?,
        None => Severity::Warning,
    };

    let providers: Vec<Arc<dyn NotificationProvider>> = providers_from_config(&config)
        .into_iter()
        .filter(|p| provider_name.is_none_or(|name| p.name() == name))
        .collect();

    if providers.is_empty() {
        match provider_name {
            Some(name) => {
                eprintln!(
                    "error: no notification destination named {name:?} in {}",
                    cli.config.display()
                );
            }
            None => {
                eprintln!(
                    "error: no notification destinations are configured.\n\
                     Add one to {} and try again:\n\n\
                    \x20   [[notification.webhooks]]\n\
                    \x20   name = \"ops\"\n\
                    \x20   url  = \"https://example.invalid/hook\"",
                    cli.config.display()
                );
            }
        }
        return Ok(1);
    }

    // Sent below the configured floor on purpose: the floor decides what is
    // worth waking someone for, and a test is not that. What is being checked
    // is whether the destination is reachable at all.
    let notification = test_notification(severity);
    let mut failed = 0;

    for provider in &providers {
        match provider.send(&notification).await {
            Ok(()) => println!("{:<20} sent", provider.name()),
            Err(error) => {
                println!("{:<20} FAILED: {error}", provider.name());
                failed += 1;
            }
        }
    }

    println!();
    if failed == 0 {
        println!(
            "{} destination(s) reached. Check that the message arrived: delivery \
             says the endpoint accepted it, not that a human will see it.",
            providers.len()
        );
        Ok(0)
    } else {
        println!("{failed} of {} destination(s) failed.", providers.len());
        Ok(2)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_message_says_it_is_a_test_everywhere_a_human_looks() {
        // A destination that pages someone must not page them for this, and
        // whoever does see it must know within a second that nothing is wrong.
        let notification = test_notification(Severity::Warning);
        assert!(notification.title.contains("TEST"));
        assert!(notification.body.contains("test"));
        assert!(notification.body.contains("nothing is wrong"));
    }

    #[test]
    fn its_fingerprint_cannot_collide_with_a_real_incident() {
        // The fingerprint is the deduplication scope. One that collided would
        // silence a real incident, which is the opposite of what a test is for.
        let notification = test_notification(Severity::Critical);
        assert_eq!(notification.fingerprint, "sentinel-test-notification");
        assert!(notification.incident_id.chars().all(|c| c == '0' || c == '-'));
    }
}
