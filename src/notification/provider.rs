//! Where notifications go.
//!
//! A provider abstraction rather than a hard-coded destination, so a
//! deployment can point at whatever it already runs (SPEC.md §112). The MVP
//! ships a generic webhook, which covers ntfy, Gotify, Slack, Discord and
//! anything else that accepts a POST.

use std::time::Duration;

use async_trait::async_trait;
use thiserror::Error;

use super::Notification;
use crate::config::WebhookFormat;
use crate::incident::Severity;

/// Why a notification could not be delivered.
#[derive(Debug, Error)]
pub enum ProviderError {
    /// The destination could not be reached.
    #[error("cannot reach {destination}: {detail}")]
    Unreachable {
        /// Where it was trying to send.
        destination: String,
        /// What went wrong.
        detail: String,
    },
    /// The destination refused the message.
    #[error("{destination} returned {status}: {detail}")]
    Rejected {
        /// Where it was trying to send.
        destination: String,
        /// HTTP status.
        status: u16,
        /// Body, or an extract.
        detail: String,
    },
    /// The provider could not be built.
    #[error("provider misconfigured: {0}")]
    Misconfigured(String),
}

impl ProviderError {
    /// Whether retrying later might work.
    pub fn is_transient(&self) -> bool {
        match self {
            ProviderError::Unreachable { .. } => true,
            ProviderError::Rejected { status, .. } => *status >= 500 || *status == 429,
            ProviderError::Misconfigured(_) => false,
        }
    }
}

/// Somewhere notifications can be sent.
#[async_trait]
pub trait NotificationProvider: Send + Sync {
    /// Provider name, used for deduplication scope and in logs.
    fn name(&self) -> &str;

    /// Deliver one notification.
    async fn send(&self, notification: &Notification) -> Result<(), ProviderError>;
}

/// Posts JSON to a URL.
#[derive(Debug, Clone)]
pub struct WebhookProvider {
    name: String,
    url: String,
    format: WebhookFormat,
    http: reqwest::Client,
}

/// Slack's limit on a `header` block, which it rejects rather than truncates.
const SLACK_HEADER_LIMIT: usize = 150;
/// Slack's limit on a `section` text block.
const SLACK_SECTION_LIMIT: usize = 3000;

/// Cut to a limit on a character boundary, marking that something was cut.
fn truncated(text: &str, limit: usize) -> String {
    if text.chars().count() <= limit {
        return text.to_string();
    }
    let mut out: String = text.chars().take(limit.saturating_sub(1)).collect();
    out.push('\u{2026}');
    out
}

impl WebhookProvider {
    /// Build a webhook provider.
    pub fn new(name: impl Into<String>, url: impl Into<String>, timeout: Duration) -> Result<Self, ProviderError> {
        let url = url.into();
        if !url.starts_with("http://") && !url.starts_with("https://") {
            return Err(ProviderError::Misconfigured(format!("{url} is not an http(s) URL")));
        }

        let http = reqwest::Client::builder()
            .timeout(timeout)
            .build()
            .map_err(|e| ProviderError::Misconfigured(e.to_string()))?;

        Ok(Self {
            name: name.into(),
            url,
            format: WebhookFormat::default(),
            http,
        })
    }

    /// Builder: shape the payload for a particular service.
    pub fn with_format(mut self, format: WebhookFormat) -> Self {
        self.format = format;
        self
    }

    /// The destination URL.
    pub fn url(&self) -> &str {
        &self.url
    }

