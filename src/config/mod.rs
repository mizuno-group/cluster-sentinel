//! Configuration.
//!
//! Deployment data — host names, addresses, partitions, storage topology —
//! lives here, in runtime discovery, or in fixtures. Never in core code
//! (SPEC.md §129, IMPLEMENTATION.md §101).
//!
//! Precedence, highest first (IMPLEMENTATION.md §58):
//!
//! ```text
//! CLI > environment variable > config file > runtime discovery > built-in default
//! ```
//!
//! Every resolved value remembers where it came from so `sentinel config check`
//! can show it.

mod precedence;
mod probes;
mod retention;
mod tls;
mod validate;

pub use precedence::{Layered, ValueSource};
pub use probes::{ProbeSchedule, ProbeSchedules};
pub use retention::{RetentionConfig, RetentionPeriod};
pub use tls::TlsConfig;
pub use validate::{validate, Severity as IssueSeverity, ValidationIssue, ValidationReport};

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::capability::CapabilityOverride;
use crate::CONFIG_VERSION;

/// Default location of the configuration file (IMPLEMENTATION.md §59).
pub const DEFAULT_CONFIG_PATH: &str = "/etc/sentinel/config.toml";
/// Default state directory.
pub const DEFAULT_STATE_DIR: &str = "/var/lib/sentinel";

/// Failure to load or accept a configuration.
#[derive(Debug, Error)]
pub enum ConfigError {
    /// The file could not be read.
    #[error("cannot read config file {path}: {source}")]
    Read {
        /// The path that failed.
        path: PathBuf,
        /// The underlying I/O error.
        #[source]
        source: std::io::Error,
    },
    /// The file is not valid TOML, or does not match the schema.
    #[error("invalid config file {path}: {source}")]
    Parse {
        /// The path that failed.
        path: PathBuf,
        /// The underlying parse error.
        #[source]
        source: toml::de::Error,
    },
    /// The file declares a schema version this build does not understand.
    ///
    /// Refusing is deliberate: silently ignoring unknown newer settings would
    /// mean monitoring something other than what the operator described
    /// (IMPLEMENTATION.md §57).
    #[error("config_version {found} is newer than this build supports ({supported}); upgrade sentinel")]
    UnsupportedVersion {
        /// The version in the file.
        found: u32,
        /// The version this build supports.
        supported: u32,
    },
    /// The file is missing `config_version`.
    #[error("config_version is required")]
    MissingVersion,
}

/// The parsed configuration file.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    /// Schema version of this file.
    pub config_version: u32,
    /// The environment this node belongs to.
    #[serde(default = "default_environment")]
    pub environment: String,
    /// Controller settings.
    #[serde(default)]
    pub controller: ControllerConfig,
    /// Agent settings.
    #[serde(default)]
    pub agent: AgentConfig,
    /// Database settings.
    #[serde(default)]
    pub database: DatabaseConfig,
    /// Peer monitoring settings.
    #[serde(default)]
    pub peer_monitoring: PeerMonitoringConfig,
    /// Operator capability overrides, keyed by capability name.
    #[serde(default)]
    pub capabilities: BTreeMap<String, CapabilityOverride>,
    /// Statically declared entities (SPEC.md §33).
    #[serde(default)]
    pub entities: Vec<EntityConfig>,
    /// Statically declared dependency edges.
    #[serde(default)]
    pub dependencies: Vec<DependencyConfig>,
    /// Integration toggles.
    #[serde(default)]
    pub discovery: DiscoveryConfig,
    /// Notification destinations.
    #[serde(default)]
    pub notification: NotificationConfig,
    /// How long recorded data is kept.
    #[serde(default)]
    pub retention: RetentionConfig,
    /// Transport security for the controller API.
    #[serde(default)]
    pub tls: TlsConfig,
    /// Per-probe schedule overrides, keyed by probe id.
    #[serde(default)]
    pub probes: ProbeSchedules,
}

fn default_environment() -> String {
    "default".to_string()
}

impl Default for Config {
    fn default() -> Self {
        Self {
            config_version: CONFIG_VERSION,
            environment: default_environment(),
            controller: ControllerConfig::default(),
            agent: AgentConfig::default(),
            database: DatabaseConfig::default(),
            peer_monitoring: PeerMonitoringConfig::default(),
            capabilities: BTreeMap::new(),
            entities: Vec::new(),
            dependencies: Vec::new(),
            discovery: DiscoveryConfig::default(),
            notification: NotificationConfig {
                webhooks: Vec::new(),
                min_severity: default_min_severity(),
            },
            retention: RetentionConfig::default(),
            tls: TlsConfig::default(),
            probes: ProbeSchedules::default(),
        }
    }
}

