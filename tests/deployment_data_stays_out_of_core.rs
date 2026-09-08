//! Guards the rule that keeps this codebase reusable: deployment specifics are
//! data, never source (SPEC.md §185, §186, IMPLEMENTATION.md §101).
//!
//! Fixtures, configuration, `dev/compose/` and tests may all name the current
//! cluster. `src/` may not.

use std::path::{Path, PathBuf};

/// Names from the initial production deployment. If one of these appears in
/// `src/`, some logic has been tied to today's topology.
const DEPLOYMENT_NAMES: &[&str] = &[
    "creator2",
    "creator3",
    "creator4",
    "creator5",
    "creator6",
    "creator7",
    "andre01",
    "david01",
    "david02",
    "grace01",
    "grace02",
    "preproc01",
    "hiegm5",
    "filesrv01",
    "filesrv02",
    "mizuno_cluster",
];

/// Names that are ordinary English words elsewhere in the code (`parent` is a
/// path component). Those are only a problem as string literals.
const DEPLOYMENT_NAMES_AS_LITERALS: &[&str] = &["parent"];

fn source_files() -> Vec<PathBuf> {
    let mut files = Vec::new();
    collect(Path::new("src"), &mut files);
    files
}

fn collect(dir: &Path, files: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect(&path, files);
        } else if path.extension().is_some_and(|ext| ext == "rs") {
            files.push(path);
        }
    }
}

#[test]
fn no_production_host_name_appears_in_core_source() {
    let mut violations = Vec::new();

    for file in source_files() {
        let text = std::fs::read_to_string(&file).expect("read source file");
        for (number, line) in text.lines().enumerate() {
            for name in DEPLOYMENT_NAMES {
                if line.contains(name) {
                    violations.push(format!("{}:{}: {name}", file.display(), number + 1));
                }
            }
            for name in DEPLOYMENT_NAMES_AS_LITERALS {
                if line.contains(&format!("\"{name}\"")) {
                    violations.push(format!("{}:{}: \"{name}\"", file.display(), number + 1));
                }
            }
        }
    }

    assert!(
        violations.is_empty(),
        "deployment-specific names belong in fixtures, config or tests, not in src/:\n{}",
        violations.join("\n")
    );
}

#[test]
fn the_guard_actually_scans_something() {
    // A silently empty file list would make the test above pass vacuously.
    let files = source_files();
    assert!(
        files.len() > 10,
        "expected to scan the source tree, found {} files",
        files.len()
    );
}

/// Hosts that name no real machine: a bind-all address, loopback, or the
/// reserved documentation name.
fn is_placeholder_host(host: &str) -> bool {
    host.is_empty()
        // A format placeholder, or the marker a generated file uses for the
        // lines an operator must fill in. Neither names a machine.
        || host.contains('{')
        || host.to_ascii_uppercase().contains("CHANGE-ME")
        || host == "0.0.0.0"
        || host == "127.0.0.1"
        || host == "localhost"
        || host.contains("example")
        // RFC 5737 documentation ranges: reserved precisely so that examples
        // can never name a machine that exists.
        || host.starts_with("192.0.2.")
        || host.starts_with("198.51.100.")
        || host.starts_with("203.0.113.")
}

/// The host part immediately preceding a `:port` occurrence, if any.
fn host_before_port(line: &str, port: &str) -> Option<String> {
    let index = line.find(port)?;
    let host: String = line[..index]
        .chars()
        .rev()
        .take_while(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_' | '{' | '}'))
        .collect();
    Some(host.chars().rev().collect())
}

#[test]
fn no_controller_host_name_is_hard_coded() {
    // SPEC.md §42: which host runs the controller is deployment configuration,
    // not a constant. A bind-all default is fine; a real host name is not.
    for file in source_files() {
        let text = std::fs::read_to_string(&file).expect("read source file");
        for (number, line) in text.lines().enumerate() {
            let Some(host) = host_before_port(line, ":7443") else {
                continue;
            };
            assert!(
                is_placeholder_host(&host),
                "{}:{}: a controller host name must come from configuration, found {host:?}: {line}",
                file.display(),
                number + 1
            );
        }
    }
}

#[test]
fn the_endpoint_guard_recognises_a_hard_coded_host() {
    assert_eq!(
        host_before_port("address = \"controller-a:7443\"", ":7443").as_deref(),
        Some("controller-a")
    );
    assert!(!is_placeholder_host("controller-a"));
    assert!(is_placeholder_host(
        &host_before_port("listen = \"0.0.0.0:7443\"", ":7443").unwrap()
    ));
    assert!(is_placeholder_host(
        &host_before_port("(\":7443\", false),", ":7443").unwrap()
    ));
}
