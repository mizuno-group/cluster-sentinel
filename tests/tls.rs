//! Transport security, end to end against a real TLS listener.
//!
//! The credential an agent presents is a bearer token: whoever reads it off
//! the wire can impersonate every agent in the cluster. Until this existed the
//! only answer was "put a reverse proxy in front of it", which is a task list
//! handed to the operator rather than a property of the system.
//!
//! Every certificate here is generated in-process. There are no fixtures on
//! disk, so nothing can expire and quietly turn these into tests of an error
//! message.

use std::path::{Path, PathBuf};
use std::time::Duration;

use sentinel::agent::ControllerClient;
use sentinel::config::{Config, TlsConfig};
use sentinel::controller::{serve, Controller, ServeOptions, ServerHandle};
use sentinel::persistence::SqliteStore;
use sentinel::protocol::{tls as tls_setup, ClusterCredential, RegisterRequest};

const TOKEN: &str = "0123456789abcdef0123456789abcdef";

fn credential() -> ClusterCredential {
    ClusterCredential::new(TOKEN)
}

fn config() -> Config {
    Config {
        config_version: 1,
        environment: "lab".into(),
        ..Config::default()
    }
}

/// A private CA and the certificates it has issued, on disk in a temp dir.
struct Pki {
    dir: tempfile::TempDir,
}

impl Pki {
    /// Issue a CA, a server certificate for localhost, and a client certificate.
    fn new() -> Self {
        let dir = tempfile::tempdir().expect("tempdir");
        let pki = Self { dir };

        let mut ca_params = rcgen::CertificateParams::new(Vec::new()).expect("ca params");
        ca_params.is_ca = rcgen::IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
        ca_params
            .distinguished_name
            .push(rcgen::DnType::CommonName, "sentinel-test-ca");
        let ca_key = rcgen::KeyPair::generate().expect("ca key");
        let ca = ca_params.self_signed(&ca_key).expect("ca");
        pki.write("ca.crt", &ca.pem());

        let issuer = rcgen::Issuer::from_params(&ca_params, &ca_key);

        // The server certificate names both spellings of loopback, so a test
        // can connect by name or by address.
        let server_params =
            rcgen::CertificateParams::new(vec!["localhost".into(), "127.0.0.1".into()]).expect("server params");
        let server_key = rcgen::KeyPair::generate().expect("server key");
        let server = server_params.signed_by(&server_key, &issuer).expect("server cert");
        pki.write("server.crt", &server.pem());
        pki.write("server.key", &server_key.serialize_pem());

        let client_params = rcgen::CertificateParams::new(vec!["agent".into()]).expect("client params");
        let client_key = rcgen::KeyPair::generate().expect("client key");
        let client = client_params.signed_by(&client_key, &issuer).expect("client cert");
        pki.write("client.crt", &client.pem());
        pki.write("client.key", &client_key.serialize_pem());

        // A second, unrelated CA, for the certificate that must be refused.
        let mut rogue_params = rcgen::CertificateParams::new(Vec::new()).expect("rogue params");
        rogue_params.is_ca = rcgen::IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
        rogue_params
            .distinguished_name
            .push(rcgen::DnType::CommonName, "not-your-ca");
        let rogue_ca_key = rcgen::KeyPair::generate().expect("rogue ca key");
        let rogue_ca = rogue_params.self_signed(&rogue_ca_key).expect("rogue ca");
        pki.write("rogue-ca.crt", &rogue_ca.pem());

        let rogue_issuer = rcgen::Issuer::from_params(&rogue_params, &rogue_ca_key);
        let rogue_client_params = rcgen::CertificateParams::new(vec!["agent".into()]).expect("params");
        let rogue_client_key = rcgen::KeyPair::generate().expect("key");
        let rogue_client = rogue_client_params
            .signed_by(&rogue_client_key, &rogue_issuer)
            .expect("rogue client cert");
        pki.write("rogue-client.crt", &rogue_client.pem());
        pki.write("rogue-client.key", &rogue_client_key.serialize_pem());

        pki
    }

    fn write(&self, name: &str, contents: &str) {
        std::fs::write(self.path(name), contents).expect("write pem");
    }

