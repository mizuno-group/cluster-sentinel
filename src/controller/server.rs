//! Running the controller as a daemon.
//!
//! Three things run concurrently, and any of them can fail without stopping
//! the others — a Slurm outage must not take the API down, and a failed
//! diagnosis pass must not stop observations arriving:
//!
//! | Loop | Cadence | Cost |
//! | --- | --- | --- |
//! | HTTP API | continuous | agents push observations here |
//! | Inventory discovery | minutes | shells out to `scontrol`, probes every host |
//! | Diagnosis and notification | seconds | reads only what is already stored |
//! | Retention pruning | hours | one delete per data class |
//!
//! The API listens with TLS when `[tls]` names a certificate and key, and in
//! plaintext otherwise. Both are bound the same way, so port 0 and host names
//! behave identically either way.
//!
//! Discovery and diagnosis are separate on purpose. Sharing discovery's
//! cadence would mean a fault waits for the next inventory sweep before anyone
//! is told, and inventory sweeps are expensive enough to want to be rare.

use std::net::SocketAddr;
use std::sync::Arc;

use tokio::net::TcpListener;
use tokio::sync::Mutex;

use crate::config::RetentionConfig;

/// How long in-flight requests get to finish when the controller is stopping.
const GRACE_PERIOD: std::time::Duration = std::time::Duration::from_secs(5);

use crate::notification::{Deduplicator, MaintenanceWindows, NotificationProvider};
use crate::protocol::ClusterCredential;

use super::agents::AgentRegistry;
use super::api::{router, ApiState};
use super::Controller;

/// A running controller.
pub struct ServerHandle {
    /// The address actually bound, which may differ from the requested one when
    /// port 0 was asked for.
    pub local_addr: SocketAddr,
    /// Whether the listener is serving TLS.
    pub tls: bool,
    shutdown: tokio::sync::oneshot::Sender<()>,
    server: tokio::task::JoinHandle<std::io::Result<()>>,
    discovery: Option<tokio::task::JoinHandle<()>>,
    diagnosis: Option<tokio::task::JoinHandle<()>>,
    retention: Option<tokio::task::JoinHandle<()>>,
}

impl ServerHandle {
    /// Ask the server to stop and wait for it (SPEC.md §84).
    pub async fn shutdown(self) {
        let _ = self.shutdown.send(());
        for task in [self.discovery, self.diagnosis, self.retention].into_iter().flatten() {
            task.abort();
            let _ = task.await;
        }
        let _ = self.server.await;
    }
}

/// Options for running a controller.
pub struct ServeOptions {
    /// Address to listen on.
    pub listen: String,
    /// The cluster credential agents must present.
    pub credential: ClusterCredential,
    /// How often agents should heartbeat.
    pub heartbeat_interval: std::time::Duration,
    /// How often to run inventory discovery; `None` disables the loop.
    pub discovery_interval: Option<std::time::Duration>,
    /// How often to diagnose, correlate and notify; `None` disables the loop.
    pub diagnosis_interval: Option<std::time::Duration>,
    /// How long to keep recorded data; `None` disables pruning entirely.
    pub retention: Option<RetentionConfig>,
    /// Transport security. Plain HTTP when this is `None`.
    pub tls: Option<Arc<rustls::ServerConfig>>,
}

