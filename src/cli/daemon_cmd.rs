//! `sentinel controller` and `sentinel agent`.
//!
//! Both run until signalled, and both shut down gracefully: the controller
//! stops accepting work and closes its database, the agent makes one last
//! attempt to hand over its spool (SPEC.md §84).

use std::sync::Arc;
use std::time::Duration;

use crate::agent::rpc::{self, RpcState};
use crate::agent::{Agent, ControllerClient, LinuxInspector, Spool, SpoolLimits};
use crate::config::{Config, DEFAULT_STATE_DIR};
use crate::controller::{serve, Controller, ServeOptions};
use crate::persistence::SqliteStore;
use crate::protocol::ClusterCredential;

use super::Cli;

/// Environment variable carrying the cluster credential.
pub const CREDENTIAL_ENV: &str = "SENTINEL_TOKEN";
/// Environment variable naming a file holding the cluster credential.
pub const CREDENTIAL_FILE_ENV: &str = "SENTINEL_TOKEN_FILE";

/// Default heartbeat interval (SPEC.md §123).
const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(5);

/// Load the cluster credential.
///
/// A file is preferred over an environment variable, and an environment
/// variable over nothing at all. There is no default and no built-in value:
/// unauthenticated operation is not offered (IMPLEMENTATION.md §65).
fn load_credential() -> anyhow::Result<ClusterCredential> {
    if let Ok(path) = std::env::var(CREDENTIAL_FILE_ENV) {
        let credential = ClusterCredential::from_file(std::path::Path::new(&path))
            .map_err(|e| anyhow::anyhow!("cannot read credential file {path}: {e}"))?;
        warn_if_weak(&credential);
        return Ok(credential);
    }

    if let Ok(token) = std::env::var(CREDENTIAL_ENV) {
        let credential = ClusterCredential::new(token);
        warn_if_weak(&credential);
        return Ok(credential);
    }

    anyhow::bail!(
        "no cluster credential: set {CREDENTIAL_FILE_ENV} to a file containing it, or {CREDENTIAL_ENV} directly. \
         Sentinel does not run unauthenticated."
    )
}

fn warn_if_weak(credential: &ClusterCredential) {
    if !credential.is_strong() {
        tracing::warn!(
            "the cluster credential is shorter than {} characters and is easy to guess",
            ClusterCredential::MIN_LENGTH
        );
    }
}

