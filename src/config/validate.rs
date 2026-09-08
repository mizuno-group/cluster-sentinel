//! `sentinel config check` (IMPLEMENTATION.md §60).
//!
//! Validation reports *every* problem it can find rather than stopping at the
//! first, because a config check that has to be run six times to find six typos
//! is a config check nobody runs.

use std::collections::BTreeSet;
use std::fmt;

use crate::dependency::Criticality;
use crate::entity::EntityType;

use super::Config;

/// How serious a validation finding is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Severity {
    /// Worth mentioning; the configuration still works.
    Warning,
    /// The configuration cannot be used as written.
    Error,
}

impl fmt::Display for Severity {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Severity::Warning => "warning",
            Severity::Error => "error",
        })
    }
}

/// One validation finding.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ValidationIssue {
    /// How serious it is.
    pub severity: Severity,
    /// Dotted path of the offending setting, e.g. `entities[1].type`.
    pub location: String,
    /// What is wrong.
    pub message: String,
}

impl ValidationIssue {
    fn error(location: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            severity: Severity::Error,
            location: location.into(),
            message: message.into(),
        }
    }

    fn warning(location: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            severity: Severity::Warning,
            location: location.into(),
            message: message.into(),
        }
    }
}

impl fmt::Display for ValidationIssue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}: {}", self.severity, self.location, self.message)
    }
}

/// The result of validating a configuration.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ValidationReport {
    /// Everything found, in discovery order.
    pub issues: Vec<ValidationIssue>,
}

impl ValidationReport {
    /// Whether the configuration is usable.
    pub fn is_ok(&self) -> bool {
        !self.issues.iter().any(|i| i.severity == Severity::Error)
    }

    /// Findings that make the configuration unusable.
    pub fn errors(&self) -> impl Iterator<Item = &ValidationIssue> {
        self.issues.iter().filter(|i| i.severity == Severity::Error)
    }

    /// Findings worth mentioning.
    pub fn warnings(&self) -> impl Iterator<Item = &ValidationIssue> {
        self.issues.iter().filter(|i| i.severity == Severity::Warning)
    }
}

/// Check a configuration for problems.
pub fn validate(config: &Config) -> ValidationReport {
    let mut report = ValidationReport::default();

    if config.environment.trim().is_empty() {
        report
            .issues
            .push(ValidationIssue::error("environment", "must not be empty"));
    }

    validate_endpoint(&config.controller.listen, "controller.listen", &mut report);
    if let Some(address) = &config.agent.controller_address {
        validate_endpoint(address, "agent.controller_address", &mut report);
    }

    if config.peer_monitoring.degree == 0 {
        report.issues.push(ValidationIssue::warning(
            "peer_monitoring.degree",
            "0 disables peer monitoring; a single observer failure will then look like a host failure",
        ));
    }

    let mut declared: BTreeSet<(String, String)> = BTreeSet::new();
    for (index, entity) in config.entities.iter().enumerate() {
        let location = format!("entities[{index}]");
        if EntityType::parse(&entity.entity_type).is_none() {
            report.issues.push(ValidationIssue::error(
                format!("{location}.type"),
                format!("unknown entity type {:?}", entity.entity_type),
            ));
        }
        if entity.name.trim().is_empty() {
            report
                .issues
                .push(ValidationIssue::error(format!("{location}.name"), "must not be empty"));
        }
        if !declared.insert((entity.entity_type.clone(), entity.name.clone())) {
            report.issues.push(ValidationIssue::error(
                location.clone(),
                format!("duplicate entity {}/{}", entity.entity_type, entity.name),
            ));
        }
        for (capability_index, capability) in entity.capabilities.iter().enumerate() {
            if capability.trim().is_empty() {
                report.issues.push(ValidationIssue::error(
                    format!("{location}.capabilities[{capability_index}]"),
                    "must not be empty",
                ));
            }
        }
    }

    for (index, dependency) in config.dependencies.iter().enumerate() {
        let location = format!("dependencies[{index}]");
        let from = parse_entity_ref(&dependency.from);
        let to = parse_entity_ref(&dependency.to);

        for (field, parsed, raw) in [("from", &from, &dependency.from), ("to", &to, &dependency.to)] {
            match parsed {
                None => report.issues.push(ValidationIssue::error(
                    format!("{location}.{field}"),
                    format!("expected \"type/name\", got {raw:?}"),
                )),
                Some(reference) if !declared.contains(reference) => {
                    // Not an error: the entity may legitimately arrive from
                    // Slurm discovery or agent registration (SPEC.md §30).
                    report.issues.push(ValidationIssue::warning(
                        format!("{location}.{field}"),
                        format!("{raw} is not declared here; it must come from discovery or registration"),
                    ));
                }
                Some(_) => {}
            }
        }

        if from.is_some() && from == to {
            report.issues.push(ValidationIssue::warning(
                location.clone(),
                "self-dependency has no effect on diagnosis",
            ));
        }

        if Criticality::parse(&dependency.criticality).is_none() {
            report.issues.push(ValidationIssue::error(
                format!("{location}.criticality"),
                format!("unknown criticality {:?}", dependency.criticality),
            ));
        }
    }

    validate_retention(config, &mut report);
    validate_tls(config, &mut report);
    validate_probes(config, &mut report);

    report
}