/// Start the controller's API, and its discovery loop if one is configured.
pub async fn serve(controller: Controller, options: ServeOptions) -> anyhow::Result<ServerHandle> {
    let discovery_interval = options.discovery_interval;
    let diagnosis_interval = options.diagnosis_interval;
    let retention = options.retention.filter(|r| r.enabled);

    let providers: Vec<Arc<dyn NotificationProvider>> = super::providers_from_config(controller.config());
    if !providers.is_empty() {
        tracing::info!(destinations = providers.len(), "notifications enabled");
    }

    let controller = Arc::new(Mutex::new(controller));

    let state = ApiState {
        controller: Arc::clone(&controller),
        agents: Arc::new(Mutex::new(AgentRegistry::new())),
        credential: Arc::new(options.credential),
        heartbeat_interval: options.heartbeat_interval,
    };

    // Bound the same way in both cases, so that port 0 and host names behave
    // identically with and without TLS.
    let listener = TcpListener::bind(&options.listen).await?;
    let local_addr = listener.local_addr()?;
    let serving_tls = options.tls.is_some();
    tracing::info!(%local_addr, tls = serving_tls, "controller listening");
    if !serving_tls {
        tracing::warn!(
            "the controller is serving plain HTTP: the cluster credential crosses \
             the network in the clear. Set [tls] cert and key, or confine this to a \
             trusted management network."
        );
    }

    let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel();
    let router = router(state);
    let server = match options.tls {
        Some(tls) => {
            let std_listener = listener.into_std()?;
            let handle = axum_server::Handle::new();
            let shutdown_handle = handle.clone();
            tokio::spawn(async move {
                let _ = shutdown_rx.await;
                // Let in-flight requests finish; an agent mid-upload should not
                // have to re-send a batch because the controller was restarted.
                shutdown_handle.graceful_shutdown(Some(GRACE_PERIOD));
            });
            let acceptor = axum_server::tls_rustls::RustlsConfig::from_config(tls);
            let server = axum_server::from_tcp_rustls(std_listener, acceptor)
                .map_err(|e| anyhow::anyhow!("cannot serve TLS on {local_addr}: {e}"))?;
            tokio::spawn(async move { server.handle(handle).serve(router.into_make_service()).await })
        }
        None => tokio::spawn(async move {
            axum::serve(listener, router)
                .with_graceful_shutdown(async {
                    let _ = shutdown_rx.await;
                })
                .await
        }),
    };

    let discovery = discovery_interval.map(|interval| {
        let controller = Arc::clone(&controller);
        tokio::spawn(async move { discovery_loop(controller, interval).await })
    });

    // Diagnosis on its own, much shorter, cadence. Sharing discovery's would
    // mean a fault waits for the next inventory sweep before anyone is told.
    let diagnosis = diagnosis_interval.map(|interval| {
        let controller = Arc::clone(&controller);
        let providers = providers.clone();
        tokio::spawn(async move { diagnosis_loop(controller, interval, providers).await })
    });

    // Pruning is last to start and first to be skippable: it is the only loop
    // whose job is to delete, so it never runs unless it was asked for.
    let retention = retention.map(|config| {
        let controller = Arc::clone(&controller);
        tokio::spawn(async move { retention_loop(controller, config).await })
    });

    Ok(ServerHandle {
        local_addr,
        tls: serving_tls,
        shutdown: shutdown_tx,
        server,
        discovery,
        diagnosis,
        retention,
    })
}

/// Delete records that have outlived their retention period.
///
/// A pass runs at startup as well as on the interval, because a controller
/// that was down for a week comes back with a week of arrears, and waiting an
/// hour to start on it wastes the one hour when the disk is emptiest.
async fn retention_loop(controller: Arc<Mutex<Controller>>, config: RetentionConfig) {
    let mut ticker = tokio::time::interval(config.interval);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    loop {
        ticker.tick().await;

        let started = std::time::Instant::now();
        let store = {
            let controller = controller.lock().await;
            controller.store().clone()
        };

        // Deliberately outside the controller lock: a long first pass must not
        // stop observations being ingested. SQLite serialises the writes.
        match store.prune(&config).await {
            Ok(outcome) if outcome.is_empty() => {
                tracing::debug!(elapsed_ms = started.elapsed().as_millis() as u64, "nothing to prune");
            }
            Ok(outcome) => tracing::info!(
                observations = outcome.observations,
                transitions = outcome.transitions,
                incidents = outcome.incidents,
                diagnoses = outcome.diagnoses,
                elapsed_ms = started.elapsed().as_millis() as u64,
                "pruned records past their retention period"
            ),
            // Never fatal: a failed prune costs disk, a stopped controller
            // costs the monitoring.
            Err(error) => tracing::error!(%error, "retention pass failed"),
        }
    }
}

/// The maintenance windows in force, as the diagnosis loop sees them.
///
/// Read every pass rather than once at startup, so a window declared while the
/// controller is running takes effect on the next tick. An operator who has
/// just started pulling disks out of a machine should not have to restart the
/// monitor to stop being paged about it.
///
/// A function rather than three lines inline because this is the step that was
/// missing: the loop used to construct an empty set here, which made every
/// window anyone might declare unreachable. Naming it gives the wiring
/// somewhere to be tested.
async fn active_maintenance(controller: &Controller) -> MaintenanceWindows {
    match controller
        .store()
        .load_maintenance_windows(&controller.config().environment)
        .await
    {
        Ok(windows) => MaintenanceWindows::from_windows(windows),
        // Failing open: not reading the windows means notifying during planned
        // work, which is noise. Failing closed would mean silence during a real
        // outage, which is the one outcome a monitor must never produce.
        Err(error) => {
            tracing::warn!(%error, "cannot read maintenance windows; notifying as usual");
            MaintenanceWindows::new()
        }
    }
}