impl Config {
    /// Parse a configuration from TOML text.
    pub fn from_toml(text: &str, path: &Path) -> Result<Self, ConfigError> {
        // Check the version before full deserialization so that a file from a
        // newer sentinel produces a clear "upgrade" error rather than a
        // confusing unknown-field error.
        let probe: VersionProbe = toml::from_str(text).map_err(|source| ConfigError::Parse {
            path: path.to_path_buf(),
            source,
        })?;
        let Some(version) = probe.config_version else {
            return Err(ConfigError::MissingVersion);
        };
        if version > CONFIG_VERSION {
            return Err(ConfigError::UnsupportedVersion {
                found: version,
                supported: CONFIG_VERSION,
            });
        }
        toml::from_str(text).map_err(|source| ConfigError::Parse {
            path: path.to_path_buf(),
            source,
        })
    }

    /// Load a configuration from disk.
    pub fn load(path: &Path) -> Result<Self, ConfigError> {
        let text = std::fs::read_to_string(path).map_err(|source| ConfigError::Read {
            path: path.to_path_buf(),
            source,
        })?;
        Self::from_toml(&text, path)
    }
}

#[derive(Deserialize)]
struct VersionProbe {
    config_version: Option<u32>,
}

/// Controller settings.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ControllerConfig {
    /// Address the controller listens on.
    #[serde(default = "default_listen")]
    pub listen: String,
    /// How often inventory discovery runs.
    #[serde(default = "default_inventory_interval", with = "humantime_serde")]
    pub inventory_interval: Duration,
    /// How often diagnosis, correlation and notification run.
    ///
    /// Separate from `inventory_interval`, and much shorter, because the two
    /// have opposite costs. Discovery shells out to `scontrol` and probes every
    /// host, so it wants to be infrequent. Diagnosis only reads what is already
    /// stored, so it can run often — and it decides how long a fault waits
    /// before anyone is told about it.
    #[serde(default = "default_diagnosis_interval", with = "humantime_serde")]
    pub diagnosis_interval: Duration,
    /// Whether the controller itself probes entities remotely.
    ///
    /// On by default: the controller is usually a useful vantage point, and in
    /// a deployment with no peer observers it is the only one. Turning it off
    /// suits a controller that only aggregates what agents and peers report —
    /// for instance one sitting behind a firewall that would make its own view
    /// misleading.
    #[serde(default = "default_observe")]
    pub observe: bool,
}

fn default_observe() -> bool {
    true
}

fn default_listen() -> String {
    "0.0.0.0:7443".to_string()
}

fn default_inventory_interval() -> Duration {
    Duration::from_secs(300)
}

fn default_diagnosis_interval() -> Duration {
    // Short enough that a fault is reported in the same minute it happens,
    // long enough that a flapping probe does not thrash the incident engine.
    Duration::from_secs(15)
}

impl Default for ControllerConfig {
    fn default() -> Self {
        Self {
            listen: default_listen(),
            inventory_interval: default_inventory_interval(),
            diagnosis_interval: default_diagnosis_interval(),
            observe: default_observe(),
        }
    }
}

/// Agent settings.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentConfig {
    /// Controller address to report to, e.g. `controller.example:7443`.
    ///
    /// Deliberately has no default: no host name belongs in the binary
    /// (SPEC.md §42).
    #[serde(default)]
    pub controller_address: Option<String>,
    /// Path of the local spool database.
    #[serde(default)]
    pub spool_path: Option<PathBuf>,
    /// Roles, used only for UI grouping and default hints (SPEC.md §14, §15).
    #[serde(default)]
    pub roles: Vec<String>,
    /// The address to report to the controller, overriding detection.
    ///
    /// Detection has to guess which of a host's interfaces other machines
    /// reach it on, and on a node with container bridges, an IPMI interface or
    /// several fabrics it can guess wrong. This is how an operator settles it.
    #[serde(default)]
    pub address: Option<String>,
    /// The interface whose addresses to report, overriding detection.
    ///
    /// Preferable to `address` on a fleet, because one line works on every
    /// node. If the named interface has no usable address, the agent reports
    /// **no** address and says so, rather than falling back to a different
    /// network: peers probing the wrong fabric is the fault this exists to
    /// prevent.
    #[serde(default)]
    pub interface: Option<String>,
    /// The port `sshd` listens on, when it is not the default and cannot be
    /// read from `sshd_config`.
    ///
    /// The agent reads `/etc/ssh/sshd_config` for this, so it is only needed
    /// where that file is unreadable or the port is set elsewhere.
    #[serde(default)]
    pub ssh_port: Option<u16>,
    /// Address the agent's health endpoint listens on.
    ///
    /// This is what lets a peer or the controller tell "the agent is dead"
    /// from "the host is dead" (SPEC.md §61).
    #[serde(default = "default_agent_listen")]
    pub listen: String,
}

