//! Deciding which address to tell the controller about.
//!
//! This is the first thing every peer will use and the last thing anyone
//! checks. Get it wrong and a healthy host looks unreachable, because the
//! probes are aimed at an address nobody can reach.
//!
//! Detection ranks what it finds, but ranking cannot answer the question that
//! actually matters. A host with `vlan101`, `vlan102` and `vlan103` configured is
//! reachable on all three; **which one carries traffic between cluster nodes
//! is a fact about the site, not about the host**, and no amount of inspection
//! recovers it. So detection is a starting point and `[agent] interface` is
//! the answer, and when detection has to choose between several equally
//! plausible interfaces it says so rather than quietly picking one.

use crate::agent::system::{rank_addresses, InterfaceAddress, SystemInspector};
use crate::config::AgentConfig;

/// Where the reported address came from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AddressSource {
    /// `[agent] address` named it exactly.
    Configured,
    /// `[agent] interface` named the interface.
    Interface(String),
    /// Detected, with one plausible candidate.
    Detected,
    /// Detected, with several equally plausible interfaces.
    ///
    /// Carries the interface names so an operator can be told what to choose
    /// between instead of being told merely that something is uncertain.
    Ambiguous(Vec<String>),
    /// Nothing usable was found.
    None,
    /// The configured interface has no usable address.
    ///
    /// Deliberately distinct from [`AddressSource::None`]: the operator said
    /// something specific and it did not hold, which is worth a different
    /// message.
    InterfaceEmpty(String),
}

/// Which addresses to report, and why.
#[derive(Debug, Clone, PartialEq)]
pub struct AddressChoice {
    /// The addresses to report, best first. May be empty.
    pub addresses: Vec<String>,
    /// How they were arrived at.
    pub source: AddressSource,
    /// Every address that could have been reported, ranked.
    pub candidates: Vec<InterfaceAddress>,
}

impl AddressChoice {
    /// The address peers will actually probe.
    pub fn primary(&self) -> Option<&str> {
        self.addresses.first().map(String::as_str)
    }

    /// A line worth putting in the log, or `None` when nothing is wrong.
    ///
    /// Reporting no address is not silent failure: the controller falls back
    /// to the host's name, and "we do not know where this is" must never read
    /// as "this is broken". But it should be said out loud.
    pub fn warning(&self) -> Option<String> {
        match &self.source {
            AddressSource::Ambiguous(interfaces) => Some(format!(
                "several interfaces could be the one peers reach this host on ({}); \
                 {} was chosen by name order. Set [agent] interface to say which.",
                interfaces.join(", "),
                self.primary().unwrap_or("none")
            )),
            AddressSource::InterfaceEmpty(name) => Some(format!(
                "[agent] interface = {name:?} has no usable address, so no address is \
                 being reported. The controller will fall back to this host's name. \
                 Interfaces with usable addresses: {}",
                self.interface_names().join(", ")
            )),
            AddressSource::None => Some(
                "no usable address was found, so the controller will fall back to \
                 this host's name"
                    .to_string(),
            ),
            AddressSource::Configured | AddressSource::Interface(_) | AddressSource::Detected => None,
        }
    }

    /// The distinct interfaces among the candidates, in rank order.
    pub fn interface_names(&self) -> Vec<String> {
        let mut names: Vec<String> = Vec::new();
        for candidate in &self.candidates {
            if !names.contains(&candidate.interface) {
                names.push(candidate.interface.clone());
            }
        }
        names
    }
}

/// Work out which addresses this host should report.
pub fn choose(inspector: &dyn SystemInspector, config: &AgentConfig) -> AddressChoice {
    let all = inspector.interface_addresses();
    let candidates = usable(&all);

    // An address given by hand is taken as given. It is not checked against
    // what was detected: an operator may be describing an address that arrives
    // later, or one this host cannot see itself behind NAT.
    if let Some(address) = &config.address {
        return AddressChoice {
            addresses: vec![address.clone()],
            source: AddressSource::Configured,
            candidates,
        };
    }

    if let Some(interface) = &config.interface {
        let on_interface: Vec<String> = candidates
            .iter()
            .filter(|c| &c.interface == interface)
            .map(|c| c.address.clone())
            .collect();

        return if on_interface.is_empty() {
            // Falling back to another interface here would report an address
            // on a network the operator did not choose, which is the fault
            // this setting exists to prevent.
            AddressChoice {
                addresses: Vec::new(),
                source: AddressSource::InterfaceEmpty(interface.clone()),
                candidates,
            }
        } else {
            AddressChoice {
                addresses: on_interface,
                source: AddressSource::Interface(interface.clone()),
                candidates,
            }
        };
    }

    let addresses: Vec<String> = candidates.iter().map(|c| c.address.clone()).collect();
    let plausible = plausible_interfaces(&candidates);

    let source = match (addresses.is_empty(), plausible.len()) {
        (true, _) => AddressSource::None,
        (false, 0 | 1) => AddressSource::Detected,
        (false, _) => AddressSource::Ambiguous(plausible),
    };

    AddressChoice {
        addresses,
        source,
        candidates,
    }
}

/// The candidates, filtered and ranked, keeping their interface names.
fn usable(all: &[InterfaceAddress]) -> Vec<InterfaceAddress> {
    let ranked = rank_addresses(all.to_vec());
    ranked
        .into_iter()
        .filter_map(|address| {
            all.iter()
                .find(|entry| entry.address == address)
                .map(|entry| InterfaceAddress::new(&entry.interface, address))
        })
        .collect()
}

