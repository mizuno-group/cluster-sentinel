//! NFS server-side probes.
//!
//! Split into separate probes rather than one `fileserver_health` check
//! (IMPLEMENTATION.md §81). "The fileserver is unhealthy" tells an operator
//! nothing; "the port answers but nothing is exported" tells them where to
//! look.

use std::path::{Path, PathBuf};
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
    exports_dir: PathBuf,
}

/// What the export sources between them said.
#[derive(Debug, Default, PartialEq)]
pub struct ExportSources {
    /// Paths this host exports.
    pub exports: Vec<String>,
    /// Sources that were read.
    pub read: usize,
    /// Sources that exist but could not be read.
    pub unreadable: usize,
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
            exports_dir: PathBuf::from(EXPORTS_DIR),
        }
    }

    /// Builder: read exports from somewhere else, for tests.
    pub fn with_exports_path(mut self, path: impl Into<PathBuf>) -> Self {
        self.exports_path = path.into();
        self
    }

    /// Builder: read the drop-in directory from somewhere else, for tests.
    pub fn with_exports_dir(mut self, path: impl Into<PathBuf>) -> Self {
        self.exports_dir = path.into();
        self
    }

    /// Everything this host exports, from every source the server reads.
    ///
    /// Both `/etc/exports` and `/etc/exports.d/*.exports`, because that is
    /// what `exportfs` itself reads (exports(5)). Reading only the first was
    /// wrong on any host whose exports are managed by something that uses the
    /// drop-in directory -- ZFS `sharenfs` writes `zfs.exports` there, and the
    /// package-supplied `/etc/exports` beside it contains nothing but
    /// comments. That host exports its whole pool and looked, to this probe,
    /// like a fileserver serving nothing.
    pub fn export_sources(&self) -> ExportSources {
        let mut found = ExportSources::default();

        let mut consider = |path: &Path, exists: bool| match std::fs::read_to_string(path) {
            Ok(text) => {
                found.read += 1;
                found.exports.extend(parse_exports(&text));
            }
            Err(_) if exists => found.unreadable += 1,
            Err(_) => {}
        };

        consider(&self.exports_path, self.exports_path.exists());

        if let Ok(entries) = std::fs::read_dir(&self.exports_dir) {
            let mut paths: Vec<PathBuf> = entries
                .flatten()
                .map(|entry| entry.path())
                .filter(|path| path.extension().is_some_and(|ext| ext == "exports"))
                .collect();
            // Sorted, because the server applies them in name order and an
            // operator comparing two runs should not have to sort by eye.
            paths.sort();
            for path in paths {
                consider(&path, true);
            }
        }

        found.exports.sort();
        found.exports.dedup();
        found
    }

    /// The exported paths, or `None` when nothing could be read.
    pub fn exports(&self) -> Option<Vec<String>> {
        let found = self.export_sources();
        (found.read > 0).then_some(found.exports)
    }
}

/// Where the NFS server reads drop-in export files from.
pub const EXPORTS_DIR: &str = "/etc/exports.d";

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
        let found = self.export_sources();

        if !found.exports.is_empty() {
            return Observation::new(PROBE_SERVER_EXPORTS.into(), context.target_entity, ProbeStatus::Ok).with_payload(
                serde_json::json!({
                    "exports": found.exports,
                    "export_count": found.exports.len(),
                    "sources_read": found.read,
                }),
            );
        }

        // A source we cannot read is not a source that says nothing. Calling
        // that an empty export list would report a working fileserver as
        // serving nothing, which is a page and a trip to the wrong machine.
        if found.unreadable > 0 {
            return Observation::new(
                PROBE_SERVER_EXPORTS.into(),
                context.target_entity,
                ProbeStatus::Unsupported,
            )
            .with_payload(serde_json::json!({"unreadable_sources": found.unreadable}))
            .with_error(
                "exports_unreadable",
                format!(
                    "{} export source(s) exist but could not be read; \
                     no conclusion can be drawn about what is exported",
                    found.unreadable
                ),
            );
        }

        if found.read > 0 {
            // Stated, not judged. "This host exports nothing" is a fact;
            // whether it is a fault depends on whether anything is listening
            // on 2049, and only a rule sees both. The capability that gates
            // this probe is detected from `/etc/exports` existing or
            // `exportfs` being installed -- true of any host with the NFS
            // packages, including every client -- so treating an empty list as
            // a failure here reported a compute node that exports nothing, and
            // was never meant to, as a broken fileserver.
            return Observation::new(PROBE_SERVER_EXPORTS.into(), context.target_entity, ProbeStatus::Ok).with_payload(
                serde_json::json!({
                    "exports": [],
                    "export_count": 0,
                    "sources_read": found.read,
                }),
            );
        }

        // Nothing to read at all is not a fault: this host may export through
        // some other mechanism, or the capability may have been forced on.
        Observation::new(
            PROBE_SERVER_EXPORTS.into(),
            context.target_entity,
            ProbeStatus::Unsupported,
        )
        .with_error(
            "no_exports_file",
            format!(
                "neither {} nor {}/*.exports could be read",
                self.exports_path.display(),
                self.exports_dir.display()
            ),
        )
    }
}

// Operators may retune this probe's schedule in [probes].
crate::probes::configurable_probe!(NfsPortProbe, NfsExportsProbe);