fn default_agent_listen() -> String {
    format!("0.0.0.0:{}", crate::agent::rpc::DEFAULT_PORT)
}

impl Default for AgentConfig {
    fn default() -> Self {
        Self {
            controller_address: None,
            spool_path: None,
            roles: Vec::new(),
            address: None,
            interface: None,
            ssh_port: None,
            listen: default_agent_listen(),
        }
    }
}

/// Database settings.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DatabaseConfig {
    /// Path of the controller database.
    #[serde(default = "default_database_path")]
    pub path: PathBuf,
}

fn default_database_path() -> PathBuf {
    PathBuf::from(DEFAULT_STATE_DIR).join("sentinel.db")
}

impl Default for DatabaseConfig {
    fn default() -> Self {
        Self {
            path: default_database_path(),
        }
    }
}

/// Peer monitoring settings.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PeerMonitoringConfig {
    /// How many observers watch each entity (SPEC.md §47).
    #[serde(default = "default_peer_degree")]
    pub degree: u32,
}

fn default_peer_degree() -> u32 {
    3
}

impl Default for PeerMonitoringConfig {
    fn default() -> Self {
        Self {
            degree: default_peer_degree(),
        }
    }
}

/// Where notifications go.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NotificationConfig {
    /// Webhook destinations. Each is a separate deduplication scope, so
    /// silencing one does not silence another.
    #[serde(default)]
    pub webhooks: Vec<WebhookConfig>,
    /// Minimum severity worth sending.
    #[serde(default = "default_min_severity")]
    pub min_severity: String,
}

fn default_min_severity() -> String {
    // Informational findings -- a drained node, a configuration mismatch --
    // belong in `sentinel status`, not in someone's phone at 3am.
    "warning".to_string()
}

/// One webhook destination.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WebhookConfig {
    /// A name for this destination, used in logs and for deduplication.
    pub name: String,
    /// Where to POST.
    pub url: String,
}

/// Integration discovery toggles.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DiscoveryConfig {
    /// Slurm discovery.
    #[serde(default)]
    pub slurm: SlurmDiscoveryConfig,
}

/// Slurm discovery settings. Slurm is one integration among others, never the
/// inventory itself (SPEC.md §31).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SlurmDiscoveryConfig {
    /// Whether to run Slurm discovery.
    #[serde(default)]
    pub enabled: bool,
    /// Optional path to `scontrol`, if it is not on `PATH`.
    #[serde(default)]
    pub scontrol_path: Option<PathBuf>,
}

/// A statically declared entity.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EntityConfig {
    /// Entity type, e.g. `host` or `storage`.
    #[serde(rename = "type")]
    pub entity_type: String,
    /// Canonical name.
    pub name: String,
    /// Optional display name.
    #[serde(default)]
    pub display_name: Option<String>,
    /// Optional cluster.
    #[serde(default)]
    pub cluster: Option<String>,
    /// Operator labels.
    #[serde(default)]
    pub labels: BTreeMap<String, String>,
    /// Capabilities to declare for this entity.
    #[serde(default)]
    pub capabilities: Vec<String>,
    /// Addresses to reach this entity at. Never used as identity.
    #[serde(default)]
    pub addresses: Vec<String>,
    /// Non-default ports, by service name (`ssh`, `agent`, `nfs`).
    ///
    /// Needed for entities with no agent to report for themselves: without it
    /// a host whose SSH runs on 2222 would be probed on 22 and reported down.
    #[serde(default)]
    pub ports: BTreeMap<String, u16>,
}

/// A statically declared dependency edge.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DependencyConfig {
    /// Dependent entity, as `type/name`.
    pub from: String,
    /// Depended-upon entity, as `type/name`.
    pub to: String,
    /// Dependency type; defaults to `depends_on`.
    #[serde(default = "default_dependency_type", rename = "type")]
    pub dependency_type: String,
    /// Criticality; defaults to `critical`.
    #[serde(default = "default_criticality")]
    pub criticality: String,
}

fn default_dependency_type() -> String {
    "depends_on".to_string()
}

