//! The Ansible role ships a version number, and it must be this one.
//!
//! It drifted once, in the way version numbers written in two places always
//! do: the role kept pointing at v0.3.0 across two releases, so it installed
//! a binary predating the flags it then used, and the failure surfaced as
//! `unexpected argument '--binary'` -- an error about command-line parsing,
//! on nodes that had just been given the wrong binary.

use std::path::Path;

fn defaults() -> String {
    std::fs::read_to_string(Path::new("deploy/ansible/roles/sentinel/defaults/main.yml"))
        .expect("the role's defaults are part of the repository")
}

/// The value of a `key: value` line, unquoted.
fn setting(text: &str, key: &str) -> Option<String> {
    text.lines()
        .map(str::trim)
        .find(|line| line.starts_with(&format!("{key}:")))
        .map(|line| line[key.len() + 1..].trim().trim_matches('"').to_string())
}

#[test]
fn the_role_installs_the_version_this_repository_builds() {
    let expected = format!("v{}", env!("CARGO_PKG_VERSION"));
    let actual = setting(&defaults(), "sentinel_version").expect("sentinel_version is set");

    assert_eq!(
        actual, expected,
        "the Ansible role installs {actual} while this repository is {expected}; \
         a release that does not update the role hands nodes a binary older than \
         the role's own requirements"
    );
}

#[test]
fn the_role_states_a_minimum_it_can_work_with() {
    // The default may be pinned back deliberately. The floor may not, because
    // below it the role uses flags the binary does not have.
    let minimum = setting(&defaults(), "sentinel_minimum_version").expect("sentinel_minimum_version is set");
    assert!(
        !minimum.starts_with('v'),
        "the minimum is compared with Ansible's version test, which wants no leading v: {minimum}"
    );

    let default = setting(&defaults(), "sentinel_version").expect("sentinel_version is set");
    assert!(
        version_parts(default.trim_start_matches('v')) >= version_parts(&minimum),
        "the default {default} is below the stated minimum {minimum}"
    );
}

/// A version as numbers, so 0.3.10 sorts after 0.3.2.
///
/// Comparing these as strings is wrong the moment a component reaches two
/// digits, and it fails in the direction that blocks a release rather than
/// letting a bad one through -- but it still fails.
fn version_parts(version: &str) -> Vec<u32> {
    version.split('.').map(|part| part.parse().unwrap_or(0)).collect()
}

#[test]
fn versions_compare_as_numbers_not_text() {
    // 0.3.10 is newer than 0.3.2, and a string comparison says otherwise.
    assert!(version_parts("0.3.10") > version_parts("0.3.2"));
    assert!(version_parts("0.4.0") > version_parts("0.3.99"));
    assert_eq!(version_parts("1.2.3"), vec![1, 2, 3]);
}

#[test]
fn every_flag_the_role_passes_exists_in_this_binary() {
    // The drift that started this was a flag the role used and the installed
    // binary did not have. The repository knows both, so it can check.
    let tasks = std::fs::read_to_string(Path::new("deploy/ansible/roles/sentinel/tasks/main.yml"))
        .expect("the role's tasks are part of the repository");

    for flag in ["--binary", "--config", "--json"] {
        if !tasks.contains(flag) {
            continue;
        }
        let help = std::process::Command::new(env!("CARGO_BIN_EXE_sentinel"))
            .args(["install", "--help"])
            .output()
            .expect("run sentinel");
        let text = String::from_utf8_lossy(&help.stdout);
        if flag == "--binary" {
            assert!(
                text.contains("--binary"),
                "the role passes --binary and the binary has no such flag"
            );
        }
    }
}