/// Diagnose, correlate and notify on a schedule.
///
/// Reads only what is already stored, so it is cheap enough to run often. This
/// interval is what decides how long a fault goes unreported.
async fn diagnosis_loop(
    controller: Arc<Mutex<Controller>>,
    interval: std::time::Duration,
    providers: Vec<Arc<dyn NotificationProvider>>,
) {
    // Notification state lives with the loop, but not only in it: the
    // deduplicator is seeded from what was actually delivered, so a restart
    // neither re-announces incidents the operator already heard about nor
    // permanently silences ones that were never successfully announced.
    let mut deduplicator = Deduplicator::new();
    {
        let controller = controller.lock().await;
        let environment = controller.config().environment.clone();
        match controller.store().load_notifications(&environment).await {
            Ok(records) => {
                let count = records.len();
                deduplicator.seed(records);
                tracing::debug!(records = count, "resumed notification history");
            }
            // Losing the history means saying something twice, which is far
            // better than the alternative, so this must not stop the loop.
            Err(error) => tracing::warn!(%error, "cannot resume notification history"),
        }
    }
    let mut ticker = tokio::time::interval(interval);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    loop {
        ticker.tick().await;

        let mut controller = controller.lock().await;
        let update = match controller.diagnose_and_correlate().await {
            Ok((diagnoses, update)) => {
                if !update.is_empty() {
                    tracing::debug!(
                        diagnoses = diagnoses.len(),
                        opened = update.opened.len(),
                        resolved = update.resolved.len(),
                        "incidents reconciled"
                    );
                }
                update
            }
            // A failed pass must never end the loop: the next one may succeed,
            // and a monitor that gives up during an outage is useless.
            Err(error) => {
                tracing::error!(%error, "diagnosis pass failed");
                continue;
            }
        };

        if providers.is_empty() {
            continue;
        }

        let maintenance = active_maintenance(&controller).await;
        match controller
            .notify(&update, &providers, &mut deduplicator, &maintenance)
            .await
        {
            Ok(outcome) if outcome.sent > 0 => {
                tracing::info!(sent = outcome.sent, failed = outcome.failed, "notifications delivered");
            }
            Ok(_) => {}
            Err(error) => tracing::warn!(%error, "notification pass failed"),
        }
        deduplicator.prune();
    }
}

/// Run inventory discovery on a schedule.
async fn discovery_loop(controller: Arc<Mutex<Controller>>, interval: std::time::Duration) {
    // Jitter so a fleet of controllers, or a controller restarted in lockstep
    // with others, does not synchronise its load (SPEC.md §123).
    let jitter = jitter_for(interval);
    let mut ticker = tokio::time::interval(interval);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    loop {
        ticker.tick().await;
        tokio::time::sleep(jitter).await;

        let mut controller = controller.lock().await;
        match controller.discover_once().await {
            Ok(report) => {
                if !report.all_providers_ok() {
                    for failure in report.failures() {
                        tracing::warn!(
                            provider = %failure.provider,
                            error = failure.error.as_deref().unwrap_or_default(),
                            "discovery provider failed"
                        );
                    }
                }
                tracing::debug!(
                    entities = report.entities,
                    observations = report.observations,
                    transitions = report.transitions.len(),
                    "discovery cycle complete"
                );
            }
            // A failed cycle must never end the loop: the next one may succeed,
            // and a monitoring daemon that gives up during an outage is useless.
            Err(error) => tracing::error!(%error, "discovery cycle failed"),
        }
    }
}