fn default_criticality() -> String {
    "critical".to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(text: &str) -> Result<Config, ConfigError> {
        Config::from_toml(text, Path::new("test.toml"))
    }

    #[test]
    fn a_minimal_agent_config_is_enough() {
        let config = parse(
            r#"
            config_version = 1
            environment = "mizuno-lab"

            [agent]
            controller_address = "controller.example:7443"
            "#,
        )
        .expect("parse");
        assert_eq!(config.environment, "mizuno-lab");
        assert_eq!(
            config.agent.controller_address.as_deref(),
            Some("controller.example:7443")
        );
        assert_eq!(config.peer_monitoring.degree, 3, "defaults fill in");
    }

    #[test]
    fn a_future_config_version_is_refused_rather_than_half_read() {
        let error = parse("config_version = 9999").expect_err("must refuse");
        assert!(
            matches!(error, ConfigError::UnsupportedVersion { found: 9999, .. }),
            "{error:?}"
        );
    }

    #[test]
    fn a_missing_config_version_is_refused() {
        let error = parse("environment = \"x\"").expect_err("must refuse");
        assert!(matches!(error, ConfigError::MissingVersion));
    }

    #[test]
    fn unknown_keys_are_refused_so_typos_do_not_silently_disable_monitoring() {
        let error = parse(
            r#"
            config_version = 1
            [peer_monitoring]
            degre = 5
            "#,
        )
        .expect_err("must refuse");
        assert!(matches!(error, ConfigError::Parse { .. }));
    }

    #[test]
    fn entities_and_dependencies_parse_without_naming_a_technology() {
        let config = parse(
            r#"
            config_version = 1
            environment = "lab"

            [[entities]]
            type = "host"
            name = "fileserver-a"
            labels = { rack = "r01" }
            capabilities = ["storage.nfs.server"]

            [[entities]]
            type = "storage"
            name = "shared-a"

            [[dependencies]]
            from = "host/compute-a"
            to = "storage/shared-a"
            type = "uses_storage"
            "#,
        )
        .expect("parse");
        assert_eq!(config.entities.len(), 2);
        assert_eq!(config.entities[0].capabilities, ["storage.nfs.server"]);
        assert_eq!(config.dependencies[0].dependency_type, "uses_storage");
        assert_eq!(config.dependencies[0].criticality, "critical", "default applies");
    }

    #[test]
    fn capability_overrides_parse() {
        let config = parse(
            r#"
            config_version = 1

            [capabilities]
            "storage.nfs.server" = "force"
            "storage.smart" = "disable"
            "#,
        )
        .expect("parse");
        assert_eq!(
            config.capabilities.get("storage.nfs.server"),
            Some(&CapabilityOverride::Force)
        );
        assert_eq!(
            config.capabilities.get("storage.smart"),
            Some(&CapabilityOverride::Disable)
        );
    }

    #[test]
    fn durations_are_written_the_way_operators_write_them() {
        let config = parse(
            r#"
            config_version = 1
            [controller]
            inventory_interval = "10m"
            "#,
        )
        .expect("parse");
        assert_eq!(config.controller.inventory_interval, Duration::from_secs(600));
    }

    #[test]
    fn notifications_default_to_nowhere_and_to_warning_and_above() {
        // Sending nowhere by default is right: a monitoring system that starts
        // paging an address the operator did not configure is worse than silent.
        let config = Config::default();
        assert!(config.notification.webhooks.is_empty());
        assert_eq!(config.notification.min_severity, "warning");
    }

    #[test]
    fn webhooks_parse() {
        let config = Config::from_toml(
            r#"
            config_version = 1

            [notification]
            min_severity = "critical"

            [[notification.webhooks]]
            name = "ntfy"
            url = "https://ntfy.example.org/cluster"
            "#,
            Path::new("test.toml"),
        )
        .expect("parse");

        assert_eq!(config.notification.webhooks.len(), 1);
        assert_eq!(config.notification.webhooks[0].name, "ntfy");
        assert_eq!(config.notification.min_severity, "critical");
    }

    #[test]
    fn no_controller_host_is_baked_into_the_defaults() {
        let config = Config::default();
        assert_eq!(config.agent.controller_address, None);
    }

    #[test]
    fn diagnosis_runs_far_more_often_than_discovery() {
        // Discovery is expensive and can wait; diagnosis decides how long a
        // fault goes unreported, and must not inherit discovery's cadence.
        let config = Config::default();
        assert!(
            config.controller.diagnosis_interval < config.controller.inventory_interval,
            "{:?} vs {:?}",
            config.controller.diagnosis_interval,
            config.controller.inventory_interval
        );
        assert!(config.controller.diagnosis_interval <= Duration::from_secs(30));
    }

    #[test]
    fn the_controller_observes_by_default() {
        // With no peers deployed, the controller is the only vantage point
        // there is; defaulting to off would mean seeing nothing.
        assert!(Config::default().controller.observe);
    }

    #[test]
    fn observation_can_be_turned_off() {
        let config = Config::from_toml(
            "config_version = 1
[controller]
observe = false
",
            Path::new("test.toml"),
        )
        .expect("parse");
        assert!(!config.controller.observe);
    }

    #[test]
    fn the_agent_listens_on_all_interfaces_by_default() {
        // A peer has to be able to reach it, so loopback would defeat the point.
        let config = Config::default();
        assert!(config.agent.listen.starts_with("0.0.0.0:"), "{}", config.agent.listen);
    }
}