/// Catch probe schedules that name a probe that does not exist, or that ask
/// for a cadence nobody wants.
fn validate_probes(config: &Config, report: &mut ValidationReport) {
    for probe_id in config.probes.ids() {
        let location = format!("probes.{probe_id:?}");

        if !crate::probes::catalog::is_known(probe_id) {
            // An unknown id would otherwise do nothing at all, which is the
            // worst outcome for a typo: the operator believes they retuned a
            // probe and nothing changed.
            report.issues.push(ValidationIssue::error(
                location.clone(),
                format!(
                    "unknown probe; known probes are: {}",
                    crate::probes::catalog::ids().join(", ")
                ),
            ));
            continue;
        }

        let Some(schedule) = config.probes.get(probe_id) else {
            continue;
        };

        if let Some(interval) = schedule.interval {
            if interval < std::time::Duration::from_secs(1) {
                report.issues.push(ValidationIssue::warning(
                    format!("{location}.interval"),
                    "under a second: probes cost a syscall or a connection each, \
                     and this one will spend more time being scheduled than measuring",
                ));
            }
        }

        // A timeout longer than the interval means a slow probe overlaps its
        // own next run, which is how a stuck target turns into a growing pile
        // of outstanding work.
        let interval = schedule.interval;
        let timeout = schedule.timeout;
        if let (Some(interval), Some(timeout)) = (interval, timeout) {
            if timeout > interval {
                report.issues.push(ValidationIssue::warning(
                    format!("{location}.timeout"),
                    "longer than the interval: executions will overlap",
                ));
            }
        }

        if schedule.enabled == Some(false) {
            report.issues.push(ValidationIssue::warning(
                format!("{location}.enabled"),
                "this probe is switched off; anything diagnosed from it will not be reported",
            ));
        }
    }
}

/// Report TLS settings that cannot work, or that work but protect nothing.
fn validate_tls(config: &Config, report: &mut ValidationReport) {
    for problem in config.tls.problems() {
        report.issues.push(ValidationIssue::error("tls", problem));
    }

    if config.tls.insecure_skip_verify {
        report.issues.push(ValidationIssue::warning(
            "tls.insecure_skip_verify",
            "the controller certificate is not checked: anyone who can redirect \
             the connection can read the cluster credential",
        ));
    }
}

/// Warn about retention settings that let the database grow unchecked.
///
/// These are warnings, not errors: a site with a large disk or a compliance
/// requirement is entitled to keep everything. It just should not be able to
/// arrive there without being told.
fn validate_retention(config: &Config, report: &mut ValidationReport) {
    let retention = &config.retention;

    if !retention.enabled {
        report.issues.push(ValidationIssue::warning(
            "retention.enabled",
            "pruning is off: the database will grow until the disk is full unless \
             something else removes rows from it",
        ));
        return;
    }

    let periods = [
        ("retention.observations", retention.observations),
        ("retention.transitions", retention.transitions),
        ("retention.resolved_incidents", retention.resolved_incidents),
        ("retention.diagnoses", retention.diagnoses),
    ];
    // Observations are the bulk of the bytes by a wide margin, so keeping them
    // forever is the one worth saying out loud.
    for (location, period) in periods {
        if period.is_forever() && location == "retention.observations" {
            report.issues.push(ValidationIssue::warning(
                location,
                "observations are kept forever: they are the bulk of the database, \
                 measured at roughly 170 MB per host per day",
            ));
        }
    }

    if retention.keep_per_entity < crate::persistence::MIN_KEEP_PER_ENTITY {
        report.issues.push(ValidationIssue::warning(
            "retention.keep_per_entity",
            format!(
                "raised to {} at prune time: diagnosis reads that many recent \
                 observations per entity, and a lower floor would let pruning blind it",
                crate::persistence::MIN_KEEP_PER_ENTITY
            ),
        ));
    }

    if retention.interval < std::time::Duration::from_secs(60) {
        report.issues.push(ValidationIssue::warning(
            "retention.interval",
            "pruning more than once a minute spends write locks to delete almost nothing",
        ));
    }
}

