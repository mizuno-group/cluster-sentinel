//! `sentinel explain` has to be right, because its whole purpose is to be
//! believed by someone who cannot check it against the source.

use sentinel::capability::catalog as capabilities;
use sentinel::config::Config;
use sentinel::probes::catalog as probes;
use sentinel::probes::ExecutionMode;

#[test]
fn every_capability_says_how_it_is_decided() {
    let text = sentinel::cli::explain::render_capabilities();
    for entry in capabilities::catalog() {
        assert!(text.contains(entry.name), "{} is missing", entry.name);
        assert!(
            text.contains(entry.detection),
            "{} does not say how it is decided",
            entry.name
        );
    }
}

#[test]
fn every_probe_says_what_it_runs() {
    let text = sentinel::cli::explain::render_probes(&Config::default());
    for entry in probes::catalog() {
        assert!(text.contains(entry.id()), "{} is missing", entry.id());
        assert!(
            text.contains(entry.mechanism),
            "{} does not say what it runs",
            entry.id()
        );
    }
}

#[test]
fn a_capability_that_enables_no_probe_says_so_rather_than_looking_empty() {
    // `observer.peer` gates peer assignment, not a probe. A blank there reads
    // as a gap in the tool rather than a fact about the capability.
    let text = sentinel::cli::explain::render_capabilities();
    assert!(text.contains("no probe"), "{text}");
}

#[test]
fn a_disabled_probe_is_marked_as_disabled() {
    // Otherwise the table describes a probe that is not running, which is
    // worse than not listing it.
    let config = Config::from_toml(
        "config_version = 1\nenvironment = \"lab\"\n\n[probes]\n\"gpu.nvidia\" = { enabled = false }\n",
        std::path::Path::new("test.toml"),
    )
    .expect("config");

    let text = sentinel::cli::explain::render_probes(&config);
    let gpu_line = text
        .lines()
        .find(|l| l.starts_with(sentinel::probes::gpu::PROBE_ID))
        .expect("the gpu probe is listed");
    assert!(gpu_line.contains("DISABLED"), "{gpu_line}");
}

#[test]
fn the_cadence_shown_is_the_configured_one() {
    // The table is read to decide whether a probe is too frequent. Showing
    // the compiled default when the site has changed it would be a lie of
    // exactly the kind this command exists to prevent.
    let config = Config::from_toml(
        "config_version = 1\nenvironment = \"lab\"\n\n[probes]\n\"network.tcp\" = { interval = \"45s\" }\n",
        std::path::Path::new("test.toml"),
    )
    .expect("config");

    let text = sentinel::cli::explain::render_probes(&config);
    assert!(text.contains("every 45s"), "{text}");
}

#[test]
fn local_and_remote_probes_are_not_confused() {
    // An `Either` probe is run *at* a host from elsewhere. Listing it under
    // "from itself" would claim an agent asks whether it answers, which it
    // can only ever say yes to.
    let local: Vec<&str> = probes::catalog()
        .iter()
        .filter(|p| p.definition.execution_mode == ExecutionMode::Local)
        .map(|p| p.id())
        .map(|s| Box::leak(s.to_string().into_boxed_str()) as &str)
        .collect();

    assert!(local.contains(&sentinel::probes::host::PROBE_ID));
    assert!(!local.contains(&sentinel::probes::ssh::PROBE_ID));
    assert!(!local.contains(&sentinel::probes::network::PROBE_ID));
    assert!(!local.contains(&sentinel::probes::sentinel_rpc::PROBE_ID));
}
