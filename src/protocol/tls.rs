//! Turning [`TlsConfig`] into working rustls and reqwest configuration.
//!
//! Failures here are values with paths in them. A TLS setup that goes wrong
//! goes wrong at startup, in a message naming the file that could not be read,
//! rather than as a handshake that mysteriously does not complete.

use std::fs;
use std::io::BufReader;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use rustls::{RootCertStore, ServerConfig};
use thiserror::Error;

use crate::config::TlsConfig;

/// Why TLS could not be set up.
#[derive(Debug, Error)]
pub enum TlsError {
    /// A file named in the configuration could not be read.
    #[error("cannot read {what} at {path}: {source}")]
    Read {
        /// Which setting the file came from.
        what: &'static str,
        /// The path that failed.
        path: PathBuf,
        /// The underlying error.
        #[source]
        source: std::io::Error,
    },
    /// A file was read but held nothing usable.
    #[error("{what} at {path} contains no {expected}")]
    Empty {
        /// Which setting the file came from.
        what: &'static str,
        /// The path.
        path: PathBuf,
        /// What was being looked for.
        expected: &'static str,
    },
    /// The configuration itself is contradictory.
    #[error("{0}")]
    Invalid(String),
    /// rustls refused the material.
    #[error("TLS configuration rejected: {0}")]
    Rejected(String),
}

/// Install the process-wide cryptographic provider.
///
/// rustls 0.23 requires one to be chosen explicitly when more than one could
/// be linked. Doing it once at startup turns a possible panic deep inside a
/// handshake into a no-op here.
pub fn install_crypto_provider() {
    // Ignored on purpose: a second call means someone else got there first,
    // which is exactly the outcome wanted.
    let _ = rustls::crypto::ring::default_provider().install_default();
}

