//! Exercises the REST API against real GStreamer pipelines on the fake display.

use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::Router;
use axum::body::Body;
use axum::http::{Method, Request, StatusCode};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use tower::ServiceExt;

use showmesh_ndi::config::{Config, SinkKind};
use showmesh_ndi::manager::Manager;

struct Bench {
    app: Router,
    manager: Arc<Manager>,
    path: std::path::PathBuf,
}

impl Drop for Bench {
    fn drop(&mut self) {
        self.manager.shutdown();
        let _ = std::fs::remove_dir_all(self.path.parent().unwrap());
    }
}

fn bench(name: &str) -> Bench {
    showmesh_ndi::init_gstreamer().unwrap();
    let dir = std::env::temp_dir().join(format!("showmesh-ndi-{name}-{}", std::process::id()));
    let path = dir.join("config.toml");
    let mut config = Config::default();
    config.display.sink = SinkKind::Fake;
    config.display.fake_outputs = 2;
    let manager = Manager::new(config, Some(path.clone())).unwrap();
    manager.start();
    Bench { app: showmesh_ndi::api::router(manager.clone()), manager, path }
}

async fn call(app: &Router, method: Method, uri: &str, body: Option<Value>) -> (StatusCode, Value) {
    let req = Request::builder().method(method).uri(uri).header("content-type", "application/json");
    let req = req.body(body.map(|b| Body::from(b.to_string())).unwrap_or_else(Body::empty)).unwrap();
    let res = app.clone().oneshot(req).await.unwrap();
    let status = res.status();
    let bytes = res.into_body().collect().await.unwrap().to_bytes();
    (status, serde_json::from_slice(&bytes).unwrap_or(Value::Null))
}

