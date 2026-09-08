//! The agent's health endpoint (SPEC.md §61).
//!
//! A tiny read-only HTTP surface that answers "is a Sentinel agent alive on
//! this host, and which one". It is what makes `SENTINEL_AGENT_FAILURE`
//! distinguishable from a host that has gone away.
//!
//! **It cannot be told to do anything.** There is one route, it is a GET, and
//! it takes no parameters. The RPC is deliberately incapable of running a
//! command (SPEC.md §116).

use std::sync::Arc;

use axum::extract::State;
use axum::routing::get;
use axum::{Json, Router};
use serde::{Deserialize, Serialize};

use crate::capability::CapabilitySet;
use crate::time::{now, Timestamp};
use crate::PROTOCOL_VERSION;

/// Default port the agent answers on.
pub const DEFAULT_PORT: u16 = 7444;
/// The one route.
pub const HEALTH_PATH: &str = "/v1/agent/health";

/// What the agent reports about itself.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AgentHealth {
    /// Protocol version.
    pub protocol_version: u32,
    /// Agent binary version.
    pub agent_version: String,
    /// Environment this agent belongs to.
    pub environment: String,
    /// The host it runs on.
    pub hostname: String,
    /// Boot id, so a peer can notice a reboot without the controller.
    pub boot_id: Option<String>,
    /// Capabilities in force.
    pub capabilities: CapabilitySet,
    /// The agent's clock, for skew detection between peers.
    pub timestamp: Timestamp,
    /// Seconds since this agent started.
    pub uptime_seconds: u64,
    /// Observations waiting to be delivered.
    ///
    /// Visible to peers on purpose: an agent whose spool is growing is an agent
    /// that cannot reach the controller, which a peer can see even when the
    /// controller cannot.
    pub spooled_observations: u64,
    /// Whether the agent currently believes it is registered.
    pub registered: bool,
}

/// Something that can report the agent's health.
///
/// A trait rather than a closure over the agent itself, and that is the point:
/// the endpoint must answer while the agent is busy. If the handler had to take
/// the agent's lock, a slow probe would stall the health endpoint at exactly
/// the moment a peer is trying to find out whether this host is alive.
#[async_trait::async_trait]
pub trait HealthSource: Send + Sync {
    /// Take a fresh snapshot.
    async fn health(&self) -> AgentHealth;
}

/// Everything the health handler needs.
#[derive(Clone)]
pub struct RpcState {
    /// Where health comes from.
    pub health: Arc<dyn HealthSource>,
}

impl RpcState {
    /// Build state from a health source.
    pub fn new(health: Arc<dyn HealthSource>) -> Self {
        Self { health }
    }
}

/// Build the router.
pub fn router(state: RpcState) -> Router {
    Router::new().route(HEALTH_PATH, get(health)).with_state(state)
}

async fn health(State(state): State<RpcState>) -> Json<AgentHealth> {
    Json(state.health.health().await)
}

/// A running agent RPC server.
pub struct RpcHandle {
    /// The address actually bound.
    pub local_addr: std::net::SocketAddr,
    shutdown: tokio::sync::oneshot::Sender<()>,
    server: tokio::task::JoinHandle<std::io::Result<()>>,
}

impl RpcHandle {
    /// Stop the server.
    pub async fn shutdown(self) {
        let _ = self.shutdown.send(());
        let _ = self.server.await;
    }
}

/// Start the agent's health endpoint.
pub async fn serve(listen: &str, state: RpcState) -> std::io::Result<RpcHandle> {
    let listener = tokio::net::TcpListener::bind(listen).await?;
    let local_addr = listener.local_addr()?;

    let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel();
    let server = tokio::spawn(async move {
        axum::serve(listener, router(state))
            .with_graceful_shutdown(async {
                let _ = shutdown_rx.await;
            })
            .await
    });

    Ok(RpcHandle {
        local_addr,
        shutdown: shutdown_tx,
        server,
    })
}

/// Reports agent health without touching the agent itself.
///
/// Everything here is either immutable or independently readable: the spool has
/// its own connection pool, and registration is an atomic flag the agent
/// updates. Nothing the handler does can be blocked by a probe.
pub struct AgentHealthHandle {
    /// Environment name.
    pub environment: String,
    /// Host name.
    pub hostname: String,
    /// Where the boot id comes from.
    pub inspector: Arc<dyn crate::agent::SystemInspector>,
    /// Capabilities in force.
    pub capabilities: CapabilitySet,
    /// The spool, for its depth.
    pub spool: crate::agent::Spool,
    /// Whether the agent believes it is registered.
    pub registered: Arc<std::sync::atomic::AtomicBool>,
    /// When the agent started.
    pub started_at: std::time::Instant,
}

#[async_trait::async_trait]
impl HealthSource for AgentHealthHandle {
    async fn health(&self) -> AgentHealth {
        health_snapshot(
            &self.environment,
            &self.hostname,
            self.inspector.boot_id(),
            self.capabilities.clone(),
            self.started_at,
            self.spool.len().await.unwrap_or(0),
            self.registered.load(std::sync::atomic::Ordering::Relaxed),
        )
    }
}

