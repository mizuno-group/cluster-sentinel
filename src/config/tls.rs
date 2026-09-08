//! Transport security for the controller API.
//!
//! The credential agents present is a bearer token: whoever reads it off the
//! wire can impersonate every agent in the cluster. Until now the only answer
//! was "put a reverse proxy in front of it", which is a task list handed to the
//! operator rather than a property of the system.
//!
//! Everything here is optional and additive, because the shapes real sites
//! need are genuinely different:
//!
//! * **Nothing set.** Plain HTTP, as before. Still the right answer on an
//!   isolated management network, and still what the pseudo-cluster uses.
//! * **`cert` and `key` on the controller.** TLS, with agents trusting the
//!   system roots or a CA named by `ca`.
//! * **`client_ca` as well.** Mutual TLS: an agent must present a certificate
//!   signed by that CA before it can even offer its token. This is the shape
//!   that survives a leaked token.
//!
//! There is deliberately no "generate a certificate for me". A monitoring
//! system minting its own trust anchors would be one more private CA nobody
//! audits, and sites that require TLS invariably already have a way to issue
//! certificates.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// TLS settings, for whichever side of the connection this node is on.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TlsConfig {
    /// Server certificate chain, PEM. Enables TLS on the controller.
    #[serde(default)]
    pub cert: Option<PathBuf>,
    /// Server private key, PEM (PKCS#8, PKCS#1 or SEC1).
    #[serde(default)]
    pub key: Option<PathBuf>,
    /// CA that client certificates must be signed by, PEM.
    ///
    /// Setting this makes a client certificate **required**, not optional.
    /// Optional client authentication is a setting that reads like security
    /// and provides none, since an attacker simply declines to present one.
    #[serde(default)]
    pub client_ca: Option<PathBuf>,
    /// CA to trust when connecting to the controller, PEM.
    ///
    /// Added to the system roots rather than replacing them, so a site using a
    /// private CA for the controller and public certificates elsewhere does not
    /// have to choose.
    #[serde(default)]
    pub ca: Option<PathBuf>,
    /// Client certificate chain to present to the controller, PEM.
    #[serde(default)]
    pub client_cert: Option<PathBuf>,
    /// Private key for `client_cert`, PEM.
    #[serde(default)]
    pub client_key: Option<PathBuf>,
    /// Name to verify the controller's certificate against.
    ///
    /// For the case where agents reach the controller by address but its
    /// certificate names a host.
    #[serde(default)]
    pub server_name: Option<String>,
    /// Connect without verifying the controller's certificate.
    ///
    /// This turns TLS into obfuscation: an attacker who can redirect the
    /// connection can present any certificate and read every token that
    /// crosses it. It exists because people bring up a cluster before they
    /// bring up a PKI, and a documented switch is safer than the alternative
    /// of going back to plain HTTP and forgetting.
    #[serde(default)]
    pub insecure_skip_verify: bool,
}

impl TlsConfig {
    /// Whether this node should serve TLS.
    pub fn serves_tls(&self) -> bool {
        self.cert.is_some() && self.key.is_some()
    }

    /// Whether this node requires client certificates.
    pub fn requires_client_certificates(&self) -> bool {
        self.client_ca.is_some()
    }

    /// Whether this node presents a client certificate.
    pub fn presents_client_certificate(&self) -> bool {
        self.client_cert.is_some() && self.client_key.is_some()
    }

    /// Whether anything here changes client behaviour.
    pub fn affects_client(&self) -> bool {
        self.ca.is_some()
            || self.presents_client_certificate()
            || self.server_name.is_some()
            || self.insecure_skip_verify
    }

    /// Configuration mistakes that would otherwise appear as a handshake
    /// failure at three in the morning.
    pub fn problems(&self) -> Vec<String> {
        let mut problems = Vec::new();
        if self.cert.is_some() != self.key.is_some() {
            problems.push("tls.cert and tls.key must be set together".to_string());
        }
        if self.client_cert.is_some() != self.client_key.is_some() {
            problems.push("tls.client_cert and tls.client_key must be set together".to_string());
        }
        if self.client_ca.is_some() && !self.serves_tls() {
            problems.push(
                "tls.client_ca requires tls.cert and tls.key: client certificates \
                 can only be verified on a TLS listener"
                    .to_string(),
            );
        }
        if self.insecure_skip_verify && self.ca.is_some() {
            problems.push("tls.insecure_skip_verify makes tls.ca pointless: nothing is verified".to_string());
        }
        problems
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn path(p: &str) -> Option<PathBuf> {
        Some(PathBuf::from(p))
    }

    #[test]
    fn the_default_is_plain_http() {
        let tls = TlsConfig::default();
        assert!(!tls.serves_tls());
        assert!(!tls.affects_client());
        assert!(tls.problems().is_empty());
    }

    #[test]
    fn a_certificate_without_a_key_is_caught_before_startup() {
        let tls = TlsConfig {
            cert: path("/tls/server.crt"),
            ..TlsConfig::default()
        };
        assert!(tls.problems().iter().any(|p| p.contains("tls.key")));
    }

    #[test]
    fn a_client_key_without_a_certificate_is_caught_too() {
        let tls = TlsConfig {
            client_key: path("/tls/agent.key"),
            ..TlsConfig::default()
        };
        assert!(tls.problems().iter().any(|p| p.contains("client_cert")));
    }

    #[test]
    fn requiring_client_certificates_without_serving_tls_is_refused() {
        // It would otherwise silently do nothing, which is the worst outcome
        // for a setting whose whole purpose is to refuse people.
        let tls = TlsConfig {
            client_ca: path("/tls/ca.crt"),
            ..TlsConfig::default()
        };
        assert!(tls.problems().iter().any(|p| p.contains("client_ca")));
    }

    #[test]
    fn skipping_verification_while_naming_a_ca_is_flagged_as_contradictory() {
        let tls = TlsConfig {
            ca: path("/tls/ca.crt"),
            insecure_skip_verify: true,
            ..TlsConfig::default()
        };
        assert!(!tls.problems().is_empty());
    }

    #[test]
    fn a_full_mutual_tls_setup_has_no_problems() {
        let tls = TlsConfig {
            cert: path("/tls/server.crt"),
            key: path("/tls/server.key"),
            client_ca: path("/tls/ca.crt"),
            ca: path("/tls/ca.crt"),
            client_cert: path("/tls/agent.crt"),
            client_key: path("/tls/agent.key"),
            server_name: Some("controller.example".into()),
            insecure_skip_verify: false,
        };
        assert!(tls.problems().is_empty(), "{:?}", tls.problems());
        assert!(tls.serves_tls());
        assert!(tls.requires_client_certificates());
        assert!(tls.presents_client_certificate());
    }
}