async fn wait_for_state(app: &Router, output: &str, state: &str) -> Value {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let (_, body) = call(app, Method::GET, &format!("/api/v1/outputs/{output}"), None).await;
        if body["receiver"]["state"] == state {
            return body;
        }
        assert!(Instant::now() < deadline, "{output} never reached {state}: {body}");
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn assigns_a_source_and_reports_frames() {
    let b = bench("assign");
    let (status, outputs) = call(&b.app, Method::GET, "/api/v1/outputs", None).await;
    assert_eq!(status, StatusCode::OK);
    let names: Vec<&str> = outputs.as_array().unwrap().iter().map(|o| o["name"].as_str().unwrap()).collect();
    assert_eq!(names, ["FAKE-1", "FAKE-2"]);
    wait_for_state(&b.app, "FAKE-1", "idle").await;

    let (status, _) = call(
        &b.app,
        Method::PUT,
        "/api/v1/outputs/FAKE-1/source",
        Some(json!({"type": "test_pattern", "pattern": "smpte"})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    wait_for_state(&b.app, "FAKE-1", "playing").await;
    tokio::time::sleep(Duration::from_millis(1500)).await;
    let (_, out) = call(&b.app, Method::GET, "/api/v1/outputs/FAKE-1", None).await;
    let rx = &out["receiver"];
    assert!(rx["fps"].as_f64().unwrap() > 30.0, "{rx}");
    assert!(rx["frames_displayed"].as_u64().unwrap() > 30, "{rx}");
    assert_eq!(rx["video"]["width"], 1920);
    assert_eq!(rx["output_mode"], "1920x1080@60");

    let (_, health) = call(&b.app, Method::GET, "/api/v1/health", None).await;
    assert_eq!(health["status"], "ok");

    let saved = std::fs::read_to_string(&b.path).unwrap();
    assert!(saved.contains("test_pattern"), "{saved}");

    let (status, _) = call(&b.app, Method::DELETE, "/api/v1/outputs/FAKE-1/source", None).await;
    assert_eq!(status, StatusCode::OK);
    wait_for_state(&b.app, "FAKE-1", "idle").await;
}

#[tokio::test(flavor = "multi_thread")]
async fn stop_start_and_mode() {
    let b = bench("stopstart");
    let src = json!({"source": {"type": "test_pattern", "pattern": "ball"}, "mode": "1280x720@60"});
    let (status, _) = call(&b.app, Method::PATCH, "/api/v1/outputs/FAKE-2", Some(src)).await;
    assert_eq!(status, StatusCode::OK);
    let out = wait_for_state(&b.app, "FAKE-2", "playing").await;
    assert_eq!(out["receiver"]["output_mode"], "1280x720@60");

    call(&b.app, Method::POST, "/api/v1/outputs/FAKE-2/stop", None).await;
    let out = wait_for_state(&b.app, "FAKE-2", "stopped").await;
    assert_eq!(out["config"]["enabled"], false);

    call(&b.app, Method::POST, "/api/v1/outputs/FAKE-2/start", None).await;
    wait_for_state(&b.app, "FAKE-2", "playing").await;

    call(&b.app, Method::POST, "/api/v1/outputs/FAKE-2/restart", None).await;
    wait_for_state(&b.app, "FAKE-2", "playing").await;

    // An NDI sender that does not exist leaves the other output playing.
    let ndi = json!({"type": "ndi", "name": "NOWHERE (Missing)"});
    call(&b.app, Method::PUT, "/api/v1/outputs/FAKE-1/source", Some(ndi)).await;
    let (_, health) = call(&b.app, Method::GET, "/api/v1/health", None).await;
    assert_eq!(health["status"], "degraded");
    wait_for_state(&b.app, "FAKE-2", "playing").await;
}

#[tokio::test(flavor = "multi_thread")]
async fn rejects_bad_requests() {
    let b = bench("errors");
    let (status, body) = call(&b.app, Method::GET, "/api/v1/outputs/HDMI-A-9", None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert!(body["error"].as_str().unwrap().contains("HDMI-A-9"));

    let (status, _) = call(&b.app, Method::PUT, "/api/v1/outputs/FAKE-1/source", Some(json!({"type": "ndi"}))).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);

    let (status, body) =
        call(&b.app, Method::PATCH, "/api/v1/outputs/FAKE-1", Some(json!({"mode": "3840x2160@60"}))).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert!(body["error"].as_str().unwrap().contains("does not support"));

    let (status, _) = call(&b.app, Method::PATCH, "/api/v1/device", Some(json!({"name": "  "}))).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);

    let (status, device) = call(&b.app, Method::PATCH, "/api/v1/device", Some(json!({"name": "rx-7"}))).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(device["name"], "rx-7");
    assert_eq!(device["version"], env!("CARGO_PKG_VERSION"));

    let (status, _) = call(&b.app, Method::GET, "/api/v1/sources", None).await;
    assert_eq!(status, StatusCode::OK);
}

#[tokio::test(flavor = "multi_thread")]
async fn serves_the_web_ui() {
    let b = bench("ui");
    let res = b.app.clone().oneshot(Request::get("/").body(Body::empty()).unwrap()).await.unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    assert!(res.headers()["content-type"].to_str().unwrap().starts_with("text/html"));
    let body = res.into_body().collect().await.unwrap().to_bytes();
    assert!(String::from_utf8_lossy(&body).contains("/api/v1/outputs"));
}

#[test]
fn openapi_describes_every_route() {
    for path in [
        "/api/v1/device:",
        "/api/v1/health:",
        "/api/v1/sources:",
        "/api/v1/config:",
        "/api/v1/outputs:",
        "/api/v1/outputs/{name}:",
        "/api/v1/outputs/{name}/source:",
        "/api/v1/outputs/{name}/start:",
        "/api/v1/outputs/{name}/stop:",
        "/api/v1/outputs/{name}/restart:",
        "/api/v1/openapi.yaml:",
    ] {
        assert!(showmesh_ndi::api::OPENAPI.contains(path), "openapi.yaml is missing {path}");
    }
    assert!(showmesh_ndi::api::OPENAPI.contains(&format!("version: {}", env!("CARGO_PKG_VERSION"))));
}