/// Wait for SIGTERM or Ctrl-C.
async fn wait_for_shutdown_signal() {
    #[cfg(unix)]
    {
        let mut terminate = match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(signal) => signal,
            Err(error) => {
                tracing::warn!(%error, "cannot listen for SIGTERM; Ctrl-C only");
                let _ = tokio::signal::ctrl_c().await;
                return;
            }
        };
        tokio::select! {
            _ = terminate.recv() => tracing::info!("received SIGTERM"),
            _ = tokio::signal::ctrl_c() => tracing::info!("received interrupt"),
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
}

/// `sentinel controller`.
pub async fn controller(cli: &Cli) -> anyhow::Result<i32> {
    let config = Config::load(&cli.config)?;
    let credential = load_credential()?;

    let store = SqliteStore::open(&config.database.path).await?;
    let listen = config.controller.listen.clone();
    let discovery_interval = config.controller.inventory_interval;
    let diagnosis_interval = config.controller.diagnosis_interval;
    let retention = config.retention.clone();
    // Built before anything else starts: a controller whose TLS material is
    // wrong should fail to start with the path in the message, not come up in
    // plaintext and be trusted to be encrypted.
    let tls_server_config = crate::protocol::tls::server_config(&config.tls)?;
    let controller = Controller::new(config, store.clone()).await?;

    let handle = serve(
        controller,
        ServeOptions {
            listen,
            credential,
            heartbeat_interval: HEARTBEAT_INTERVAL,
            discovery_interval: Some(discovery_interval),
            diagnosis_interval: Some(diagnosis_interval),
            retention: Some(retention),
            tls: tls_server_config,
        },
    )
    .await?;

    tracing::info!(address = %handle.local_addr, "controller started");
    wait_for_shutdown_signal().await;
    handle.shutdown().await;
    store.close().await;
    tracing::info!("controller stopped");
    Ok(0)
}

/// `sentinel agent`.
pub async fn agent(cli: &Cli) -> anyhow::Result<i32> {
    let config = Config::load(&cli.config)?;
    let credential = load_credential()?;

    let address = config
        .agent
        .controller_address
        .clone()
        .ok_or_else(|| anyhow::anyhow!("agent.controller_address is not configured"))?;

    let spool_path = config
        .agent
        .spool_path
        .clone()
        .unwrap_or_else(|| std::path::PathBuf::from(DEFAULT_STATE_DIR).join("spool.db"));

    let client = ControllerClient::with_tls(&address, &credential, Duration::from_secs(10), &config.tls)?;
    let spool = Spool::open(&spool_path, SpoolLimits::default()).await?;
    let inspector = Arc::new(LinuxInspector::new());
    let mut agent = Agent::new(&config, inspector, client, spool)?;

    tracing::info!(
        hostname = agent.hostname(),
        controller = %address,
        capabilities = agent.capabilities().len(),
        probes = agent.local_probes().len(),
        "agent starting"
    );

    // The health endpoint is what lets a peer tell "the agent is dead" from
    // "the host is dead", so it comes up before anything else and keeps
    // answering even when the controller is unreachable (SPEC.md §61). It reads
    // its own handles rather than the agent, so a slow probe cannot stall it.
    let rpc = rpc::serve(&config.agent.listen, RpcState::new(Arc::new(agent.health_handle()))).await?;
    tracing::info!(address = %rpc.local_addr, "agent health endpoint listening");

    let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel();
    let signal = tokio::spawn(async move {
        wait_for_shutdown_signal().await;
        let _ = shutdown_tx.send(());
    });

    agent.run(HEARTBEAT_INTERVAL, shutdown_rx).await;
    signal.abort();
    rpc.shutdown().await;
    agent.spool().close().await;
    tracing::info!("agent stopped");
    Ok(0)
}

/// Whether a cluster credential is reachable, and from where.
///
/// The environment is not the whole answer, and reporting only on it told
/// operators their credential was missing while the daemon beside them was
/// using it perfectly well: the daemon gets the path from its unit, and an
/// interactive shell has no such variable. The file the unit would name is
/// checked too, so `doctor` describes the host rather than the shell it
/// happens to be running in.
fn credential_status(config: &std::path::Path) -> (bool, String) {
    credential_status_from(
        std::env::var(CREDENTIAL_ENV).ok(),
        std::env::var(CREDENTIAL_FILE_ENV).ok(),
        config,
    )
}

/// [`credential_status`] with the environment passed in.
///
/// The environment is process-wide, so a test that set it would decide the
/// answer for every other test running beside it.
fn credential_status_from(
    env_token: Option<String>,
    env_file: Option<String>,
    config: &std::path::Path,
) -> (bool, String) {
    if env_token.is_some() {
        return (true, format!("configured ({CREDENTIAL_ENV})"));
    }
    if let Some(path) = env_file {
        let readable = std::fs::metadata(&path).is_ok();
        return (
            readable,
            if readable {
                format!("configured ({path})")
            } else {
                format!("NOT CONFIGURED ({CREDENTIAL_FILE_ENV} points at {path}, which cannot be read)")
            },
        );
    }

    let beside_config = crate::cli::install_cmd::token_path(config);
    match std::fs::metadata(&beside_config) {
        Ok(_) => (true, format!("configured ({})", beside_config.display())),
        // Present but unreadable is the ordinary case for an operator running
        // `doctor` as themselves: the daemon can read it, and saying "not
        // configured" would be wrong.
        Err(error) if error.kind() == std::io::ErrorKind::PermissionDenied => (
            true,
            format!(
                "present at {} (readable only by the service user)",
                beside_config.display()
            ),
        ),
        Err(_) => (false, "NOT CONFIGURED".to_string()),
    }
}

/// `sentinel doctor`: report what this host looks like to Sentinel.
///
/// Read-only and offline. Its job is to answer "why is this host not being
/// monitored the way I expected" before anything is deployed.
pub async fn doctor(cli: &Cli, json: bool) -> anyhow::Result<i32> {
    use crate::agent::{discovery, SystemInspector};
    use crate::capability::ResolutionReason;

    let config = Config::load(&cli.config).unwrap_or_default();
    let inspector = LinuxInspector::new();
    let resolution = discovery::resolve(&inspector, &config.capabilities, &config.agent.roles);

    let capabilities: Vec<_> = resolution
        .reasons
        .iter()
        .map(|(capability, reason)| {
            serde_json::json!({
                "capability": capability.as_str(),
                "enabled": reason.is_enabled(),
                "reason": reason,
            })
        })
        .collect();

    // The address is the first thing every peer uses and the last thing anyone
    // checks, so `doctor` shows which one would be reported and what else was
    // available, not merely the list.
    let address_choice = crate::agent::addressing::choose(&inspector, &config.agent);
    let candidates: Vec<serde_json::Value> = address_choice
        .candidates
        .iter()
        .map(|c| serde_json::json!({ "interface": c.interface, "address": c.address }))
        .collect();

    let report = serde_json::json!({
        "environment": config.environment,
        "hostname": inspector.hostname(),
        "fqdn": inspector.fqdn(),
        "boot_id": inspector.boot_id(),
        "address_reported": address_choice.primary(),
        "address_candidates": candidates,
        "address_warning": address_choice.warning(),
        "addresses": address_choice.addresses,
        "hardware": inspector.hardware(),
        "mounts": inspector.mounts().len(),
        "controller": config.agent.controller_address,
        "credential_configured": credential_status(&cli.config).0,
        "credential_source": credential_status(&cli.config).1,
        "capabilities": capabilities,
    });

    if json {
        println!("{}", serde_json::to_string_pretty(&report)?);
        return Ok(0);
    }

    println!(
        "Host:        {}",
        inspector.hostname().unwrap_or_else(|| "(unknown)".into())
    );
    println!("Environment: {}", config.environment);
    println!(
        "Controller:  {}",
        config.agent.controller_address.as_deref().unwrap_or("(not configured)")
    );
    let (credential_ok, credential_source) = credential_status(&cli.config);
    println!(
        "Credential:  {}",
        if credential_ok {
            credential_source
        } else {
            "NOT CONFIGURED".to_string()
        }
    );
    println!(
        "Address:     {}",
        address_choice
            .primary()
            .unwrap_or("(none — the controller will use this host's name)")
    );
    if address_choice.candidates.len() > 1 {
        for candidate in &address_choice.candidates {
            let mark = if Some(candidate.address.as_str()) == address_choice.primary() {
                "->"
            } else {
                "  "
            };
            println!("  {mark} {:<16} {}", candidate.interface, candidate.address);
        }
    }
    if let Some(warning) = address_choice.warning() {
        println!("  ! {warning}");
    }

    println!("\nCapabilities");
    println!("{}", "\u{2500}".repeat(60));
    for (capability, reason) in &resolution.reasons {
        let mark = if reason.is_enabled() { "on " } else { "off" };
        let explanation = match reason {
            ResolutionReason::ForcedDisabled => "disabled by configuration",
            ResolutionReason::ForcedEnabled => "forced on by configuration",
            ResolutionReason::Discovered => "detected on this host",
            ResolutionReason::NotDiscovered => "not present on this host",
            ResolutionReason::OperatorEnabled => "enabled by configuration",
            ResolutionReason::RoleHint => "suggested by a role",
        };
        println!("{mark}  {:<26} {explanation}", capability.as_str());
    }

    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Environment variables are process-global, so these tests must not run
    /// concurrently with each other.
    fn with_clean_env<T>(body: impl FnOnce() -> T) -> T {
        static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
        let _guard = LOCK.lock().unwrap_or_else(|e| e.into_inner());

        let previous = (
            std::env::var(CREDENTIAL_ENV).ok(),
            std::env::var(CREDENTIAL_FILE_ENV).ok(),
        );
        std::env::remove_var(CREDENTIAL_ENV);
        std::env::remove_var(CREDENTIAL_FILE_ENV);

        let result = body();

        match previous.0 {
            Some(value) => std::env::set_var(CREDENTIAL_ENV, value),
            None => std::env::remove_var(CREDENTIAL_ENV),
        }
        match previous.1 {
            Some(value) => std::env::set_var(CREDENTIAL_FILE_ENV, value),
            None => std::env::remove_var(CREDENTIAL_FILE_ENV),
        }
        result
    }

    #[test]
    fn doctor_reports_the_credential_the_daemon_would_use() {
        // Reported by an operator: `doctor` said NOT CONFIGURED on a host
        // whose daemon was authenticating fine. It only looked at the
        // environment, and a daemon gets the path from its unit -- so the
        // check described the shell rather than the host.
        let dir = tempfile::tempdir().expect("tempdir");
        let config = dir.path().join("config.toml");

        let (ok, message) = credential_status_from(None, None, &config);
        assert!(!ok, "{message}");

        std::fs::write(crate::cli::install_cmd::token_path(&config), "token\n").expect("write");
        let (ok, message) = credential_status_from(None, None, &config);
        assert!(ok, "{message}");
        assert!(message.contains("token"), "it should name what it found: {message}");
    }

    #[test]
    fn without_a_credential_the_daemons_refuse_to_start() {
        // Unauthenticated operation is not on offer (IMPLEMENTATION.md §65).
        with_clean_env(|| {
            let error = load_credential().expect_err("must refuse");
            assert!(error.to_string().contains("does not run unauthenticated"), "{error}");
        });
    }

    #[test]
    fn a_credential_can_come_from_the_environment() {
        with_clean_env(|| {
            std::env::set_var(CREDENTIAL_ENV, "0123456789abcdef0123456789abcdef");
            let credential = load_credential().expect("credential");
            assert!(credential.is_strong());
        });
    }

    #[test]
    fn a_credential_file_takes_precedence_over_the_environment() {
        // A file can be given permissions; an environment variable is visible
        // in the process table to anyone on the host.
        with_clean_env(|| {
            let dir = tempfile::tempdir().expect("tempdir");
            let path = dir.path().join("token");
            std::fs::write(&path, "fedcba9876543210fedcba9876543210\n").expect("write");

            std::env::set_var(CREDENTIAL_ENV, "0123456789abcdef0123456789abcdef");
            std::env::set_var(CREDENTIAL_FILE_ENV, path.display().to_string());

            let credential = load_credential().expect("credential");
            assert!(credential
                .verify_header(Some("Bearer fedcba9876543210fedcba9876543210"))
                .is_ok());
        });
    }

    #[test]
    fn an_unreadable_credential_file_is_an_error_not_a_silent_fallback() {
        with_clean_env(|| {
            std::env::set_var(CREDENTIAL_FILE_ENV, "/nonexistent/sentinel/token");
            std::env::set_var(CREDENTIAL_ENV, "0123456789abcdef0123456789abcdef");
            assert!(load_credential().is_err(), "falling back would hide a misconfiguration");
        });
    }
}