    fn path(&self, name: &str) -> PathBuf {
        self.dir.path().join(name)
    }

    fn at(&self, name: &str) -> Option<PathBuf> {
        Some(self.path(name))
    }
}

/// Start a controller with the given TLS settings.
async fn start(tls: &TlsConfig) -> ServerHandle {
    let store = SqliteStore::open_in_memory().await.expect("store");
    let controller = Controller::new(config(), store).await.expect("controller");
    let server_config = tls_setup::server_config(tls).expect("server tls");
    serve(
        controller,
        ServeOptions {
            listen: "127.0.0.1:0".into(),
            credential: credential(),
            heartbeat_interval: Duration::from_secs(5),
            discovery_interval: None,
            diagnosis_interval: None,
            retention: None,
            tls: server_config,
        },
    )
    .await
    .expect("serve")
}

fn registration() -> RegisterRequest {
    RegisterRequest::new("lab", "n1", Default::default())
}

/// Try to register, returning whether the call got through.
async fn can_register(handle: &ServerHandle, tls: &TlsConfig) -> Result<(), String> {
    let address = format!("127.0.0.1:{}", handle.local_addr.port());
    let client =
        ControllerClient::with_tls(&address, &credential(), Duration::from_secs(5), tls).map_err(|e| e.to_string())?;
    client
        .register(&registration())
        .await
        .map(|_| ())
        .map_err(|e| e.to_string())
}

fn trusting(pki: &Pki) -> TlsConfig {
    TlsConfig {
        ca: pki.at("ca.crt"),
        ..TlsConfig::default()
    }
}

fn serving(pki: &Pki) -> TlsConfig {
    TlsConfig {
        cert: pki.at("server.crt"),
        key: pki.at("server.key"),
        ..TlsConfig::default()
    }
}

#[tokio::test]
async fn an_agent_that_trusts_the_ca_can_register_over_tls() {
    let pki = Pki::new();
    let handle = start(&serving(&pki)).await;
    assert!(handle.tls, "the listener should be serving TLS");

    can_register(&handle, &trusting(&pki)).await.expect("registration");
    handle.shutdown().await;
}

#[tokio::test]
async fn an_agent_that_does_not_trust_the_certificate_is_refused() {
    // The point of TLS here is that a redirected connection fails rather than
    // handing over the cluster credential.
    let pki = Pki::new();
    let handle = start(&serving(&pki)).await;

    let untrusting = TlsConfig {
        ca: pki.at("rogue-ca.crt"),
        ..TlsConfig::default()
    };
    let error = can_register(&handle, &untrusting).await.expect_err("should fail");
    assert!(
        !error.contains(TOKEN),
        "an error message must never carry the credential: {error}"
    );

    handle.shutdown().await;
}

#[tokio::test]
async fn verification_can_be_skipped_deliberately() {
    // People bring up a cluster before they bring up a PKI. A documented
    // switch is safer than the alternative, which is going back to plain HTTP
    // and forgetting.
    let pki = Pki::new();
    let handle = start(&serving(&pki)).await;

    let insecure = TlsConfig {
        insecure_skip_verify: true,
        ..TlsConfig::default()
    };
    can_register(&handle, &insecure).await.expect("registration");
    handle.shutdown().await;
}

#[tokio::test]
async fn mutual_tls_refuses_an_agent_with_no_certificate() {
    // This is the shape that survives a leaked token: the token alone is not
    // enough to talk to the controller at all.
    let pki = Pki::new();
    let mut serving = serving(&pki);
    serving.client_ca = pki.at("ca.crt");
    let handle = start(&serving).await;

    let error = can_register(&handle, &trusting(&pki))
        .await
        .expect_err("a client with no certificate must be refused");
    assert!(!error.contains(TOKEN), "{error}");

    handle.shutdown().await;
}

#[tokio::test]
async fn mutual_tls_accepts_an_agent_with_a_certificate_from_the_right_ca() {
    let pki = Pki::new();
    let mut serving = serving(&pki);
    serving.client_ca = pki.at("ca.crt");
    let handle = start(&serving).await;

    let mut client_tls = trusting(&pki);
    client_tls.client_cert = pki.at("client.crt");
    client_tls.client_key = pki.at("client.key");

    can_register(&handle, &client_tls).await.expect("registration");
    handle.shutdown().await;
}