/// A deterministic-per-process jitter of up to 10% of the interval.
fn jitter_for(interval: std::time::Duration) -> std::time::Duration {
    let span = interval.as_millis() as u64 / 10;
    if span == 0 {
        return std::time::Duration::ZERO;
    }
    // Derived from the process id rather than a random number generator: enough
    // to break lockstep between hosts, and reproducible within one process.
    std::time::Duration::from_millis(u64::from(std::process::id()) % span)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::persistence::SqliteStore;
    use crate::protocol::{RegisterRequest, API_PREFIX};

    const TOKEN: &str = "0123456789abcdef0123456789abcdef";

    #[tokio::test]
    async fn the_diagnosis_loop_reads_the_windows_an_operator_declared() {
        // The step that did not exist. `sentinel maintenance start` writes a
        // row; without this read, the row was inert and the suppression branch
        // in `notify` could never be taken in a real controller.
        let config = Config {
            config_version: 1,
            environment: "lab".into(),
            ..Config::default()
        };
        let store = SqliteStore::open_in_memory().await.expect("store");
        let controller = Controller::new(config, store).await.expect("controller");

        assert!(
            active_maintenance(&controller).await.is_empty(),
            "nothing declared, nothing suppressed"
        );

        controller
            .store()
            .save_maintenance_window("lab", &crate::notification::MaintenanceWindow::for_environment("work"))
            .await
            .expect("save");

        assert_eq!(
            active_maintenance(&controller).await.len(),
            1,
            "the loop did not see a declared window"
        );
    }

    async fn start(discovery_interval: Option<std::time::Duration>) -> ServerHandle {
        let config = Config {
            config_version: 1,
            environment: "lab".into(),
            ..Config::default()
        };
        let store = SqliteStore::open_in_memory().await.expect("store");
        let controller = Controller::new(config, store).await.expect("controller");

        serve(
            controller,
            ServeOptions {
                // Port 0: the OS picks a free port, so tests never collide.
                listen: "127.0.0.1:0".into(),
                credential: ClusterCredential::new(TOKEN),
                heartbeat_interval: std::time::Duration::from_secs(5),
                discovery_interval,
                diagnosis_interval: None,
                retention: None,
                tls: None,
            },
        )
        .await
        .expect("serve")
    }

    #[tokio::test]
    async fn the_server_binds_and_answers_health() {
        let handle = start(None).await;
        let url = format!("http://{}{API_PREFIX}/health", handle.local_addr);

        let body: serde_json::Value = reqwest::get(&url).await.expect("get").json().await.expect("json");
        assert_eq!(body["environment"], "lab");
        assert_eq!(body["protocol_version"], crate::PROTOCOL_VERSION);

        handle.shutdown().await;
    }

    #[tokio::test]
    async fn an_agent_can_register_over_http() {
        let handle = start(None).await;
        let url = format!("http://{}{API_PREFIX}/agents/register", handle.local_addr);
        let request = RegisterRequest::new("lab", "node-a", ["host.metrics"].into_iter().collect());

        let response = reqwest::Client::new()
            .post(&url)
            .bearer_auth(TOKEN)
            .json(&request)
            .send()
            .await
            .expect("register");

        assert!(response.status().is_success());
        let body: serde_json::Value = response.json().await.expect("json");
        assert!(body["session_id"].is_string());

        handle.shutdown().await;
    }

    #[tokio::test]
    async fn registration_without_the_credential_is_refused_over_http() {
        let handle = start(None).await;
        let url = format!("http://{}{API_PREFIX}/agents/register", handle.local_addr);
        let request = RegisterRequest::new("lab", "node-a", Default::default());

        let response = reqwest::Client::new()
            .post(&url)
            .json(&request)
            .send()
            .await
            .expect("request");
        assert_eq!(response.status(), reqwest::StatusCode::UNAUTHORIZED);

        handle.shutdown().await;
    }

    #[tokio::test]
    async fn shutdown_stops_the_listener() {
        let handle = start(None).await;
        let addr = handle.local_addr;
        handle.shutdown().await;

        // Give the OS a moment to release the socket, then confirm it is gone.
        for _ in 0..50 {
            if reqwest::get(format!("http://{addr}{API_PREFIX}/health")).await.is_err() {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        panic!("the server was still answering after shutdown");
    }

    #[tokio::test]
    async fn the_discovery_loop_runs_without_blocking_the_api() {
        let handle = start(Some(std::time::Duration::from_millis(50))).await;
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;

        let url = format!("http://{}{API_PREFIX}/health", handle.local_addr);
        assert!(reqwest::get(&url).await.expect("get").status().is_success());

        handle.shutdown().await;
    }

    #[test]
    fn jitter_is_bounded_by_a_tenth_of_the_interval() {
        for seconds in [1u64, 5, 60, 300] {
            let interval = std::time::Duration::from_secs(seconds);
            assert!(jitter_for(interval) < interval / 10 + std::time::Duration::from_millis(1));
        }
    }

    #[test]
    fn a_tiny_interval_produces_no_jitter_rather_than_dividing_by_zero() {
        assert_eq!(
            jitter_for(std::time::Duration::from_millis(5)),
            std::time::Duration::ZERO
        );
        assert_eq!(jitter_for(std::time::Duration::ZERO), std::time::Duration::ZERO);
    }
}
