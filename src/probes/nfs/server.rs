//! NFS server-side probes.
//!
//! Split into separate probes rather than one `fileserver_health` check
//! (IMPLEMENTATION.md §81). "The fileserver is unhealthy" tells an operator
//! nothing; "the port answers but nothing is exported" tells them where to
//! look.

use std::path::PathBuf;
use std::time::Duration;

use async_trait::async_trait;

use crate::capability::well_known;
use crate::entity::EntityType;
use crate::observation::{Observation, ProbeStatus};
use crate::probes::network::connect;
use crate::probes::{ExecutionMode, Probe, ProbeContext, ProbeDefinition};

use super::NFS_PORT;

/// Probe id for the NFS port check.
pub const PROBE_SERVER_PORT: &str = "nfs.server.port";
/// Probe id for the export list check.
pub const PROBE_SERVER_EXPORTS: &str = "nfs.server.exports";

/// Checks that the NFS port answers.
///
/// Runs from anywhere: a fileserver's own view of its port is less interesting
/// than a client's, and a client is where the problem is actually felt.
#[derive(Debug, Clone)]
pub struct NfsPortProbe {
    definition: ProbeDefinition,
}

impl Default for NfsPortProbe {
    fn default() -> Self {
        Self::new()
    }
}

impl NfsPortProbe {
    /// A probe with the default schedule.
    pub fn new() -> Self {
        Self {
            definition: ProbeDefinition::new(PROBE_SERVER_PORT)
                .requiring([well_known::STORAGE_NFS_SERVER])
                .targeting([EntityType::Host, EntityType::Storage])
                .every(Duration::from_secs(15))
                .within(Duration::from_secs(5))
                .mode(ExecutionMode::Either),
        }
    }
}

#[async_trait]
impl Probe for NfsPortProbe {
    fn definition(&self) -> &ProbeDefinition {
        &self.definition
    }

    async fn collect(&self, context: &ProbeContext) -> Observation {
        let Some(address) = context.parameter_str("address") else {
            return Observation::new(
                PROBE_SERVER_PORT.into(),
                context.target_entity,
                ProbeStatus::NotApplicable,
            )
            .with_error("no_address", "no address is known for this storage entity");
        };
        let port = context.parameter_u64("nfs_port").unwrap_or(NFS_PORT as u64) as u16;

        let outcome = connect(address, port, context.timeout).await;

        // Unlike generic reachability, a refusal here IS a failure: the export
        // service is supposed to be listening. A refusal does still tell us the
        // host is alive, which the diagnosis rules use to separate "the
        // fileserver is down" from "its NFS service is down".
        let observation = Observation::new(PROBE_SERVER_PORT.into(), context.target_entity, outcome.status())
            .with_payload(serde_json::json!({
                "address": address,
                "port": port,
                "outcome": outcome.code(),
                "host_responded": outcome.proves_host_responds(),
            }));

        if outcome.status() == ProbeStatus::Ok {
            observation
        } else {
            observation.with_error(
                outcome.code(),
                format!("the NFS service on {address}:{port} is not accepting connections"),
            )
        }
    }
}

/// Checks that a fileserver is actually exporting something.
///
/// A server whose port answers but whose export list is empty is a real and
/// confusing failure mode: clients get permission errors rather than timeouts,
/// and nothing looks down.
#[derive(Debug, Clone)]
pub struct NfsExportsProbe {
    definition: ProbeDefinition,
    exports_path: PathBuf,
}

impl Default for NfsExportsProbe {
    fn default() -> Self {
        Self::new()
    }
}

impl NfsExportsProbe {
    /// A probe reading the real `/etc/exports`.
    pub fn new() -> Self {
        Self {
            definition: ProbeDefinition::new(PROBE_SERVER_EXPORTS)
                .requiring([well_known::STORAGE_NFS_SERVER])
                .targeting([EntityType::Host])
                .every(Duration::from_secs(60))
                .within(Duration::from_secs(5)),
            exports_path: PathBuf::from("/etc/exports"),
        }
    }

    /// Builder: read exports from somewhere else, for tests.
    pub fn with_exports_path(mut self, path: impl Into<PathBuf>) -> Self {
        self.exports_path = path.into();
        self
    }

    /// The exported paths.
    pub fn exports(&self) -> Option<Vec<String>> {
        let text = std::fs::read_to_string(&self.exports_path).ok()?;
        Some(parse_exports(&text))
    }
}

/// Parse `/etc/exports`, returning the exported paths.
pub fn parse_exports(text: &str) -> Vec<String> {
    text.lines()
        .map(|line| line.split('#').next().unwrap_or("").trim())
        .filter(|line| !line.is_empty())
        .filter_map(|line| {
            // A path may be quoted, or escaped with `\040` for spaces.
            if let Some(rest) = line.strip_prefix('"') {
                return rest.split('"').next().map(str::to_string);
            }
            line.split_whitespace().next().map(|p| p.replace("\\040", " "))
        })
        .collect()
}

#[async_trait]
impl Probe for NfsExportsProbe {
    fn definition(&self) -> &ProbeDefinition {
        &self.definition
    }

