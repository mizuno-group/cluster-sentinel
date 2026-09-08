//! The controller's HTTP API (IMPLEMENTATION.md §63).
//!
//! Every route here is either read-only or an append. There is no endpoint that
//! changes anything on a monitored host, and none that takes a command to run.

use std::sync::Arc;

use axum::extract::Path;
use axum::extract::{Json, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::Router;
use tokio::sync::Mutex;

use crate::protocol::{
    check_version, AssignmentsResponse, ClusterCredential, ErrorResponse, HealthResponse, HeartbeatRequest,
    HeartbeatResponse, ObservationBatch, ObservationBatchResponse, RegisterRequest, RegisterResponse, API_PREFIX,
    AUTH_HEADER,
};
use crate::time::now;
use crate::PROTOCOL_VERSION;

use super::agents::AgentRegistry;
use super::Controller;

/// Everything a request handler needs.
#[derive(Clone)]
pub struct ApiState {
    /// The controller, behind a lock: ingestion mutates the state engine.
    pub controller: Arc<Mutex<Controller>>,
    /// Registered agents.
    pub agents: Arc<Mutex<AgentRegistry>>,
    /// The credential every authenticated route requires.
    pub credential: Arc<ClusterCredential>,
    /// How often agents should heartbeat.
    pub heartbeat_interval: std::time::Duration,
}

/// Build the router.
pub fn router(state: ApiState) -> Router {
    Router::new()
        // Unauthenticated on purpose: a peer must be able to tell that the
        // controller is alive without holding the cluster credential, and this
        // route reveals nothing an attacker on the network does not know.
        .route(&format!("{API_PREFIX}/health"), get(health))
        .route(&format!("{API_PREFIX}/agents/register"), post(register))
        .route(&format!("{API_PREFIX}/agents/heartbeat"), post(heartbeat))
        .route(&format!("{API_PREFIX}/observations/batch"), post(observations))
        .route(
            &format!("{API_PREFIX}/agents/{{agent_id}}/assignments"),
            get(assignments),
        )
        .with_state(state)
}

/// An error with an HTTP status.
struct ApiError(StatusCode, ErrorResponse);

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (self.0, Json(self.1)).into_response()
    }
}

impl ApiError {
    fn unauthorized(message: impl Into<String>) -> Self {
        Self(StatusCode::UNAUTHORIZED, ErrorResponse::new("unauthorized", message))
    }

    fn bad_version(offered: u32) -> Self {
        Self(
            StatusCode::BAD_REQUEST,
            ErrorResponse::new(
                "protocol_version_mismatch",
                format!("this controller speaks protocol version {PROTOCOL_VERSION}, not {offered}"),
            ),
        )
    }

    fn internal(message: impl Into<String>) -> Self {
        Self(
            StatusCode::INTERNAL_SERVER_ERROR,
            ErrorResponse::new("internal_error", message),
        )
    }
}

fn authenticate(state: &ApiState, headers: &HeaderMap) -> Result<(), ApiError> {
    let header = headers.get(AUTH_HEADER).and_then(|v| v.to_str().ok());
    state
        .credential
        .verify_header(header)
        .map_err(|e| ApiError::unauthorized(e.to_string()))
}

async fn health(State(state): State<ApiState>) -> Result<Json<HealthResponse>, ApiError> {
    let controller = state.controller.lock().await;
    let environment = controller.config().environment.clone();
    let entities = controller
        .store()
        .load_entities(&environment)
        .await
        .map_err(|e| ApiError::internal(e.to_string()))?
        .len();

    Ok(Json(HealthResponse {
        protocol_version: PROTOCOL_VERSION,
        version: crate::VERSION.to_string(),
        environment,
        server_time: now(),
        entities,
        agents: state.agents.lock().await.len(),
    }))
}

async fn register(
    State(state): State<ApiState>,
    headers: HeaderMap,
    Json(request): Json<RegisterRequest>,
) -> Result<Json<RegisterResponse>, ApiError> {
    authenticate(&state, &headers)?;
    check_version(request.protocol_version).map_err(|m| ApiError::bad_version(m.offered))?;

    let mut controller = state.controller.lock().await;
    let registration = controller
        .register_agent(&request)
        .await
        .map_err(|e| ApiError::internal(e.to_string()))?;

    let session = state.agents.lock().await.register(&request, registration.entity_id);

    Ok(Json(RegisterResponse {
        protocol_version: PROTOCOL_VERSION,
        agent_id: session.agent_id,
        session_id: session.session_id,
        entity_id: registration.entity_id.to_string(),
        server_time: now(),
        heartbeat_interval: state.heartbeat_interval,
    }))
}

async fn heartbeat(
    State(state): State<ApiState>,
    headers: HeaderMap,
    Json(request): Json<HeartbeatRequest>,
) -> Result<Json<HeartbeatResponse>, ApiError> {
    authenticate(&state, &headers)?;
    check_version(request.protocol_version).map_err(|m| ApiError::bad_version(m.offered))?;

    let server_time = now();
    let mut agents = state.agents.lock().await;
    let outcome = agents.heartbeat(&request, server_time);

    Ok(Json(HeartbeatResponse {
        protocol_version: PROTOCOL_VERSION,
        server_time,
        // Positive means the agent's clock is ahead of the controller's.
        clock_skew_ms: (request.agent_time - server_time).num_milliseconds(),
        reregister: outcome.reregister,
        assignment_revision: outcome.assignment_revision,
    }))
}