    /// The whole notification as one block of readable text.
    ///
    /// Carried alongside the structured fields because the destinations people
    /// actually use want a message, not a schema: Slack refuses a payload with
    /// no `text` outright (`missing_text_or_fallback_or_attachments`), and
    /// Discord and Teams want the same thing under their own names. A receiver
    /// that parses the structured fields simply ignores these.
    fn message(notification: &Notification) -> String {
        let mut text = format!("{}\n\n{}", notification.title, notification.body);
        if !notification.recommended_actions.is_empty() {
            // The body may or may not end in a newline depending on what built
            // it, and a heading glued to the end of a sentence reads as a typo.
            if !text.ends_with("\n\n") {
                text.push_str(if text.ends_with('\n') { "\n" } else { "\n\n" });
            }
            text.push_str("Recommended actions:\n");
            for action in &notification.recommended_actions {
                text.push_str(&format!("  - {action}\n"));
            }
        }
        text
    }

    /// Colour and icon for a notification, by what it is telling you.
    ///
    /// Colour is the thing a person reads before any words, so it carries the
    /// one distinction that matters at a glance: is this starting or ending.
    /// A recovery is green whatever its severity.
    fn appearance(notification: &Notification) -> (&'static str, &'static str) {
        if notification.trigger.is_recovery() {
            return ("#2e7d32", "\u{2705}");
        }
        match notification.severity {
            Severity::Critical => ("#d32f2f", "\u{1f534}"),
            Severity::Warning => ("#f9a825", "\u{1f7e1}"),
            Severity::Info => ("#546e7a", "\u{1f535}"),
        }
    }

    /// The summary without the severity word the heading already carries.
    ///
    /// Titles are built as `SEVERITY: summary`. Repeating the severity beside
    /// a coloured bar that already says it wastes the first line, which on a
    /// phone is most of what gets read.
    fn summary(notification: &Notification) -> &str {
        notification
            .title
            .split_once(": ")
            .map(|(_, rest)| rest)
            .unwrap_or(&notification.title)
    }

    /// Slack Block Kit: a coloured attachment with the detail laid out.
    fn slack_payload(notification: &Notification) -> serde_json::Value {
        let (color, icon) = Self::appearance(notification);
        let heading = if notification.trigger.is_recovery() {
            format!("{icon} RESOLVED")
        } else {
            format!("{icon} {}", notification.severity.to_string().to_uppercase())
        };

        let mut blocks = vec![
            serde_json::json!({
                "type": "header",
                "text": { "type": "plain_text", "text": truncated(&heading, SLACK_HEADER_LIMIT), "emoji": true }
            }),
            serde_json::json!({
                "type": "section",
                "text": { "type": "mrkdwn", "text": truncated(&format!("*{}*", Self::summary(notification)), SLACK_SECTION_LIMIT) }
            }),
        ];

        // The detail as a preformatted block: it is machine-written, aligned,
        // and mangled by Slack's paragraph wrapping otherwise. A fence inside
        // the text would close the block early and spill the rest as markup,
        // so any is neutralised first.
        if !notification.body.trim().is_empty() {
            let body = notification.body.trim().replace("```", "'\u{2019}'");
            blocks.push(serde_json::json!({
                "type": "section",
                "text": {
                    "type": "mrkdwn",
                    "text": truncated(&format!("```{body}```"), SLACK_SECTION_LIMIT)
                }
            }));
        }

        if !notification.recommended_actions.is_empty() {
            let actions = notification
                .recommended_actions
                .iter()
                .map(|a| format!("• `{a}`"))
                .collect::<Vec<_>>()
                .join("\n");
            blocks.push(serde_json::json!({
                "type": "section",
                "text": {
                    "type": "mrkdwn",
                    "text": truncated(&format!("*Suggested investigation (read-only)*\n{actions}"), SLACK_SECTION_LIMIT)
                }
            }));
        }

        blocks.push(serde_json::json!({
            "type": "context",
            "elements": [{
                "type": "mrkdwn",
                "text": format!("cluster-sentinel • `{}` • {}", notification.fingerprint, notification.created_at)
            }]
        }));

        serde_json::json!({
            // Also at the top level: Slack uses it for the notification
            // preview and for clients that do not render blocks.
            "text": truncated(&notification.title, SLACK_SECTION_LIMIT),
            "attachments": [{
                "color": color,
                "fallback": truncated(&notification.title, SLACK_SECTION_LIMIT),
                "blocks": blocks
            }]
        })
    }

