//! The agent's HTTP client for talking to a controller.
//!
//! Every call has a timeout, and every failure is a value rather than a panic:
//! an unreachable controller is a normal, expected condition that the agent
//! must ride out with its spool, not an error that stops it.

use std::time::Duration;

use thiserror::Error;

use crate::config::TlsConfig;
use crate::protocol::{
    AssignmentsResponse, ClusterCredential, HeartbeatRequest, HeartbeatResponse, ObservationBatch,
    ObservationBatchResponse, RegisterRequest, RegisterResponse, API_PREFIX,
};

/// Why a call to the controller did not succeed.
#[derive(Debug, Error)]
pub enum ClientError {
    /// The controller could not be reached.
    #[error("cannot reach controller at {endpoint}: {detail}")]
    Unreachable {
        /// The endpoint that was tried.
        endpoint: String,
        /// What went wrong.
        detail: String,
    },
    /// The controller refused the credential.
    #[error("controller rejected our credential")]
    Unauthorized,
    /// The controller speaks a different protocol version.
    #[error("controller refused our protocol version: {detail}")]
    ProtocolMismatch {
        /// The controller's explanation.
        detail: String,
    },
    /// The controller returned an error.
    #[error("controller returned {status}: {detail}")]
    Rejected {
        /// HTTP status.
        status: u16,
        /// Body, or an extract of it.
        detail: String,
    },
    /// The response could not be decoded.
    #[error("cannot decode controller response: {0}")]
    Decode(String),
    /// The client could not be built from the configuration given.
    #[error("client configuration: {0}")]
    Configuration(String),
}

/// Split `scheme://host:port` into its host and port.
fn split_host_port(url: &str) -> Result<(String, u16), ClientError> {
    let rest = url.split_once("://").map(|(_, rest)| rest).unwrap_or(url);
    let rest = rest.split('/').next().unwrap_or(rest);
    let (host, port) = rest
        .rsplit_once(':')
        .ok_or_else(|| ClientError::Configuration(format!("{url:?} has no port; tls.server_name needs one")))?;
    let port = port
        .parse()
        .map_err(|_| ClientError::Configuration(format!("{port:?} is not a port number")))?;
    Ok((host.to_string(), port))
}

impl ClientError {
    /// Whether retrying later might work.
    ///
    /// A rejected credential or protocol version will not fix itself by being
    /// retried, and hammering the controller with it helps nobody.
    pub fn is_transient(&self) -> bool {
        match self {
            ClientError::Unreachable { .. } => true,
            ClientError::Rejected { status, .. } => *status >= 500 || *status == 429,
            ClientError::Unauthorized
            | ClientError::ProtocolMismatch { .. }
            | ClientError::Decode(_)
            // A misconfigured client will be misconfigured on the next attempt
            // too; retrying only hides the message that would fix it.
            | ClientError::Configuration(_) => false,
        }
    }
}

/// A client for one controller.
#[derive(Debug, Clone)]
pub struct ControllerClient {
    base_url: String,
    credential: String,
    http: reqwest::Client,
}

impl ControllerClient {
    /// Build a client.
    ///
    /// `address` may be `host:port` or a full URL. A bare `host:port` is taken
    /// as `http://`; TLS is a deployment decision expressed in the URL, and
    /// `docs/SECURITY.md` records what that implies.
    pub fn new(address: &str, credential: &ClusterCredential, timeout: Duration) -> Result<Self, ClientError> {
        Self::with_tls(address, credential, timeout, &TlsConfig::default())
    }

    /// Build a client with transport security.
    ///
    /// A bare `host:port` is `http://` unless the TLS settings say otherwise,
    /// in which case it is `https://`: an operator who has configured a CA or
    /// a client certificate has said what they want, and silently connecting
    /// in plaintext anyway would be the wrong reading of it.
    pub fn with_tls(
        address: &str,
        credential: &ClusterCredential,
        timeout: Duration,
        tls: &TlsConfig,
    ) -> Result<Self, ClientError> {
        let scheme = if tls.affects_client() { "https" } else { "http" };
        let base_url = if address.starts_with("http://") || address.starts_with("https://") {
            address.trim_end_matches('/').to_string()
        } else {
            format!("{scheme}://{}", address.trim_end_matches('/'))
        };

        let mut builder = reqwest::Client::builder().timeout(timeout);
        builder = crate::protocol::tls::apply_client_config(builder, tls)
            .map_err(|e| ClientError::Configuration(e.to_string()))?;

        // `server_name` exists for the case where the controller is reached by
        // address but its certificate names a host. The request is addressed to
        // the name, and the name is resolved back to the address it came from.
        let (base_url, builder) = match &tls.server_name {
            Some(name) => {
                let (host, port) = split_host_port(&base_url)?;
                let ip: std::net::IpAddr = host.parse().map_err(|_| {
                    ClientError::Configuration(format!(
                        "tls.server_name is for reaching the controller by address,                          but {host:?} is already a name; remove one of the two"
                    ))
                })?;
                let url = base_url.replacen(&host, name, 1);
                (url, builder.resolve(name, std::net::SocketAddr::new(ip, port)))
            }
            None => (base_url, builder),
        };

        let http = builder.build().map_err(|e| ClientError::Configuration(e.to_string()))?;

        Ok(Self {
            base_url,
            credential: credential.header_value(),
            http,
        })
    }

