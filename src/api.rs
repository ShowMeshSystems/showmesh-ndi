use std::sync::Arc;

use axum::extract::{Path, State};
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post, put};
use axum::{Json, Router};
use serde::{Deserialize, Deserializer, Serialize};
use serde_json::json;

use crate::config::{ModeSpec, SourceConfig};
use crate::manager::{ApiError, Manager, OutputView};
use crate::receiver::State as RxState;

pub const OPENAPI: &str = include_str!("../api/openapi.yaml");

pub fn router(manager: Arc<Manager>) -> Router {
    Router::new()
        .route("/api/v1/device", get(device).patch(patch_device))
        .route("/api/v1/health", get(health))
        .route("/api/v1/sources", get(sources))
        .route("/api/v1/config", get(config))
        .route("/api/v1/outputs", get(outputs))
        .route("/api/v1/outputs/{name}", get(output).patch(patch_output))
        .route("/api/v1/outputs/{name}/source", put(put_source).delete(delete_source))
        .route("/api/v1/outputs/{name}/start", post(start))
        .route("/api/v1/outputs/{name}/stop", post(stop))
        .route("/api/v1/outputs/{name}/restart", post(restart))
        .route("/api/v1/openapi.yaml", get(openapi))
        .with_state(manager)
}

type AppState = State<Arc<Manager>>;

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let (status, message) = match self {
            ApiError::NotFound(m) => (StatusCode::NOT_FOUND, m),
            ApiError::Invalid(m) => (StatusCode::UNPROCESSABLE_ENTITY, m),
            ApiError::Internal(m) => (StatusCode::INTERNAL_SERVER_ERROR, m),
        };
        (status, Json(json!({ "error": message }))).into_response()
    }
}

/// Runs a manager call that may join a receiver thread off the async runtime.
async fn blocking<T: Send + 'static>(f: impl FnOnce() -> Result<T, ApiError> + Send + 'static) -> Result<T, ApiError> {
    tokio::task::spawn_blocking(f).await.map_err(|e| ApiError::Internal(format!("Request failed: {e}.")))?
}

#[derive(Serialize)]
struct DeviceInfo {
    name: String,
    software: &'static str,
    version: &'static str,
    hostname: String,
    display_mode: crate::config::SinkKind,
    #[serde(skip_serializing_if = "Option::is_none")]
    display_card: Option<String>,
    uptime_s: u64,
    ndi_discovery: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    ndi_message: Option<String>,
}