/// Interfaces that could each equally be the one peers use.
///
/// Only the ones detection ranks as physical count. A container bridge or a
/// tunnel alongside one real interface is not a genuine choice, and warning
/// about it on every node with Docker installed would train people to ignore
/// the warning that matters.
fn plausible_interfaces(candidates: &[InterfaceAddress]) -> Vec<String> {
    let mut names: Vec<String> = Vec::new();
    for candidate in candidates {
        if crate::agent::system::is_virtual_interface_name(&candidate.interface) {
            continue;
        }
        if !names.contains(&candidate.interface) {
            names.push(candidate.interface.clone());
        }
    }
    names
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::system::FakeInspector;

    fn config() -> AgentConfig {
        AgentConfig::default()
    }

    /// A host as a real site had it: three VLANs, a tunnel, and a physical
    /// interface carrying only a link-local address.
    fn multi_vlan_host() -> FakeInspector {
        FakeInspector::bare()
            .without_addresses()
            .with_interface_address("lo", "127.0.0.1")
            .with_interface_address("eno1", "fe80::5054:ff:fe12:3456")
            .with_interface_address("vlan103", "192.0.2.32")
            .with_interface_address("vlan102", "192.0.2.20")
            .with_interface_address("vlan101", "192.0.2.10")
            .with_interface_address("wg0", "10.0.0.1")
    }

    #[test]
    fn loopback_and_link_local_are_never_reported() {
        let choice = choose(&multi_vlan_host(), &config());
        assert!(!choice.addresses.iter().any(|a| a.starts_with("127.")));
        assert!(!choice.addresses.iter().any(|a| a.starts_with("fe80")));
    }

    #[test]
    fn a_tunnel_ranks_below_a_real_interface() {
        let choice = choose(&multi_vlan_host(), &config());
        let wireguard = choice.addresses.iter().position(|a| a == "10.0.0.1");
        let vlan = choice.addresses.iter().position(|a| a == "192.0.2.20");
        assert!(vlan < wireguard, "{:?}", choice.addresses);
    }

    #[test]
    fn several_vlans_are_reported_as_a_choice_not_guessed_at() {
        // The case detection genuinely cannot solve. Which VLAN carries
        // traffic between nodes is a fact about the site.
        let choice = choose(&multi_vlan_host(), &config());
        let AddressSource::Ambiguous(interfaces) = &choice.source else {
            panic!("expected an ambiguous choice, got {:?}", choice.source);
        };
        assert!(interfaces.contains(&"vlan102".to_string()), "{interfaces:?}");
        assert!(interfaces.contains(&"vlan101".to_string()), "{interfaces:?}");

        let warning = choice.warning().expect("a warning");
        assert!(warning.contains("[agent] interface"), "{warning}");
    }

    #[test]
    fn naming_the_interface_settles_it() {
        let mut config = config();
        config.interface = Some("vlan102".into());

        let choice = choose(&multi_vlan_host(), &config);
        assert_eq!(choice.primary(), Some("192.0.2.20"));
        assert_eq!(choice.source, AddressSource::Interface("vlan102".into()));
        assert!(choice.warning().is_none());
    }

    #[test]
    fn a_named_interface_with_no_address_reports_nothing_rather_than_the_wrong_thing() {
        // eno1 carries only a link-local address. Falling back to vlan101
        // would aim every peer at a network the operator did not choose, which
        // is the fault this setting exists to prevent.
        let mut config = config();
        config.interface = Some("eno1".into());

        let choice = choose(&multi_vlan_host(), &config);
        assert!(choice.addresses.is_empty(), "{:?}", choice.addresses);

        let warning = choice.warning().expect("a warning");
        assert!(warning.contains("eno1"), "{warning}");
        assert!(
            warning.contains("vlan102"),
            "it should list the real options: {warning}"
        );
    }

    #[test]
    fn an_explicit_address_wins_over_everything() {
        let mut config = config();
        config.address = Some("203.0.113.7".into());
        config.interface = Some("vlan101".into());

        let choice = choose(&multi_vlan_host(), &config);
        assert_eq!(choice.primary(), Some("203.0.113.7"));
        assert_eq!(choice.source, AddressSource::Configured);
    }

    #[test]
    fn one_real_interface_beside_container_bridges_is_not_ambiguous() {
        // Otherwise every node with Docker installed warns, and the warning
        // that matters gets ignored with the rest.
        let inspector = FakeInspector::bare()
            .without_addresses()
            .with_interface_address("eth0", "10.1.0.5")
            .with_interface_address("docker0", "172.17.0.1")
            .with_interface_address("br-abc123", "172.18.0.1");

        let choice = choose(&inspector, &config());
        assert_eq!(choice.primary(), Some("10.1.0.5"));
        assert_eq!(choice.source, AddressSource::Detected);
        assert!(choice.warning().is_none());
    }

    #[test]
    fn a_host_with_nothing_usable_says_so() {
        let inspector = FakeInspector::bare()
            .without_addresses()
            .with_interface_address("lo", "127.0.0.1");

        let choice = choose(&inspector, &config());
        assert!(choice.addresses.is_empty());
        assert_eq!(choice.source, AddressSource::None);
        assert!(choice.warning().is_some());
    }
}
