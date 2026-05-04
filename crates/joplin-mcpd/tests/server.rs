use axum::{
    body::{Body, to_bytes},
    http::{Request, StatusCode, header},
};
use joplin_mcpd::{
    auth::tokens::generate_raw_token,
    config::Config,
    contracts::MCP_PROTOCOL_VERSION,
    http::router,
    lifecycle::{Readiness, ReadinessStatus},
};
use serde_json::json;
use tower::ServiceExt;

#[tokio::test]
#[ignore = "requires explicit server-test command"]
async fn health_and_readiness_routes_are_exposed() {
    let app = router(Config::default(), Readiness::new(ReadinessStatus::Ready));

    let health = app
        .clone()
        .oneshot(
            Request::get("/healthz")
                .body(Body::empty())
                .expect("request"),
        )
        .await
        .expect("health response");
    let ready = app
        .oneshot(
            Request::get("/readyz")
                .body(Body::empty())
                .expect("request"),
        )
        .await
        .expect("ready response");

    assert_eq!(health.status(), StatusCode::OK);
    assert_eq!(ready.status(), StatusCode::OK);
}

#[tokio::test]
#[ignore = "requires explicit server-test command"]
async fn web_login_form_does_not_render_secret_values() {
    let app = router(Config::default(), Readiness::new(ReadinessStatus::Ready));

    let response = app
        .oneshot(Request::get("/login").body(Body::empty()).expect("request"))
        .await
        .expect("login response");

    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        response.headers().get(header::CONTENT_TYPE),
        Some(&axum::http::HeaderValue::from_static(
            "text/html; charset=utf-8"
        ))
    );

    let body = to_bytes(response.into_body(), 4096).await.expect("body");
    let body = std::str::from_utf8(body.as_ref()).expect("utf8 body");
    assert!(body.contains(r#"<form method="post" action="/login">"#));
    assert!(body.contains(r#"name="email""#));
    assert!(body.contains(r#"name="password""#));
    assert!(!body.contains("mcp_"));
    assert!(!body.contains(r#"value=""#));
}

#[tokio::test]
#[ignore = "requires explicit server-test command"]
async fn mcp_initialize_uses_current_protocol_version() {
    let app = router(Config::default(), Readiness::new(ReadinessStatus::Ready));
    let request = json!({
        "jsonrpc": "2.0",
        "id": "init-1",
        "method": "initialize",
        "params": {}
    });

    let response = app
        .oneshot(
            Request::post("/mcp")
                .header(header::CONTENT_TYPE, "application/json")
                .header(header::ORIGIN, "http://127.0.0.1:8081")
                .header(
                    header::AUTHORIZATION,
                    format!("Bearer {}", generate_raw_token()),
                )
                .body(Body::from(request.to_string()))
                .expect("request"),
        )
        .await
        .expect("mcp response");

    assert_eq!(response.status(), StatusCode::OK);
    let body = to_bytes(response.into_body(), 4096).await.expect("body");
    let body: serde_json::Value = serde_json::from_slice(&body).expect("json body");
    assert_eq!(body["result"]["protocolVersion"], MCP_PROTOCOL_VERSION);
}
