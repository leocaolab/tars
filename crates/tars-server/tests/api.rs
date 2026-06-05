//! HTTP API tests — drive the router via `tower::oneshot` (no socket),
//! over a state backed by mock providers (no network / no live model).

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use tower::ServiceExt;

use tars_server::{AppState, router};

fn state_from(toml: &str, default_provider: Option<&str>) -> Arc<AppState> {
    let cfg = tars_config::ConfigManager::load_from_str(toml).expect("config parses");
    AppState::from_config(&cfg, default_provider.map(str::to_string)).expect("state builds")
}

/// One *user-added* mock provider, set as the default. (Config also
/// carries the built-in provider table, so the registry holds more.)
fn single_provider() -> Arc<AppState> {
    state_from(
        r#"
        [providers.mock1]
        type = "mock"
        canned_response = "hello from the mock"
        "#,
        Some("mock1"),
    )
}

async fn body_json(resp: axum::response::Response) -> serde_json::Value {
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    serde_json::from_slice(&bytes).unwrap()
}

fn post_json(path: &str, json: &str) -> Request<Body> {
    Request::post(path)
        .header("content-type", "application/json")
        .body(Body::from(json.to_string()))
        .unwrap()
}

#[tokio::test]
async fn healthz_is_ok() {
    let resp = router(single_provider())
        .oneshot(Request::get("/healthz").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(body_json(resp).await["status"], "ok");
}

#[tokio::test]
async fn lists_providers_with_default() {
    let resp = router(single_provider())
        .oneshot(Request::get("/v1/providers").body(Body::empty()).unwrap())
        .await
        .unwrap();
    let v = body_json(resp).await;
    let names: Vec<&str> = v["providers"]
        .as_array()
        .unwrap()
        .iter()
        .map(|x| x.as_str().unwrap())
        .collect();
    assert!(names.contains(&"mock1"), "got {names:?}"); // among builtins
    assert_eq!(v["default"], "mock1"); // set via --default-provider
}

#[tokio::test]
async fn complete_runs_through_the_pipeline() {
    // Single provider → `provider` can be omitted; `model` too (mock has
    // no default_model, but the request supplies one).
    let resp = router(single_provider())
        .oneshot(post_json("/v1/complete", r#"{"model":"m","user":"hi"}"#))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let v = body_json(resp).await;
    assert!(v["text"].is_string(), "got {v}");
    assert!(v["usage"].is_object());
}

#[tokio::test]
async fn complete_requires_user_or_messages() {
    let resp = router(single_provider())
        .oneshot(post_json("/v1/complete", r#"{"model":"m"}"#))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    assert_eq!(body_json(resp).await["error"]["kind"], "bad_request");
}

#[tokio::test]
async fn ambiguous_provider_is_a_400() {
    let state = state_from(
        r#"
        [providers.a]
        type = "mock"
        canned_response = "x"

        [providers.b]
        type = "mock"
        canned_response = "y"
        "#,
        None, // no default → request must name the provider
    );
    // Many providers (mocks + builtins), none specified → must error.
    let resp = router(state)
        .oneshot(post_json("/v1/complete", r#"{"model":"m","user":"hi"}"#))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    let v = body_json(resp).await;
    assert!(
        v["error"]["message"]
            .as_str()
            .unwrap()
            .contains("multiple providers"),
        "got {v}"
    );
}

#[tokio::test]
async fn unknown_provider_is_a_400() {
    let resp = router(single_provider())
        .oneshot(post_json(
            "/v1/complete",
            r#"{"provider":"nope","model":"m","user":"hi"}"#,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
}