    async fn collect(&self, context: &ProbeContext) -> Observation {
        match self.exports() {
            Some(exports) if !exports.is_empty() => {
                Observation::new(PROBE_SERVER_EXPORTS.into(), context.target_entity, ProbeStatus::Ok)
                    .with_payload(serde_json::json!({"exports": exports, "export_count": exports.len()}))
            }
            Some(_) => Observation::new(PROBE_SERVER_EXPORTS.into(), context.target_entity, ProbeStatus::Failed)
                .with_payload(serde_json::json!({"exports": [], "export_count": 0}))
                .with_error(
                    "no_exports",
                    "the export list is empty; clients will be refused, not timed out",
                ),
            // No exports file is not a fault: this host may export through some
            // other mechanism, or the capability may have been forced on.
            None => Observation::new(
                PROBE_SERVER_EXPORTS.into(),
                context.target_entity,
                ProbeStatus::Unsupported,
            )
            .with_error(
                "no_exports_file",
                format!("cannot read {}", self.exports_path.display()),
            ),
        }
    }
}

// Operators may retune this probe's schedule in [probes].
crate::probes::configurable_probe!(NfsPortProbe, NfsExportsProbe);

#[cfg(test)]
mod tests {
    use super::*;
    use crate::capability::CapabilitySet;
    use crate::entity::{EntityKey, EntityType};

    fn context(parameters: serde_json::Value) -> ProbeContext {
        ProbeContext::local(
            EntityKey::new("lab", EntityType::Host, "fs-a").entity_id(),
            CapabilitySet::new(),
        )
        .with_parameters(parameters)
        .with_timeout(Duration::from_millis(500))
    }

    fn exports_file(contents: &str) -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("exports");
        std::fs::write(&path, contents).expect("write");
        (dir, path)
    }

    #[test]
    fn exports_parse_with_comments_and_options() {
        let text = "\
# Home directories
/export/home  10.0.0.0/24(rw,sync,no_subtree_check)
/export/scratch *(rw,async)

# commented out for now
#/export/old 10.0.0.0/24(ro)
";
        assert_eq!(parse_exports(text), ["/export/home", "/export/scratch"]);
    }

    #[test]
    fn a_quoted_export_path_with_spaces_parses() {
        assert_eq!(parse_exports("\"/export/my data\" *(rw)\n"), ["/export/my data"]);
    }

    #[test]
    fn an_escaped_space_parses() {
        assert_eq!(parse_exports("/export/my\\040data *(rw)\n"), ["/export/my data"]);
    }

    #[test]
    fn an_empty_exports_file_yields_nothing() {
        assert!(parse_exports("").is_empty());
        assert!(parse_exports("# only a comment\n\n").is_empty());
    }

    #[tokio::test]
    async fn a_server_with_exports_reports_them() {
        let (_dir, path) = exports_file("/export/home 10.0.0.0/24(rw)\n");
        let observation = NfsExportsProbe::new()
            .with_exports_path(&path)
            .collect(&context(serde_json::Value::Null))
            .await;

        assert_eq!(observation.status, ProbeStatus::Ok);
        assert_eq!(observation.payload["export_count"], 1);
    }

    #[tokio::test]
    async fn a_server_exporting_nothing_is_a_failure() {
        // Clients get permission errors rather than timeouts, so nothing else
        // looks down. Worth reporting loudly.
        let (_dir, path) = exports_file("# everything commented out\n");
        let observation = NfsExportsProbe::new()
            .with_exports_path(&path)
            .collect(&context(serde_json::Value::Null))
            .await;

        assert_eq!(observation.status, ProbeStatus::Failed);
        assert_eq!(observation.error_code.as_deref(), Some("no_exports"));
    }

    #[tokio::test]
    async fn a_missing_exports_file_is_unsupported_not_a_failure() {
        // The host may export through another mechanism entirely.
        let observation = NfsExportsProbe::new()
            .with_exports_path("/nonexistent/etc/exports")
            .collect(&context(serde_json::Value::Null))
            .await;

        assert_eq!(observation.status, ProbeStatus::Unsupported);
        assert!(!observation.status.is_bad());
    }

    #[tokio::test]
    async fn a_listening_nfs_port_is_healthy() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let port = listener.local_addr().expect("addr").port();

        let observation = NfsPortProbe::new()
            .collect(&context(serde_json::json!({"address": "127.0.0.1", "nfs_port": port})))
            .await;

        assert_eq!(observation.status, ProbeStatus::Ok);
        assert_eq!(observation.payload["host_responded"], true);
    }

    #[tokio::test]
    async fn a_stopped_nfs_service_is_a_failure_but_shows_the_host_answering() {
        // The distinction that separates "the fileserver is down" from "its
        // NFS service is down": the refusal proves the machine is alive.
        let port = {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("bind");
            listener.local_addr().expect("addr").port()
        };

        let observation = NfsPortProbe::new()
            .collect(&context(serde_json::json!({"address": "127.0.0.1", "nfs_port": port})))
            .await;

        assert_eq!(observation.status, ProbeStatus::Failed);
        assert_eq!(observation.payload["outcome"], "refused");
        assert_eq!(
            observation.payload["host_responded"], true,
            "a refusal is still evidence the fileserver itself is up"
        );
    }

    #[tokio::test]
    async fn a_storage_entity_with_no_address_is_not_applicable() {
        let observation = NfsPortProbe::new().collect(&context(serde_json::Value::Null)).await;
        assert_eq!(observation.status, ProbeStatus::NotApplicable);
    }

    #[test]
    fn the_probes_are_gated_on_the_server_capability() {
        for definition in [
            NfsPortProbe::new().definition().clone(),
            NfsExportsProbe::new().definition().clone(),
        ] {
            assert!(definition.applies_to(EntityType::Host, &CapabilitySet::from_iter(["storage.nfs.server"])));
            assert!(!definition.applies_to(EntityType::Host, &CapabilitySet::new()));
            assert!(
                !definition.applies_to(EntityType::Host, &CapabilitySet::from_iter(["storage.nfs.client"])),
                "mounting NFS does not make a host a server"
            );
        }
    }
}