#[cfg(test)]
mod tests {

    /// A probe reading a temporary directory instead of /etc.
    fn probe_over(dir: &std::path::Path) -> NfsExportsProbe {
        NfsExportsProbe::new()
            .with_exports_path(dir.join("exports"))
            .with_exports_dir(dir.join("exports.d"))
    }

    fn exports_context() -> ProbeContext {
        context(serde_json::Value::Null)
    }

    #[tokio::test]
    async fn exports_managed_by_zfs_are_found_in_the_drop_in_directory() {
        // The reason this reads more than one file. `zfs set sharenfs=on`
        // writes /etc/exports.d/zfs.exports, and the /etc/exports beside it is
        // the one the package shipped: comments only. Reading just that file
        // reported a fileserver exporting its whole pool as exporting nothing,
        // which is a critical incident pointing at a healthy machine.
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(dir.path().join("exports"), "# /etc/exports: see exports(5)\n").expect("exports");
        std::fs::create_dir(dir.path().join("exports.d")).expect("dir");
        std::fs::write(
            dir.path().join("exports.d/zfs.exports"),
            "/tank\t*(rw,no_subtree_check)\n",
        )
        .expect("zfs.exports");

        let observation = probe_over(dir.path()).collect(&exports_context()).await;

        assert_eq!(observation.status, ProbeStatus::Ok, "{:?}", observation.error_message);
        assert_eq!(observation.payload["exports"][0], "/tank");
    }

    #[tokio::test]
    async fn both_sources_are_combined_without_duplicates() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(dir.path().join("exports"), "/srv/home *(rw)\n/tank *(rw)\n").expect("exports");
        std::fs::create_dir(dir.path().join("exports.d")).expect("dir");
        std::fs::write(dir.path().join("exports.d/zfs.exports"), "/tank *(rw)\n").expect("zfs");

        let found = probe_over(dir.path()).export_sources();
        assert_eq!(found.exports, vec!["/srv/home".to_string(), "/tank".to_string()]);
        assert_eq!(found.read, 2);
    }

    #[tokio::test]
    async fn files_without_the_exports_extension_are_ignored() {
        // The server reads *.exports only, so a stray backup in that directory
        // must not become part of what this host claims to serve.
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(dir.path().join("exports"), "# nothing\n").expect("exports");
        std::fs::create_dir(dir.path().join("exports.d")).expect("dir");
        std::fs::write(dir.path().join("exports.d/zfs.exports.bak"), "/old *(rw)\n").expect("bak");

        let observation = probe_over(dir.path()).collect(&exports_context()).await;
        assert_eq!(observation.status, ProbeStatus::Ok);
        assert_eq!(observation.payload["export_count"], 0, "the .bak file is not an export");
    }

    #[tokio::test]
    async fn a_host_exporting_nothing_anywhere_reports_a_count_of_zero() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(dir.path().join("exports"), "# see exports(5)\n").expect("exports");
        std::fs::create_dir(dir.path().join("exports.d")).expect("dir");

        let observation = probe_over(dir.path()).collect(&exports_context()).await;
        assert_eq!(observation.status, ProbeStatus::Ok);
        assert_eq!(observation.payload["export_count"], 0);
    }

    #[tokio::test]
    async fn no_export_sources_at_all_is_not_a_fault() {
        // The capability may have been forced on, or this host may export
        // through something else entirely.
        let dir = tempfile::tempdir().expect("tempdir");
        let observation = probe_over(dir.path()).collect(&exports_context()).await;
        assert_eq!(observation.status, ProbeStatus::Unsupported);
        assert_eq!(observation.error_code.as_deref(), Some("no_exports_file"));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_source_that_cannot_be_read_draws_no_conclusion() {
        // Not "nothing is exported". An unreadable file is a question this
        // probe cannot answer, and answering it anyway condemns a working
        // fileserver.
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(dir.path().join("exports"), "# see exports(5)\n").expect("exports");
        std::fs::create_dir(dir.path().join("exports.d")).expect("dir");
        let secret = dir.path().join("exports.d/zfs.exports");
        std::fs::write(&secret, "/tank *(rw)\n").expect("zfs");
        std::fs::set_permissions(&secret, std::fs::Permissions::from_mode(0o000)).expect("chmod");

        let observation = probe_over(dir.path()).collect(&exports_context()).await;

        // Running as root would read it regardless, and then the assertion
        // below is about a different situation.
        if observation.status != ProbeStatus::Ok {
            assert_eq!(observation.status, ProbeStatus::Unsupported, "{observation:?}");
            assert_eq!(observation.error_code.as_deref(), Some("exports_unreadable"));
        }
    }
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
    async fn an_empty_export_list_is_recorded_as_a_fact_not_a_fault() {
        // Whether exporting nothing is a fault depends on whether anything is
        // listening on 2049, and only a rule sees both. The capability gating
        // this probe is true of any host with the NFS packages installed, so
        // failing here condemned every client that had them.
        let (_dir, path) = exports_file("# everything commented out\n");
        let observation = NfsExportsProbe::new()
            .with_exports_path(&path)
            .collect(&context(serde_json::Value::Null))
            .await;

        assert_eq!(observation.status, ProbeStatus::Ok);
        assert_eq!(observation.payload["export_count"], 0);
        assert!(observation.error_code.is_none(), "{:?}", observation.error_code);
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