    /// The JSON body sent for a notification.
    ///
    /// Deliberately flat and self-describing: a receiver should not need to
    /// know Sentinel's internals to route on severity or recognise a recovery.
    ///
    /// It also carries the message under the field names Slack, Discord and
    /// Teams each insist on, so those work with a URL and nothing else. A
    /// dedicated provider per service would render better -- colour, threads,
    /// buttons -- but a webhook that needs a translator in front of it is a
    /// webhook most people will not get working at all.
    pub fn payload_for(format: WebhookFormat, notification: &Notification) -> serde_json::Value {
        match format {
            WebhookFormat::Slack => Self::slack_payload(notification),
            WebhookFormat::Generic => Self::payload(notification),
        }
    }

    /// The generic JSON body.
    pub fn payload(notification: &Notification) -> serde_json::Value {
        let message = Self::message(notification);
        serde_json::json!({
            "source": "cluster-sentinel",
            "incident_id": notification.incident_id,
            "fingerprint": notification.fingerprint,
            "trigger": notification.trigger,
            "severity": notification.severity,
            "resolved": notification.trigger.is_recovery(),
            "title": notification.title,
            "body": notification.body,
            "recommended_actions": notification.recommended_actions,
            "timestamp": notification.created_at,
            // Slack and Microsoft Teams.
            "text": message,
            // Discord.
            "content": message,
        })
    }
}

#[async_trait]
impl NotificationProvider for WebhookProvider {
    fn name(&self) -> &str {
        &self.name
    }