async fn device(State(m): AppState) -> Json<DeviceInfo> {
    let discovery = m.discovery();
    Json(DeviceInfo {
        name: m.config().device.name,
        software: "showmesh-ndi",
        version: env!("CARGO_PKG_VERSION"),
        hostname: std::fs::read_to_string("/etc/hostname").unwrap_or_default().trim().to_string(),
        display_mode: m.sink(),
        display_card: m.card(),
        uptime_s: m.uptime().as_secs(),
        ndi_discovery: discovery.running,
        ndi_message: discovery.message,
    })
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct DevicePatch {
    name: Option<String>,
}

async fn patch_device(State(m): AppState, Json(body): Json<DevicePatch>) -> Result<Json<DeviceInfo>, ApiError> {
    if let Some(name) = body.name {
        let name = name.trim().to_string();
        if name.is_empty() {
            return Err(ApiError::Invalid("The device name is empty. Send a name with at least one character.".into()));
        }
        m.update_config(|c| c.device.name = name)?;
    }
    Ok(device(State(m)).await)
}

#[derive(Serialize)]
struct Health {
    status: &'static str,
    outputs: Vec<OutputHealth>,
}

#[derive(Serialize)]
struct OutputHealth {
    name: String,
    connected: bool,
    #[serde(flatten)]
    receiver: Option<crate::receiver::ReceiverStatus>,
}

async fn health(State(m): AppState) -> Json<Health> {
    let outputs: Vec<OutputHealth> = m
        .outputs()
        .into_iter()
        .filter(|o| o.connected || o.config.source.is_some())
        .map(|o| OutputHealth { name: o.name, connected: o.connected, receiver: o.receiver })
        .collect();
    let degraded = outputs.iter().any(|o| {
        o.receiver.as_ref().is_some_and(|r| {
            r.source.is_some() && !matches!(r.state, RxState::Playing | RxState::Stopped | RxState::Idle)
        })
    });
    Json(Health { status: if degraded { "degraded" } else { "ok" }, outputs })
}

async fn sources(State(m): AppState) -> impl IntoResponse {
    Json(m.discovery())
}

async fn config(State(m): AppState) -> impl IntoResponse {
    Json(m.config())
}

async fn outputs(State(m): AppState) -> Json<Vec<OutputView>> {
    Json(m.outputs())
}

async fn output(State(m): AppState, Path(name): Path<String>) -> Result<Json<OutputView>, ApiError> {
    m.output(&name).map(Json)
}

fn present<'de, T: Deserialize<'de>, D: Deserializer<'de>>(d: D) -> Result<Option<T>, D::Error> {
    T::deserialize(d).map(Some)
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct OutputPatch {
    enabled: Option<bool>,
    /// Absent leaves the mode alone; null returns to the display's preferred mode.
    #[serde(default, deserialize_with = "present")]
    mode: Option<Option<String>>,
    #[serde(default, deserialize_with = "present")]
    source: Option<Option<SourceConfig>>,
}

async fn patch_output(
    State(m): AppState,
    Path(name): Path<String>,
    Json(body): Json<OutputPatch>,
) -> Result<Json<OutputView>, ApiError> {
    if let Some(Some(mode)) = &body.mode {
        check_mode(&m, &name, mode)?;
    }
    blocking(move || {
        m.update_output(&name, |o| {
            if let Some(enabled) = body.enabled {
                o.enabled = enabled;
            }
            if let Some(mode) = body.mode {
                o.mode = mode;
            }
            if let Some(source) = body.source {
                o.source = source;
            }
        })
    })
    .await
    .map(Json)
}

fn check_mode(m: &Manager, name: &str, mode: &str) -> Result<(), ApiError> {
    let spec = ModeSpec::parse(mode).map_err(|e| ApiError::Invalid(e.to_string()))?;
    let out = m.output(name)?;
    let supported = out
        .modes
        .iter()
        .any(|md| md.width == spec.width && md.height == spec.height && spec.refresh.is_none_or(|r| r == md.refresh));
    if out.connected && !out.modes.is_empty() && !supported {
        return Err(ApiError::Invalid(format!(
            "Output {name} does not support {mode}. Pick a mode from the output's mode list."
        )));
    }
    Ok(())
}

async fn put_source(
    State(m): AppState,
    Path(name): Path<String>,
    Json(source): Json<SourceConfig>,
) -> Result<Json<OutputView>, ApiError> {
    blocking(move || m.update_output(&name, |o| o.source = Some(source))).await.map(Json)
}

async fn delete_source(State(m): AppState, Path(name): Path<String>) -> Result<Json<OutputView>, ApiError> {
    blocking(move || m.update_output(&name, |o| o.source = None)).await.map(Json)
}

async fn start(State(m): AppState, Path(name): Path<String>) -> Result<Json<OutputView>, ApiError> {
    blocking(move || m.update_output(&name, |o| o.enabled = true)).await.map(Json)
}

async fn stop(State(m): AppState, Path(name): Path<String>) -> Result<Json<OutputView>, ApiError> {
    blocking(move || m.update_output(&name, |o| o.enabled = false)).await.map(Json)
}

async fn restart(State(m): AppState, Path(name): Path<String>) -> Result<Json<OutputView>, ApiError> {
    blocking(move || m.restart(&name)).await.map(Json)
}

async fn openapi() -> impl IntoResponse {
    ([(header::CONTENT_TYPE, "application/yaml")], OPENAPI)
}