#[tokio::test]
async fn mutual_tls_refuses_a_certificate_from_another_ca() {
    let pki = Pki::new();
    let mut serving = serving(&pki);
    serving.client_ca = pki.at("ca.crt");
    let handle = start(&serving).await;

    let mut rogue = trusting(&pki);
    rogue.client_cert = pki.at("rogue-client.crt");
    rogue.client_key = pki.at("rogue-client.key");

    assert!(
        can_register(&handle, &rogue).await.is_err(),
        "a certificate from an unrelated CA was accepted"
    );
    handle.shutdown().await;
}

#[tokio::test]
async fn tls_does_not_replace_the_credential() {
    // Two independent gates. A valid certificate is not an authorisation, and
    // a client that gets through the handshake with the wrong token is still
    // refused.
    let pki = Pki::new();
    let handle = start(&serving(&pki)).await;

    let address = format!("127.0.0.1:{}", handle.local_addr.port());
    let wrong = ClusterCredential::new("ffffffffffffffffffffffffffffffff");
    let client = ControllerClient::with_tls(&address, &wrong, Duration::from_secs(5), &trusting(&pki)).expect("client");

    let error = client.register(&registration()).await.expect_err("should be refused");
    assert!(
        error.to_string().to_lowercase().contains("credential"),
        "expected a credential refusal, got: {error}"
    );

    handle.shutdown().await;
}

#[tokio::test]
async fn a_server_name_lets_an_agent_connect_by_address() {
    // The controller is reached at 127.0.0.1 but its certificate names
    // localhost. Without this the only options would be a certificate per
    // address or turning verification off.
    let pki = Pki::new();
    let hostname_only = rcgen::CertificateParams::new(vec!["controller.example".into()]).expect("params");
    let key = rcgen::KeyPair::generate().expect("key");

    let mut ca_params = rcgen::CertificateParams::new(Vec::new()).expect("ca params");
    ca_params.is_ca = rcgen::IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
    let ca_key = rcgen::KeyPair::generate().expect("ca key");
    let ca = ca_params.self_signed(&ca_key).expect("ca");
    let issuer = rcgen::Issuer::from_params(&ca_params, &ca_key);
    let cert = hostname_only.signed_by(&key, &issuer).expect("cert");

    pki.write("named.crt", &cert.pem());
    pki.write("named.key", &key.serialize_pem());
    pki.write("named-ca.crt", &ca.pem());

    let handle = start(&TlsConfig {
        cert: pki.at("named.crt"),
        key: pki.at("named.key"),
        ..TlsConfig::default()
    })
    .await;

    let named = TlsConfig {
        ca: pki.at("named-ca.crt"),
        server_name: Some("controller.example".into()),
        ..TlsConfig::default()
    };
    can_register(&handle, &named).await.expect("registration");

    // And without it, the same connection fails: the name really is what is
    // being checked.
    let unnamed = TlsConfig {
        ca: pki.at("named-ca.crt"),
        ..TlsConfig::default()
    };
    assert!(can_register(&handle, &unnamed).await.is_err());

    handle.shutdown().await;
}

#[tokio::test]
async fn a_plain_http_controller_still_works() {
    // TLS is additive. Nothing about the existing deployment shape changes.
    let handle = start(&TlsConfig::default()).await;
    assert!(!handle.tls);
    can_register(&handle, &TlsConfig::default())
        .await
        .expect("registration");
    handle.shutdown().await;
}

#[tokio::test]
async fn a_controller_with_broken_tls_material_refuses_to_start() {
    // The failure mode this prevents is the worst one available: coming up in
    // plaintext while the operator believes the connection is encrypted.
    let missing = TlsConfig {
        cert: Some(Path::new("/nonexistent/server.crt").to_path_buf()),
        key: Some(Path::new("/nonexistent/server.key").to_path_buf()),
        ..TlsConfig::default()
    };
    assert!(tls_setup::server_config(&missing).is_err());
}