    /// The controller's base URL.
    pub fn base_url(&self) -> &str {
        &self.base_url
    }

    /// Register with the controller.
    pub async fn register(&self, request: &RegisterRequest) -> Result<RegisterResponse, ClientError> {
        self.post("/agents/register", request).await
    }

    /// Send a heartbeat.
    pub async fn heartbeat(&self, request: &HeartbeatRequest) -> Result<HeartbeatResponse, ClientError> {
        self.post("/agents/heartbeat", request).await
    }

    /// Send a batch of observations.
    pub async fn send_observations(&self, batch: &ObservationBatch) -> Result<ObservationBatchResponse, ClientError> {
        self.post("/observations/batch", batch).await
    }

    /// Fetch what this agent has been asked to observe.
    pub async fn assignments(&self, agent_id: uuid::Uuid) -> Result<AssignmentsResponse, ClientError> {
        self.get(&format!("/agents/{agent_id}/assignments")).await
    }

    async fn get<Res: serde::de::DeserializeOwned>(&self, path: &str) -> Result<Res, ClientError> {
        let url = format!("{}{API_PREFIX}{path}", self.base_url);

        let response = self
            .http
            .get(&url)
            .header(crate::protocol::AUTH_HEADER, &self.credential)
            .send()
            .await
            .map_err(|e| ClientError::Unreachable {
                endpoint: url.clone(),
                detail: e.to_string(),
            })?;

        Self::decode(response).await
    }

    async fn post<Req: serde::Serialize, Res: serde::de::DeserializeOwned>(
        &self,
        path: &str,
        body: &Req,
    ) -> Result<Res, ClientError> {
        let url = format!("{}{API_PREFIX}{path}", self.base_url);

        let response = self
            .http
            .post(&url)
            .header(crate::protocol::AUTH_HEADER, &self.credential)
            .json(body)
            .send()
            .await
            .map_err(|e| ClientError::Unreachable {
                endpoint: url.clone(),
                detail: e.to_string(),
            })?;

        Self::decode(response).await
    }

    async fn decode<Res: serde::de::DeserializeOwned>(response: reqwest::Response) -> Result<Res, ClientError> {
        let status = response.status();
        if status.is_success() {
            return response.json().await.map_err(|e| ClientError::Decode(e.to_string()));
        }

        let detail = response.text().await.unwrap_or_default();
        Err(match status.as_u16() {
            401 | 403 => ClientError::Unauthorized,
            400 if detail.contains("protocol_version_mismatch") => ClientError::ProtocolMismatch { detail },
            other => ClientError::Rejected { status: other, detail },
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::capability::CapabilitySet;

    fn credential() -> ClusterCredential {
        ClusterCredential::new("0123456789abcdef0123456789abcdef")
    }

    fn client(address: &str) -> ControllerClient {
        ControllerClient::new(address, &credential(), Duration::from_millis(200)).expect("client")
    }

    #[test]
    fn a_bare_host_and_port_becomes_an_http_url() {
        assert_eq!(
            client("controller.example:7443").base_url(),
            "http://controller.example:7443"
        );
    }

    #[test]
    fn an_explicit_scheme_is_preserved() {
        assert_eq!(
            client("https://controller.example:7443").base_url(),
            "https://controller.example:7443"
        );
        assert_eq!(
            client("http://controller.example:7443/").base_url(),
            "http://controller.example:7443"
        );
    }

    #[tokio::test]
    async fn an_unreachable_controller_is_an_error_not_a_panic() {
        // Port 1 on loopback: nothing is listening, and nothing should be.
        let error = client("127.0.0.1:1")
            .register(&RegisterRequest::new("lab", "node-a", CapabilitySet::new()))
            .await
            .expect_err("must fail");

        assert!(matches!(error, ClientError::Unreachable { .. }), "{error:?}");
        assert!(error.is_transient(), "the agent must keep trying and keep spooling");
    }

    #[test]
    fn permanent_failures_are_distinguished_from_transient_ones() {
        // Retrying a rejected credential forever helps nobody.
        assert!(!ClientError::Unauthorized.is_transient());
        assert!(!ClientError::ProtocolMismatch { detail: String::new() }.is_transient());
        assert!(!ClientError::Decode("bad json".into()).is_transient());

        assert!(ClientError::Unreachable {
            endpoint: String::new(),
            detail: String::new()
        }
        .is_transient());
        assert!(ClientError::Rejected {
            status: 503,
            detail: String::new()
        }
        .is_transient());
        assert!(ClientError::Rejected {
            status: 429,
            detail: String::new()
        }
        .is_transient());
        assert!(!ClientError::Rejected {
            status: 400,
            detail: String::new()
        }
        .is_transient());
    }
}
