//! Which address a host reports itself at.
//!
//! This is the first thing every peer uses and the last thing anyone checks.
//! Report the wrong one and a perfectly healthy host looks unreachable,
//! because every probe is aimed somewhere nobody can reach — and the
//! diagnosis, being about reachability, is exactly the one Sentinel is
//! supposed to get right.
//!
//! The host modelled here is a real one: three VLANs, a WireGuard tunnel, and
//! a physical interface carrying only a link-local address. Nothing about the
//! machine says which VLAN carries traffic between cluster nodes.

use std::sync::Arc;
use std::time::Duration;

use sentinel::agent::addressing::{choose, AddressSource};
use sentinel::agent::system::FakeInspector;
use sentinel::agent::{Agent, ControllerClient, Spool, SpoolLimits};
use sentinel::config::Config;
use sentinel::protocol::ClusterCredential;

/// `ip -o addr show` on a host with several VLANs, as reported by an operator.
fn parent_host() -> FakeInspector {
    FakeInspector::bare()
        .without_addresses()
        .with_interface_address("lo", "127.0.0.1")
        .with_interface_address("lo", "::1")
        .with_interface_address("eno1", "fe80::5054:ff:fe12:3456")
        .with_interface_address("vlan103", "192.0.2.32")
        .with_interface_address("vlan103", "fe80::5054:ff:fe12:3456")
        .with_interface_address("vlan102", "192.0.2.20")
        .with_interface_address("vlan101", "192.0.2.10")
        .with_interface_address("wg0", "10.0.0.1")
}

fn config(agent_section: &str) -> Config {
    let text = format!(
        "config_version = 1\nenvironment = \"lab\"\n\n[agent]\ncontroller_address = \"127.0.0.1:1\"\n{agent_section}"
    );
    Config::from_toml(&text, std::path::Path::new("test.toml")).expect("config")
}

async fn agent(config: &Config) -> Agent {
    Agent::new(
        config,
        Arc::new(parent_host()),
        ControllerClient::new(
            "127.0.0.1:1",
            &ClusterCredential::new("0123456789abcdef0123456789abcdef"),
            Duration::from_millis(50),
        )
        .expect("client"),
        Spool::open_in_memory(SpoolLimits::default()).await.expect("spool"),
    )
    .expect("agent")
}

#[test]
fn nothing_unreachable_is_ever_offered_to_peers() {
    let choice = choose(&parent_host(), &config("").agent);

    for address in &choice.addresses {
        assert!(!address.starts_with("127."), "loopback offered: {address}");
        assert_ne!(address, "::1", "loopback offered");
        assert!(!address.starts_with("fe80"), "link-local offered: {address}");
    }
    assert!(!choice.addresses.is_empty(), "nothing at all was offered");
}

#[test]
fn a_non_loopback_address_on_the_loopback_interface_is_not_offered() {
    // The case that started this: WSL puts 10.255.255.254/32 on `lo`. It is
    // not a loopback *address*, so filtering by address alone keeps it, and it
    // sorts first — leaving every peer probing something reachable by nobody.
    let inspector = FakeInspector::bare()
        .without_addresses()
        .with_interface_address("lo", "10.255.255.254")
        .with_interface_address("eth0", "172.24.173.73");

    let choice = choose(&inspector, &config("").agent);
    assert_eq!(choice.primary(), Some("172.24.173.73"));
    assert!(!choice.addresses.contains(&"10.255.255.254".to_string()));
}

#[test]
fn several_vlans_are_declared_ambiguous_rather_than_guessed_at() {
    // Detection cannot know which VLAN carries traffic between nodes: that is
    // a fact about the site. Picking one quietly would be wrong two times in
    // three here, and wrong invisibly.
    let choice = choose(&parent_host(), &config("").agent);

    let AddressSource::Ambiguous(interfaces) = &choice.source else {
        panic!("expected ambiguity, got {:?}", choice.source);
    };
    for expected in ["vlan101", "vlan102", "vlan103"] {
        assert!(interfaces.contains(&expected.to_string()), "{interfaces:?}");
    }

    let warning = choice.warning().expect("a warning");
    assert!(warning.contains("[agent] interface"), "{warning}");
    assert!(warning.contains("vlan102"), "the options must be named: {warning}");
}

#[tokio::test]
async fn naming_the_interface_decides_what_the_agent_registers() {
    // The end-to-end claim: one line of configuration, and the address that
    // reaches the controller is the one on the cluster's own network.
    let agent = agent(&config("interface = \"vlan102\"\n")).await;
    let registration = agent.registration();

    assert_eq!(registration.addresses, vec!["192.0.2.20".to_string()]);
}

#[tokio::test]
async fn without_it_the_agent_registers_a_plausible_but_unverified_address() {
    // Still useful, still honest: something is registered, and `doctor` says
    // it was a guess. What must not happen is registering a loopback or
    // link-local address, which no configuration could rescue.
    let agent = agent(&config("")).await;
    let registration = agent.registration();

    // Only the first is probed, so only the first has to be right; the rest
    // are kept because they are informative, the tunnel ranked last.
    let first = registration.addresses.first().expect("an address");
    assert!(first.starts_with("192.0.2."), "{first}");
    assert_eq!(registration.addresses.last().map(String::as_str), Some("10.0.0.1"));
    assert!(agent.address_choice().warning().is_some());
}

#[tokio::test]
async fn an_explicit_address_is_registered_verbatim() {
    // For a host reached through NAT, or one whose address arrives later: the
    // operator's answer is not checked against what the host can see.
    let agent = agent(&config("address = \"203.0.113.9\"\n")).await;

    assert_eq!(agent.registration().addresses, vec!["203.0.113.9".to_string()]);
    assert!(agent.address_choice().warning().is_none());
}

#[tokio::test]
async fn a_named_interface_without_an_address_registers_nothing() {
    // eno1 has only a link-local address. Falling back to a VLAN would aim
    // every peer at a network the operator did not choose — the exact fault
    // this setting exists to prevent. Reporting nothing lets the controller
    // fall back to the host's name, which is visible and correctable.
    let agent = agent(&config("interface = \"eno1\"\n")).await;

    assert!(agent.registration().addresses.is_empty());
    let warning = agent.address_choice().warning().expect("a warning");
    assert!(warning.contains("eno1"), "{warning}");
}