/// Read PEM certificates from a file.
fn read_certificates(what: &'static str, path: &Path) -> Result<Vec<CertificateDer<'static>>, TlsError> {
    let data = fs::read(path).map_err(|source| TlsError::Read {
        what,
        path: path.to_path_buf(),
        source,
    })?;
    let certificates: Vec<CertificateDer<'static>> = rustls_pemfile::certs(&mut BufReader::new(&data[..]))
        .collect::<Result<_, _>>()
        .map_err(|source| TlsError::Read {
            what,
            path: path.to_path_buf(),
            source,
        })?;
    if certificates.is_empty() {
        return Err(TlsError::Empty {
            what,
            path: path.to_path_buf(),
            expected: "PEM certificates",
        });
    }
    Ok(certificates)
}

/// Read a PEM private key from a file.
fn read_private_key(what: &'static str, path: &Path) -> Result<PrivateKeyDer<'static>, TlsError> {
    let data = fs::read(path).map_err(|source| TlsError::Read {
        what,
        path: path.to_path_buf(),
        source,
    })?;
    rustls_pemfile::private_key(&mut BufReader::new(&data[..]))
        .map_err(|source| TlsError::Read {
            what,
            path: path.to_path_buf(),
            source,
        })?
        .ok_or_else(|| TlsError::Empty {
            what,
            path: path.to_path_buf(),
            expected: "a PEM private key",
        })
}

/// Build the rustls server configuration a controller should listen with.
///
/// Returns `Ok(None)` when the configuration asks for plain HTTP, so that
/// "no TLS" stays an ordinary outcome rather than an error path.
pub fn server_config(config: &TlsConfig) -> Result<Option<Arc<ServerConfig>>, TlsError> {
    if let Some(problem) = config.problems().into_iter().next() {
        return Err(TlsError::Invalid(problem));
    }
    let (Some(cert_path), Some(key_path)) = (&config.cert, &config.key) else {
        return Ok(None);
    };

    install_crypto_provider();

    let certificates = read_certificates("tls.cert", cert_path)?;
    let key = read_private_key("tls.key", key_path)?;

    let builder = match &config.client_ca {
        // Client certificates are required, never optional: an optional
        // requirement is one an attacker declines.
        Some(ca_path) => {
            let mut roots = RootCertStore::empty();
            for certificate in read_certificates("tls.client_ca", ca_path)? {
                roots
                    .add(certificate)
                    .map_err(|e| TlsError::Rejected(format!("tls.client_ca: {e}")))?;
            }
            let verifier = rustls::server::WebPkiClientVerifier::builder(Arc::new(roots))
                .build()
                .map_err(|e| TlsError::Rejected(format!("tls.client_ca: {e}")))?;
            ServerConfig::builder().with_client_cert_verifier(verifier)
        }
        None => ServerConfig::builder().with_no_client_auth(),
    };

    let mut server_config = builder
        .with_single_cert(certificates, key)
        .map_err(|e| TlsError::Rejected(e.to_string()))?;
    // The API is HTTP/1.1; advertising anything else invites a client to
    // negotiate a protocol the server does not speak.
    server_config.alpn_protocols = vec![b"http/1.1".to_vec()];

    Ok(Some(Arc::new(server_config)))
}

/// Apply the client half of the configuration to a reqwest builder.
pub fn apply_client_config(
    builder: reqwest::ClientBuilder,
    config: &TlsConfig,
) -> Result<reqwest::ClientBuilder, TlsError> {
    if let Some(problem) = config.problems().into_iter().next() {
        return Err(TlsError::Invalid(problem));
    }
    if !config.affects_client() {
        return Ok(builder);
    }

    install_crypto_provider();
    let mut builder = builder;

    if let Some(ca_path) = &config.ca {
        let data = fs::read(ca_path).map_err(|source| TlsError::Read {
            what: "tls.ca",
            path: ca_path.clone(),
            source,
        })?;
        // Added to the system roots, not replacing them: a site with a private
        // CA for the controller and public certificates elsewhere should not
        // have to choose between them.
        let certificates =
            reqwest::Certificate::from_pem_bundle(&data).map_err(|e| TlsError::Rejected(format!("tls.ca: {e}")))?;
        for certificate in certificates {
            builder = builder.add_root_certificate(certificate);
        }
    }

    if let (Some(cert_path), Some(key_path)) = (&config.client_cert, &config.client_key) {
        let mut pem = fs::read(cert_path).map_err(|source| TlsError::Read {
            what: "tls.client_cert",
            path: cert_path.clone(),
            source,
        })?;
        let key = fs::read(key_path).map_err(|source| TlsError::Read {
            what: "tls.client_key",
            path: key_path.clone(),
            source,
        })?;
        pem.push(b'\n');
        pem.extend_from_slice(&key);
        let identity = reqwest::Identity::from_pem(&pem)
            .map_err(|e| TlsError::Rejected(format!("tls.client_cert / tls.client_key: {e}")))?;
        builder = builder.identity(identity);
    }

    if config.insecure_skip_verify {
        // Loud on purpose. This is the setting that makes TLS decorative, and
        // it should be visible in the log of every node that has it on.
        tracing::warn!(
            "tls.insecure_skip_verify is on: the controller's certificate is not \
             checked, and the cluster credential is exposed to anyone who can \
             redirect the connection"
        );
        builder = builder.danger_accept_invalid_certs(true);
    }

    Ok(builder)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_http_is_not_an_error() {
        assert!(server_config(&TlsConfig::default()).expect("config").is_none());
    }

    #[test]
    fn a_contradictory_configuration_fails_at_startup() {
        let config = TlsConfig {
            cert: Some(PathBuf::from("/tls/server.crt")),
            ..TlsConfig::default()
        };
        assert!(matches!(server_config(&config), Err(TlsError::Invalid(_))));
    }

    #[test]
    fn a_missing_file_names_the_file_and_the_setting() {
        let config = TlsConfig {
            cert: Some(PathBuf::from("/nonexistent/server.crt")),
            key: Some(PathBuf::from("/nonexistent/server.key")),
            ..TlsConfig::default()
        };
        let error = server_config(&config).expect_err("should fail").to_string();
        assert!(error.contains("tls.cert"), "{error}");
        assert!(error.contains("/nonexistent/server.crt"), "{error}");
    }

    #[test]
    fn a_file_that_is_not_a_certificate_says_so() {
        let dir = tempfile::tempdir().expect("tempdir");
        let cert = dir.path().join("server.crt");
        std::fs::write(&cert, b"this is not a certificate\n").expect("write");
        let config = TlsConfig {
            cert: Some(cert.clone()),
            key: Some(cert),
            ..TlsConfig::default()
        };
        let error = server_config(&config).expect_err("should fail").to_string();
        assert!(error.contains("no PEM certificates"), "{error}");
    }

    #[test]
    fn a_client_with_nothing_configured_is_left_alone() {
        let builder = apply_client_config(reqwest::Client::builder(), &TlsConfig::default());
        assert!(builder.is_ok());
    }

    #[test]
    fn a_client_ca_that_does_not_exist_names_the_setting() {
        let config = TlsConfig {
            ca: Some(PathBuf::from("/nonexistent/ca.crt")),
            ..TlsConfig::default()
        };
        let error = apply_client_config(reqwest::Client::builder(), &config)
            .expect_err("should fail")
            .to_string();
        assert!(error.contains("tls.ca"), "{error}");
    }

    #[test]
    fn the_provider_can_be_installed_more_than_once() {
        // Every entry point calls this, and several may run in one process.
        install_crypto_provider();
        install_crypto_provider();
    }
}