async fn observations(
    State(state): State<ApiState>,
    headers: HeaderMap,
    Json(batch): Json<ObservationBatch>,
) -> Result<Json<ObservationBatchResponse>, ApiError> {
    authenticate(&state, &headers)?;
    check_version(batch.protocol_version).map_err(|m| ApiError::bad_version(m.offered))?;

    let mut controller = state.controller.lock().await;
    let outcome = controller
        .ingest_agent_batch(&batch)
        .await
        .map_err(|e| ApiError::internal(e.to_string()))?;

    state.agents.lock().await.touch(batch.agent_id, now());

    Ok(Json(outcome))
}

async fn assignments(
    State(state): State<ApiState>,
    Path(agent_id): Path<String>,
    headers: HeaderMap,
) -> Result<Json<AssignmentsResponse>, ApiError> {
    authenticate(&state, &headers)?;

    let agent_id: uuid::Uuid = agent_id.parse().map_err(|_| {
        ApiError(
            StatusCode::BAD_REQUEST,
            ErrorResponse::new("bad_agent_id", "not a valid agent id"),
        )
    })?;

    // An agent that is not registered gets an empty plan rather than an error:
    // it will re-register on its next heartbeat, and refusing here would put a
    // scary line in its log for an ordinary startup race.
    let observer = state.agents.lock().await.get(agent_id).map(|session| session.entity_id);

    let controller = state.controller.lock().await;
    let plan = controller
        .assignment_plan()
        .await
        .map_err(|e| ApiError::internal(e.to_string()))?;

    let targets = match observer {
        Some(observer) => controller
            .assigned_targets(&plan, observer)
            .await
            .map_err(|e| ApiError::internal(e.to_string()))?,
        None => Vec::new(),
    };

    Ok(Json(AssignmentsResponse {
        protocol_version: PROTOCOL_VERSION,
        revision: plan.revision,
        targets,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::capability::CapabilitySet;
    use crate::config::Config;
    use crate::entity::{EntityKey, EntityType};
    use crate::observation::{Observation, ProbeStatus};
    use crate::persistence::SqliteStore;
    use crate::probes::ProbeId;
    use axum::body::Body;
    use axum::http::Request;
    use tower::ServiceExt;

    const TOKEN: &str = "0123456789abcdef0123456789abcdef";

    async fn test_router() -> Router {
        let config = Config {
            config_version: 1,
            environment: "lab".into(),
            ..Config::default()
        };
        let store = SqliteStore::open_in_memory().await.expect("store");
        let controller = Controller::new(config, store).await.expect("controller");

        router(ApiState {
            controller: Arc::new(Mutex::new(controller)),
            agents: Arc::new(Mutex::new(AgentRegistry::new())),
            credential: Arc::new(ClusterCredential::new(TOKEN)),
            heartbeat_interval: std::time::Duration::from_secs(5),
        })
    }

    fn request(path: &str, body: serde_json::Value, token: Option<&str>) -> Request<Body> {
        let mut builder = Request::builder()
            .method("POST")
            .uri(path)
            .header("content-type", "application/json");
        if let Some(token) = token {
            builder = builder.header(AUTH_HEADER, format!("Bearer {token}"));
        }
        builder.body(Body::from(body.to_string())).expect("build request")
    }

    async fn body_json(response: Response) -> serde_json::Value {
        let bytes = axum::body::to_bytes(response.into_body(), 1 << 20)
            .await
            .expect("read body");
        serde_json::from_slice(&bytes).expect("parse body")
    }

    fn registration() -> serde_json::Value {
        serde_json::to_value(RegisterRequest::new(
            "lab",
            "node-a",
            CapabilitySet::from_iter(["host.metrics", "ssh.server"]),
        ))
        .expect("serialize")
    }

    #[tokio::test]
    async fn health_is_reachable_without_a_credential() {
        // A peer must be able to see that the controller is alive.
        let response = test_router()
            .await
            .oneshot(Request::builder().uri("/v1/health").body(Body::empty()).unwrap())
            .await
            .expect("request");

        assert_eq!(response.status(), StatusCode::OK);
        let body = body_json(response).await;
        assert_eq!(body["protocol_version"], PROTOCOL_VERSION);
        assert_eq!(body["environment"], "lab");
    }

    #[tokio::test]
    async fn registration_without_a_credential_is_refused() {
        let response = test_router()
            .await
            .oneshot(request("/v1/agents/register", registration(), None))
            .await
            .expect("request");
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn registration_with_the_wrong_credential_is_refused() {
        let response = test_router()
            .await
            .oneshot(request("/v1/agents/register", registration(), Some("wrong")))
            .await
            .expect("request");
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn a_valid_registration_creates_the_host_entity() {
        let router = test_router().await;
        let response = router
            .clone()
            .oneshot(request("/v1/agents/register", registration(), Some(TOKEN)))
            .await
            .expect("request");

        assert_eq!(response.status(), StatusCode::OK);
        let body = body_json(response).await;
        assert_eq!(
            body["entity_id"],
            EntityKey::new("lab", EntityType::Host, "node-a")
                .entity_id()
                .to_string()
        );
        assert!(body["agent_id"].is_string());
        assert!(body["session_id"].is_string());
    }

    #[tokio::test]
    async fn a_mismatched_protocol_version_is_refused_with_a_clear_error() {
        let mut body = registration();
        body["protocol_version"] = serde_json::json!(PROTOCOL_VERSION + 1);

        let response = test_router()
            .await
            .oneshot(request("/v1/agents/register", body, Some(TOKEN)))
            .await
            .expect("request");

        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        assert_eq!(body_json(response).await["error"], "protocol_version_mismatch");
    }

    #[tokio::test]
    async fn a_heartbeat_reports_the_measured_clock_skew() {
        let router = test_router().await;
        let registered = body_json(
            router
                .clone()
                .oneshot(request("/v1/agents/register", registration(), Some(TOKEN)))
                .await
                .expect("register"),
        )
        .await;

        let ahead = now() + chrono::Duration::seconds(30);
        let heartbeat = serde_json::json!({
            "protocol_version": PROTOCOL_VERSION,
            "agent_id": registered["agent_id"],
            "session_id": registered["session_id"],
            "agent_time": crate::time::to_rfc3339(ahead),
            "spooled_observations": 0,
        });

        let body = body_json(
            router
                .oneshot(request("/v1/agents/heartbeat", heartbeat, Some(TOKEN)))
                .await
                .expect("heartbeat"),
        )
        .await;

        let skew = body["clock_skew_ms"].as_i64().expect("skew");
        assert!((29_000..=31_000).contains(&skew), "skew was {skew}");
        assert_eq!(body["reregister"], false);
    }

    #[tokio::test]
    async fn a_heartbeat_from_an_unknown_agent_asks_it_to_register_again() {
        // The controller lost its memory, or the agent is from a previous life.
        let heartbeat = serde_json::json!({
            "protocol_version": PROTOCOL_VERSION,
            "agent_id": uuid::Uuid::new_v4(),
            "session_id": uuid::Uuid::new_v4(),
            "agent_time": crate::time::to_rfc3339(now()),
        });

        let response = test_router()
            .await
            .oneshot(request("/v1/agents/heartbeat", heartbeat, Some(TOKEN)))
            .await
            .expect("heartbeat");

        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(body_json(response).await["reregister"], true);
    }

    #[tokio::test]
    async fn an_observation_batch_is_stored_and_replay_is_idempotent() {
        let router = test_router().await;
        let registered = body_json(
            router
                .clone()
                .oneshot(request("/v1/agents/register", registration(), Some(TOKEN)))
                .await
                .expect("register"),
        )
        .await;

        let entity = EntityKey::new("lab", EntityType::Host, "node-a").entity_id();
        let batch = serde_json::json!({
            "protocol_version": PROTOCOL_VERSION,
            "agent_id": registered["agent_id"],
            "session_id": registered["session_id"],
            "observations": [Observation::new(ProbeId::new("host.metrics"), entity, ProbeStatus::Ok)],
        });

        let first = body_json(
            router
                .clone()
                .oneshot(request("/v1/observations/batch", batch.clone(), Some(TOKEN)))
                .await
                .expect("batch"),
        )
        .await;
        assert_eq!(first["accepted"], 1);
        assert_eq!(first["duplicates"], 0);

        let replay = body_json(
            router
                .oneshot(request("/v1/observations/batch", batch, Some(TOKEN)))
                .await
                .expect("replay"),
        )
        .await;
        assert_eq!(replay["accepted"], 0);
        assert_eq!(replay["duplicates"], 1, "a spool replay must not double-count");
    }

    #[tokio::test]
    async fn an_observation_batch_without_a_credential_is_refused() {
        let batch = serde_json::json!({
            "protocol_version": PROTOCOL_VERSION,
            "agent_id": uuid::Uuid::new_v4(),
            "session_id": uuid::Uuid::new_v4(),
            "observations": [],
        });
        let response = test_router()
            .await
            .oneshot(request("/v1/observations/batch", batch, None))
            .await
            .expect("request");
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn there_is_no_route_that_executes_anything() {
        // SPEC.md §116, checked rather than assumed.
        let router = test_router().await;
        for path in [
            "/v1/exec",
            "/v1/agents/exec",
            "/v1/command",
            "/v1/run",
            "/v1/probes/run",
        ] {
            let response = router
                .clone()
                .oneshot(request(path, serde_json::json!({}), Some(TOKEN)))
                .await
                .expect("request");
            assert_eq!(response.status(), StatusCode::NOT_FOUND, "{path} must not exist");
        }
    }
}