fn validate_endpoint(value: &str, location: &str, report: &mut ValidationReport) {
    // Accept `host:port` and `[v6]:port`; resolution happens later, this only
    // catches shapes that can never work.
    let Some((host, port)) = value.rsplit_once(':') else {
        report.issues.push(ValidationIssue::error(
            location,
            format!("expected \"host:port\", got {value:?}"),
        ));
        return;
    };
    if host.trim().is_empty() {
        report
            .issues
            .push(ValidationIssue::error(location, "host part is empty"));
    }
    match port.parse::<u16>() {
        Ok(0) => report
            .issues
            .push(ValidationIssue::error(location, "port 0 is not a valid endpoint")),
        Ok(_) => {}
        Err(_) => report
            .issues
            .push(ValidationIssue::error(location, format!("invalid port {port:?}"))),
    }
}

fn parse_entity_ref(value: &str) -> Option<(String, String)> {
    let (entity_type, name) = value.split_once('/')?;
    if entity_type.is_empty() || name.is_empty() || EntityType::parse(entity_type).is_none() {
        return None;
    }
    Some((entity_type.to_string(), name.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    fn parse(text: &str) -> Config {
        Config::from_toml(text, Path::new("test.toml")).expect("parse")
    }

    #[test]
    fn a_sound_configuration_passes_cleanly() {
        let config = parse(
            r#"
            config_version = 1
            environment = "lab"

            [controller]
            listen = "0.0.0.0:7443"

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
        );
        let report = validate(&config);
        assert!(report.is_ok(), "{:?}", report.issues);
        assert_eq!(report.issues.len(), 0, "{:?}", report.issues);
    }

    #[test]
    fn every_problem_is_reported_not_just_the_first() {
        let config = parse(
            r#"
            config_version = 1
            environment = ""

            [controller]
            listen = "not-an-endpoint"

            [[entities]]
            type = "spaceship"
            name = "x"

            [[entities]]
            type = "host"
            name = ""
            "#,
        );
        let report = validate(&config);
        assert!(!report.is_ok());
        assert!(report.errors().count() >= 4, "{:?}", report.issues);
    }

    #[test]
    fn duplicate_entities_are_an_error() {
        let config = parse(
            r#"
            config_version = 1
            [[entities]]
            type = "host"
            name = "node-a"
            [[entities]]
            type = "host"
            name = "node-a"
            "#,
        );
        let report = validate(&config);
        assert!(
            report.errors().any(|i| i.message.contains("duplicate")),
            "{:?}",
            report.issues
        );
    }

    #[test]
    fn the_same_name_under_different_types_is_fine() {
        let config = parse(
            r#"
            config_version = 1
            [[entities]]
            type = "host"
            name = "alpha"
            [[entities]]
            type = "storage"
            name = "alpha"
            "#,
        );
        assert!(validate(&config).is_ok());
    }

    #[test]
    fn a_dependency_on_an_undeclared_entity_warns_but_does_not_fail() {
        // It may legitimately be discovered from Slurm or agent registration.
        let config = parse(
            r#"
            config_version = 1
            [[dependencies]]
            from = "host/discovered-later"
            to = "storage/also-later"
            "#,
        );
        let report = validate(&config);
        assert!(report.is_ok(), "{:?}", report.issues);
        assert_eq!(report.warnings().count(), 2);
    }

    #[test]
    fn a_malformed_dependency_reference_is_an_error() {
        let config = parse(
            r#"
            config_version = 1
            [[dependencies]]
            from = "node-a"
            to = "spaceship/x"
            "#,
        );
        let report = validate(&config);
        assert_eq!(report.errors().count(), 2, "{:?}", report.issues);
    }

    #[test]
    fn bad_criticality_is_an_error() {
        let config = parse(
            r#"
            config_version = 1
            [[entities]]
            type = "host"
            name = "a"
            [[entities]]
            type = "host"
            name = "b"
            [[dependencies]]
            from = "host/a"
            to = "host/b"
            criticality = "extremely"
            "#,
        );
        assert!(validate(&config).errors().any(|i| i.location.ends_with("criticality")));
    }

    #[test]
    fn zero_peer_degree_warns_about_losing_quorum() {
        let config = parse(
            r#"
            config_version = 1
            [peer_monitoring]
            degree = 0
            "#,
        );
        let report = validate(&config);
        assert!(report.is_ok());
        assert!(report.warnings().any(|i| i.location == "peer_monitoring.degree"));
    }

    #[test]
    fn endpoint_shapes_are_checked() {
        for (listen, ok) in [
            ("0.0.0.0:7443", true),
            ("host.example:1", true),
            ("host.example:0", false),
            ("host.example:99999", false),
            ("host.example", false),
            (":7443", false),
        ] {
            let config = parse(&format!("config_version = 1\n[controller]\nlisten = \"{listen}\"\n"));
            assert_eq!(validate(&config).is_ok(), ok, "listen = {listen}");
        }
    }
}