    async fn send(&self, notification: &Notification) -> Result<(), ProviderError> {
        let response = self
            .http
            .post(&self.url)
            .json(&Self::payload_for(self.format, notification))
            .send()
            .await
            .map_err(|e| ProviderError::Unreachable {
                destination: self.url.clone(),
                detail: e.to_string(),
            })?;

        if response.status().is_success() {
            return Ok(());
        }

        let status = response.status().as_u16();
        Err(ProviderError::Rejected {
            destination: self.url.clone(),
            status,
            detail: response.text().await.unwrap_or_default().chars().take(200).collect(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::diagnosis::{kind, Confidence, Diagnosis};
    use crate::incident::{Incident, Severity};
    use crate::notification::Trigger;

    fn notification(trigger: Trigger) -> Notification {
        let mut incident = Incident::open("cause:fs1", Severity::Critical);
        incident.add_diagnosis(
            Diagnosis::new(kind::NFS_SERVICE_FAILURE, "storage.service_failure", Confidence::High)
                .with_summary("the export port is not answering")
                .recommending(vec!["systemctl status nfs-server".into()]),
        );
        Notification::for_incident(&incident, trigger)
    }

    #[test]
    fn slack_gets_a_coloured_attachment_with_blocks() {
        // Slack accepts a payload with text, blocks or attachments. This has
        // all three routes covered, and the colour is what a person reads
        // before any words.
        let payload = WebhookProvider::slack_payload(&notification(Trigger::Opened));

        assert!(payload["text"].as_str().is_some_and(|t| !t.is_empty()));
        let attachment = &payload["attachments"][0];
        assert!(attachment["color"].as_str().is_some_and(|c| c.starts_with('#')));
        assert!(attachment["blocks"].as_array().is_some_and(|b| !b.is_empty()));
    }

    #[test]
    fn a_recovery_is_green_whatever_its_severity() {
        // The distinction a colour has to carry is starting versus ending.
        // A critical incident that has just resolved is good news.
        let mut resolved = notification(Trigger::Resolved);
        resolved.severity = Severity::Critical;
        let (color, icon) = WebhookProvider::appearance(&resolved);
        assert_eq!(color, "#2e7d32");
        assert_eq!(icon, "\u{2705}");

        let (color, _) = WebhookProvider::appearance(&notification(Trigger::Opened));
        assert_ne!(color, "#2e7d32");
    }

    #[test]
    fn the_heading_does_not_repeat_what_the_colour_says() {
        // Titles are `SEVERITY: summary`, and the heading already carries the
        // severity beside a coloured bar. Repeating it wastes the first line,
        // which on a phone is most of what gets read.
        let mut opened = notification(Trigger::Opened);
        opened.title = "CRITICAL: filesrv01 is up but the export port is not answering".into();
        assert_eq!(
            WebhookProvider::summary(&opened),
            "filesrv01 is up but the export port is not answering"
        );

        // A title with no prefix is left alone rather than losing its first clause.
        opened.title = "something happened".into();
        assert_eq!(WebhookProvider::summary(&opened), "something happened");
    }

    #[test]
    fn a_code_fence_in_the_body_cannot_escape_the_block() {
        // A fence inside the text would close the preformatted block early and
        // spill the rest as markup.
        let mut n = notification(Trigger::Opened);
        n.body = "before ``` after".into();

        let payload = WebhookProvider::slack_payload(&n);
        let text = payload["attachments"][0]["blocks"][2]["text"]["text"]
            .as_str()
            .expect("body block");
        assert_eq!(text.matches("```").count(), 2, "{text}");
    }

    #[test]
    fn blocks_stay_inside_slacks_limits() {
        // Slack rejects an over-long header rather than truncating it, so a
        // long diagnosis would mean no notification at all.
        let mut long = notification(Trigger::Opened);
        long.title = "X".repeat(400);
        long.body = "Y".repeat(8000);
        long.recommended_actions = vec!["Z".repeat(5000)];

        let payload = WebhookProvider::slack_payload(&long);
        for block in payload["attachments"][0]["blocks"].as_array().expect("blocks") {
            if let Some(text) = block["text"]["text"].as_str() {
                let limit = if block["type"] == "header" {
                    SLACK_HEADER_LIMIT
                } else {
                    SLACK_SECTION_LIMIT
                };
                assert!(
                    text.chars().count() <= limit,
                    "{} block: {} chars",
                    block["type"],
                    text.chars().count()
                );
            }
        }
    }

    #[test]
    fn the_format_decides_the_shape() {
        let n = notification(Trigger::Opened);
        assert!(WebhookProvider::payload_for(WebhookFormat::Slack, &n)["attachments"].is_array());
        assert!(WebhookProvider::payload_for(WebhookFormat::Generic, &n)["attachments"].is_null());
        // Generic keeps the structured fields a script would route on.
        assert_eq!(
            WebhookProvider::payload_for(WebhookFormat::Generic, &n)["source"],
            "cluster-sentinel"
        );
    }

    #[test]
    fn the_payload_carries_a_message_slack_will_accept() {
        // Slack refuses a payload with no `text`:
        // `missing_text_or_fallback_or_attachments`. Reported from a real
        // cluster, where the only destination configured was a Slack webhook
        // and every notification bounced with a 400.
        let payload = WebhookProvider::payload(&notification(Trigger::Opened));

        let text = payload["text"].as_str().expect("text");
        assert!(!text.is_empty());
        assert!(text.contains(&notification(Trigger::Opened).title));

        // Discord wants the same thing under another name.
        assert_eq!(payload["content"], payload["text"]);
    }

    #[test]
    fn the_message_includes_what_to_do_about_it() {
        let payload = WebhookProvider::payload(&notification(Trigger::Opened));
        let text = payload["text"].as_str().expect("text");
        for action in &notification(Trigger::Opened).recommended_actions {
            assert!(text.contains(action), "{text}");
        }
    }

    #[test]
    fn the_structured_fields_are_still_there() {
        // The message is carried alongside them, not instead of them: a
        // receiver that routes on severity must keep working.
        let payload = WebhookProvider::payload(&notification(Trigger::Resolved));
        assert_eq!(payload["source"], "cluster-sentinel");
        assert_eq!(payload["resolved"], true);
        assert!(payload["severity"].is_string());
        assert!(payload["fingerprint"].is_string());
    }

    #[test]
    fn a_url_without_a_scheme_is_refused_at_construction() {
        // Better to fail at startup than to discover it during an outage.
        let error =
            WebhookProvider::new("webhook", "example.org/hook", Duration::from_secs(5)).expect_err("must refuse");
        assert!(matches!(error, ProviderError::Misconfigured(_)));
        assert!(!error.is_transient());
    }

    #[test]
    fn both_schemes_are_accepted() {
        for url in ["http://example.org/hook", "https://example.org/hook"] {
            assert!(
                WebhookProvider::new("webhook", url, Duration::from_secs(5)).is_ok(),
                "{url}"
            );
        }
    }

    #[test]
    fn the_payload_is_self_describing() {
        // A receiver should be able to route on severity and recognise a
        // recovery without knowing anything about Sentinel.
        let payload = WebhookProvider::payload(&notification(Trigger::Opened));

        assert_eq!(payload["source"], "cluster-sentinel");
        assert_eq!(payload["severity"], "critical");
        assert_eq!(payload["trigger"], "opened");
        assert_eq!(payload["resolved"], false);
        assert!(payload["title"].as_str().unwrap().contains("CRITICAL"));
        assert_eq!(payload["recommended_actions"][0], "systemctl status nfs-server");
    }

    #[test]
    fn a_recovery_is_flagged_as_such_in_the_payload() {
        let payload = WebhookProvider::payload(&notification(Trigger::Resolved));
        assert_eq!(payload["resolved"], true);
        assert_eq!(payload["trigger"], "resolved");
    }

    #[tokio::test]
    async fn a_notification_reaches_a_listening_endpoint() {
        let received = std::sync::Arc::new(tokio::sync::Mutex::new(Vec::<serde_json::Value>::new()));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let port = listener.local_addr().expect("addr").port();

        {
            let received = std::sync::Arc::clone(&received);
            tokio::spawn(async move {
                let app = axum::Router::new().route(
                    "/hook",
                    axum::routing::post(move |axum::Json(body): axum::Json<serde_json::Value>| {
                        let received = std::sync::Arc::clone(&received);
                        async move {
                            received.lock().await.push(body);
                            "ok"
                        }
                    }),
                );
                let _ = axum::serve(listener, app).await;
            });
        }

        let provider = WebhookProvider::new(
            "webhook",
            format!("http://127.0.0.1:{port}/hook"),
            Duration::from_secs(2),
        )
        .expect("provider");
        provider.send(&notification(Trigger::Opened)).await.expect("send");

        let received = received.lock().await;
        assert_eq!(received.len(), 1);
        assert_eq!(received[0]["fingerprint"], "cause:fs1");
    }

    #[tokio::test]
    async fn an_unreachable_endpoint_is_a_transient_error() {
        // The agent should keep trying; a webhook being down is not a reason
        // to stop monitoring.
        let provider =
            WebhookProvider::new("webhook", "http://127.0.0.1:1/hook", Duration::from_millis(200)).expect("provider");
        let error = provider
            .send(&notification(Trigger::Opened))
            .await
            .expect_err("must fail");

        assert!(matches!(error, ProviderError::Unreachable { .. }), "{error:?}");
        assert!(error.is_transient());
    }

    #[test]
    fn permanent_and_transient_failures_are_distinguished() {
        assert!(ProviderError::Rejected {
            destination: String::new(),
            status: 503,
            detail: String::new()
        }
        .is_transient());
        assert!(ProviderError::Rejected {
            destination: String::new(),
            status: 429,
            detail: String::new()
        }
        .is_transient());
        assert!(!ProviderError::Rejected {
            destination: String::new(),
            status: 400,
            detail: String::new()
        }
        .is_transient());
    }
}