/// A fixed health source, for tests.
pub struct StaticHealth(pub AgentHealth);

#[async_trait::async_trait]
impl HealthSource for StaticHealth {
    async fn health(&self) -> AgentHealth {
        AgentHealth {
            timestamp: now(),
            ..self.0.clone()
        }
    }
}

/// Build a health snapshot.
pub fn health_snapshot(
    environment: &str,
    hostname: &str,
    boot_id: Option<String>,
    capabilities: CapabilitySet,
    started_at: std::time::Instant,
    spooled_observations: u64,
    registered: bool,
) -> AgentHealth {
    AgentHealth {
        protocol_version: PROTOCOL_VERSION,
        agent_version: crate::VERSION.to_string(),
        environment: environment.to_string(),
        hostname: hostname.to_string(),
        boot_id,
        capabilities,
        timestamp: now(),
        uptime_seconds: started_at.elapsed().as_secs(),
        spooled_observations,
        registered,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state() -> RpcState {
        RpcState::new(Arc::new(StaticHealth(health_snapshot(
            "lab",
            "node-a",
            Some("boot-1".into()),
            CapabilitySet::from_iter(["host.metrics", "ssh.server"]),
            std::time::Instant::now(),
            3,
            true,
        ))))
    }

    #[tokio::test]
    async fn the_health_endpoint_reports_the_agents_identity() {
        let handle = serve("127.0.0.1:0", state()).await.expect("serve");
        let url = format!("http://{}{HEALTH_PATH}", handle.local_addr);

        let health: AgentHealth = reqwest::get(&url).await.expect("get").json().await.expect("json");

        assert_eq!(health.hostname, "node-a");
        assert_eq!(health.environment, "lab");
        assert_eq!(health.boot_id.as_deref(), Some("boot-1"));
        assert!(health.capabilities.has("ssh.server"));
        assert_eq!(health.spooled_observations, 3);
        assert!(health.registered);

        handle.shutdown().await;
    }

    #[tokio::test]
    async fn there_is_no_route_that_does_anything_but_report() {
        // SPEC.md §116, checked rather than trusted.
        let handle = serve("127.0.0.1:0", state()).await.expect("serve");
        let base = format!("http://{}", handle.local_addr);
        let client = reqwest::Client::new();

        for path in ["/v1/agent/exec", "/v1/agent/run", "/v1/exec", "/v1/agent/probe"] {
            let response = client.post(format!("{base}{path}")).send().await.expect("request");
            assert_eq!(
                response.status(),
                reqwest::StatusCode::NOT_FOUND,
                "{path} must not exist"
            );
        }

        // Even the one route that exists refuses anything but a GET.
        let response = client
            .post(format!("{base}{HEALTH_PATH}"))
            .send()
            .await
            .expect("request");
        assert!(!response.status().is_success(), "the health route must be read-only");

        handle.shutdown().await;
    }

    #[tokio::test]
    async fn shutdown_stops_the_listener() {
        let handle = serve("127.0.0.1:0", state()).await.expect("serve");
        let url = format!("http://{}{HEALTH_PATH}", handle.local_addr);
        assert!(reqwest::get(&url).await.is_ok());

        handle.shutdown().await;

        for _ in 0..50 {
            if reqwest::get(&url).await.is_err() {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        panic!("the agent RPC was still answering after shutdown");
    }

    #[tokio::test]
    async fn the_snapshot_is_taken_fresh_on_each_request() {
        struct Counting(std::sync::atomic::AtomicU64);

        #[async_trait::async_trait]
        impl HealthSource for Counting {
            async fn health(&self) -> AgentHealth {
                let spooled = self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                health_snapshot(
                    "lab",
                    "node-a",
                    None,
                    CapabilitySet::new(),
                    std::time::Instant::now(),
                    spooled,
                    true,
                )
            }
        }

        let handle = serve(
            "127.0.0.1:0",
            RpcState::new(Arc::new(Counting(std::sync::atomic::AtomicU64::new(0)))),
        )
        .await
        .expect("serve");

        let url = format!("http://{}{HEALTH_PATH}", handle.local_addr);
        let first: AgentHealth = reqwest::get(&url).await.expect("get").json().await.expect("json");
        let second: AgentHealth = reqwest::get(&url).await.expect("get").json().await.expect("json");

        assert_eq!(first.spooled_observations, 0);
        assert_eq!(
            second.spooled_observations, 1,
            "a stale snapshot would hide a growing spool"
        );

        handle.shutdown().await;
    }

    #[test]
    fn health_json_round_trips() {
        let health = health_snapshot(
            "lab",
            "node-a",
            Some("boot-1".into()),
            CapabilitySet::from_iter(["host.metrics"]),
            std::time::Instant::now(),
            0,
            false,
        );
        let text = serde_json::to_string(&health).expect("serialize");
        assert_eq!(serde_json::from_str::<AgentHealth>(&text).expect("deserialize"), health);
    }
}
