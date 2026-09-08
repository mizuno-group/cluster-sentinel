//! The shipped configuration templates must actually be valid.
//!
//! A template that no longer parses is worse than no template: an operator
//! copies it, hits an error during a deployment window, and has no way to tell
//! whether the mistake is theirs. Templates rot silently as the schema moves,
//! so this test moves with it.

use std::path::{Path, PathBuf};

use sentinel::config::{validate, Config};

fn templates_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("docs/templates")
}

fn templates() -> Vec<PathBuf> {
    let mut found: Vec<PathBuf> = std::fs::read_dir(templates_dir())
        .expect("templates directory")
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "toml"))
        .collect();
    found.sort();
    found
}

fn load(path: &Path) -> Config {
    let text = std::fs::read_to_string(path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
    Config::from_toml(&text, path).unwrap_or_else(|e| panic!("{} does not parse: {e}", path.display()))
}

#[test]
fn the_templates_directory_is_not_empty() {
    // A vacuous pass here would let every other assertion below pass too.
    assert!(templates().len() >= 4, "found {:?}", templates());
}

#[test]
fn every_template_parses() {
    for path in templates() {
        load(&path);
    }
}

#[test]
fn every_template_passes_config_check() {
    // Exactly what an operator runs before starting the service. If this fails
    // for them, they cannot tell our mistake from theirs.
    for path in templates() {
        let report = validate(&load(&path));
        let errors: Vec<_> = report.errors().collect();
        assert!(
            errors.is_empty(),
            "{} would fail `sentinel config check`: {errors:?}",
            path.display()
        );
    }
}

#[test]
fn the_controller_template_declares_a_workable_storage_topology() {
    // The dependency graph is the part an operator is most likely to get wrong
    // and least likely to notice, because nothing breaks until a fileserver
    // does. The template has to model it correctly.
    let config = load(&templates_dir().join("controller.toml"));

    assert!(
        config.entities.iter().any(|e| e.entity_type == "scheduler"),
        "a scheduler entity, so the control plane is distinct from its host"
    );
    assert!(
        config.entities.iter().filter(|e| e.entity_type == "storage").count() >= 2,
        "two storage domains, so a shared failure can be told from a client-local one"
    );

    let provides = config
        .dependencies
        .iter()
        .filter(|d| d.dependency_type == "provides")
        .count();
    let uses = config
        .dependencies
        .iter()
        .filter(|d| d.dependency_type == "uses_storage")
        .count();
    assert!(provides >= 2, "each storage is provided by a fileserver");
    assert!(uses >= 3, "and clients are spread across both, not all on one");
}

#[test]
fn the_ssh_port_template_actually_sets_an_ssh_port() {
    // This template exists for one reason; if it stops doing that, it is
    // actively misleading.
    let config = load(&templates_dir().join("agent-nonstandard-ssh.toml"));
    assert_eq!(config.agent.ssh_port, Some(2222));
}

#[test]
fn the_agent_templates_point_somewhere_and_observe() {
    for name in ["agent.toml", "agent-nonstandard-ssh.toml"] {
        let config = load(&templates_dir().join(name));
        assert!(
            config.agent.controller_address.is_some(),
            "{name} must name a controller"
        );
        assert_eq!(
            config.capabilities.get("observer.peer"),
            Some(&sentinel::capability::CapabilityOverride::Force),
            "{name}: without observers, no reachability diagnosis is possible"
        );
    }
}

#[test]
fn no_template_ships_a_credential() {
    // Templates get copied verbatim. A secret in one is a secret in every
    // deployment that used it.
    for path in templates() {
        let text = std::fs::read_to_string(&path).expect("read");
        for forbidden in ["token =", "SENTINEL_TOKEN=", "password", "secret ="] {
            assert!(
                !text.contains(forbidden),
                "{} appears to contain a credential ({forbidden})",
                path.display()
            );
        }
    }
}

#[test]
fn placeholders_are_obvious_rather_than_plausible() {
    // A template with a real-looking hostname gets deployed unedited. One that
    // says CHANGE-ME does not.
    let config = load(&templates_dir().join("controller.toml"));
    assert!(config.environment.contains("CHANGE-ME"), "{}", config.environment);

    let agent = load(&templates_dir().join("agent.toml"));
    assert!(
        agent
            .agent
            .controller_address
            .as_deref()
            .is_some_and(|a| a.contains("CHANGE-ME")),
        "{:?}",
        agent.agent.controller_address
    );
}

#[test]
fn no_template_names_a_real_production_host() {
    // The same rule the source tree is held to: deployment specifics are the
    // operator's, not ours.
    let deployment_names = [
        "creator2",
        "creator3",
        "andre01",
        "david01",
        "grace01",
        "preproc01",
        "hiegm5",
        "filesrv01",
        "mizuno_cluster",
    ];

    for path in templates() {
        let text = std::fs::read_to_string(&path).expect("read");
        for name in deployment_names {
            assert!(
                !text.contains(name),
                "{} names a production host ({name}); templates must use placeholders",
                path.display()
            );
        }
    }
}
