use crate::{
    auth::{
        joplin::{
            BootstrapAuthError, BootstrapAuthFailure, HttpJoplinAuthenticator, JoplinAuthenticator,
            resolve_joplin_user_by_id, upsert_mcp_user_by_joplin_user,
        },
        tokens::{HmacKey, TokenRecord, TokenRepository},
    },
    config::{BootstrapRateLimitConfig, Config},
    contracts::MCP_PROTOCOL_VERSION,
    db::audit::{AuditEventType, AuditOutcome, AuditRecord, write_audit_log},
    indexer::refresh::request_index_refresh_now,
    lifecycle::{Readiness, ReadinessStatus, RemoteIp, ShutdownDrain, extract_client_ip},
    mcp::transport::{McpAuth, mcp_delete, mcp_get, mcp_post},
    observability::{
        metrics,
        request_context::{RequestContext, TEST_ID_HEADER},
    },
    security::{endpoint_class, validate_origin_policy},
};
use axum::{
    BoxError, Form, Json, Router,
    body::{Body, Bytes},
    error_handling::HandleErrorLayer,
    extract::{ConnectInfo, Request as AxumRequest, State, rejection::FormRejection},
    http::{HeaderMap, StatusCode, header},
    middleware::{self, Next},
    response::{Html, IntoResponse, Response},
    routing::{get, post},
};
use chrono::{Duration as ChronoDuration, Utc};
use serde::{Deserialize, Serialize};
use serde_json::json;
use sqlx::PgPool;
use std::{
    collections::BTreeMap,
    future::Future,
    net::SocketAddr,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};
use tokio::net::TcpListener;
use tower::{ServiceBuilder, timeout::TimeoutLayer};
use tracing::Instrument;

#[derive(Debug, Clone)]
pub struct AppState {
    pub config: Config,
    pub readiness: Readiness,
    pub mcp_auth: McpAuth,
    pub api_backend: ApiBackend,
    bootstrap_rate_limiter: BootstrapRateLimiter,
}

#[derive(Debug, Clone)]
pub enum ApiBackend {
    NotConfigured,
    Repository {
        mcp_pool: PgPool,
        joplin_pool: PgPool,
        token_repository: TokenRepository,
        hmac_keys: Arc<Vec<HmacKey>>,
        active_hmac_key: HmacKey,
    },
}

pub fn router(config: Config, readiness: Readiness) -> Router {
    router_with_mcp_auth(config, readiness, McpAuth::SyntaxOnly)
}

pub fn router_with_mcp_auth(config: Config, readiness: Readiness, mcp_auth: McpAuth) -> Router {
    router_with_backends(config, readiness, mcp_auth, ApiBackend::NotConfigured)
}

pub fn router_with_backends(
    config: Config,
    readiness: Readiness,
    mcp_auth: McpAuth,
    api_backend: ApiBackend,
) -> Router {
    let state = AppState {
        config,
        readiness,
        mcp_auth,
        api_backend,
        bootstrap_rate_limiter: BootstrapRateLimiter::new(),
    };
    router_from_state(state)
}

fn router_from_state(state: AppState) -> Router {
    let request_timeout = Duration::from_secs(state.config.server.request_timeout_seconds);
    Router::new()
        .route("/healthz", get(healthz))
        .route("/readyz", get(readyz))
        .route("/api/bootstrap/login", post(bootstrap_login))
        .route("/api/token/check", post(token_check))
        .route("/api/token/revoke", post(token_revoke))
        .route("/api/index/status", get(index_status))
        .route("/login", get(web_login_form).post(web_login_submit))
        .route("/mcp", get(mcp_get).post(mcp_post).delete(mcp_delete))
        .layer(
            ServiceBuilder::new()
                .layer(HandleErrorLayer::new(|_error: BoxError| async move {
                    crate::lifecycle::timeout_response()
                }))
                .layer(TimeoutLayer::new(request_timeout)),
        )
        .layer(middleware::from_fn_with_state(
            state.clone(),
            enforce_origin_policy,
        ))
        .layer(middleware::from_fn_with_state(
            state.clone(),
            attach_remote_ip,
        ))
        .layer(middleware::from_fn(attach_request_context))
        .with_state(state)
}

pub async fn serve(config: Config, readiness: Readiness, mcp_auth: McpAuth) -> anyhow::Result<()> {
    serve_with_backends(config, readiness, mcp_auth, ApiBackend::NotConfigured).await
}

pub async fn serve_with_backends(
    config: Config,
    readiness: Readiness,
    mcp_auth: McpAuth,
    api_backend: ApiBackend,
) -> anyhow::Result<()> {
    let drain = ShutdownDrain::new(readiness.clone());
    let shutdown = async move {
        if let Err(error) = tokio::signal::ctrl_c().await {
            tracing::warn!(operation = "shutdown_signal", %error, "failed to listen for shutdown signal");
        }
        drain.begin().await;
    };
    serve_with_shutdown(config, readiness, mcp_auth, api_backend, shutdown).await
}

pub async fn serve_with_shutdown<F>(
    config: Config,
    readiness: Readiness,
    mcp_auth: McpAuth,
    api_backend: ApiBackend,
    shutdown_signal: F,
) -> anyhow::Result<()>
where
    F: Future<Output = ()> + Send + 'static,
{
    let listen = config.server.listen;
    let grace = Duration::from_secs(config.server.shutdown_grace_seconds);
    let app = router_with_backends(config, readiness, mcp_auth, api_backend);
    let listener = TcpListener::bind(listen).await?;
    tracing::info!(operation = "serve", %listen, "joplin-mcpd listening");
    axum::serve(
        listener,
        app.into_make_service_with_connect_info::<SocketAddr>(),
    )
    .with_graceful_shutdown(shutdown_signal)
    .await?;
    tracing::info!(
        operation = "shutdown",
        shutdown_grace_seconds = grace.as_secs(),
        "joplin-mcpd stopped accepting requests"
    );
    Ok(())
}

#[derive(Debug, Serialize)]
struct HealthResponse {
    status: &'static str,
    service: &'static str,
}

async fn healthz() -> Json<HealthResponse> {
    Json(HealthResponse {
        status: "ok",
        service: "joplin-mcpd",
    })
}

#[derive(Debug, Serialize)]
struct ReadyResponse {
    status: &'static str,
    mcp_protocol_version: &'static str,
}

async fn readyz(State(state): State<AppState>) -> Response {
    let readiness = state.readiness.status().await;
    let response = Json(ReadyResponse {
        status: readiness.as_str(),
        mcp_protocol_version: MCP_PROTOCOL_VERSION,
    });
    match readiness {
        ReadinessStatus::Ready => response.into_response(),
        ReadinessStatus::NotReady | ReadinessStatus::ShuttingDown => {
            (StatusCode::SERVICE_UNAVAILABLE, response).into_response()
        }
    }
}

#[derive(Debug, Deserialize)]
struct BootstrapLoginRequest {
    email: String,
    password: String,
    client_label: Option<String>,
}

#[derive(Debug, Serialize)]
struct BootstrapLoginResponse {
    token: String,
}

const LOGIN_FORM_HTML: &str = r#"<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<title>Joplin MCP Login</title>
</head>
<body>
<main>
<h1>Joplin MCP Login</h1>
<form method="post" action="/login">
<label>Email <input name="email" type="email" autocomplete="username" required></label>
<label>Password <input name="password" type="password" autocomplete="current-password" required></label>
<label>Client label <input name="client_label" type="text" autocomplete="off"></label>
<button type="submit">Create token</button>
</form>
</main>
</body>
</html>
"#;

fn render_login_success(token: &str) -> String {
    format!(
        r#"<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<title>Joplin MCP Token</title>
</head>
<body>
<main>
<h1>Joplin MCP Token</h1>
<textarea readonly rows="3" cols="80">{}</textarea>
</main>
</body>
</html>
"#,
        html_escape(token)
    )
}

fn html_escape(value: &str) -> String {
    let mut escaped = String::with_capacity(value.len());
    for character in value.chars() {
        match character {
            '&' => escaped.push_str("&amp;"),
            '<' => escaped.push_str("&lt;"),
            '>' => escaped.push_str("&gt;"),
            '"' => escaped.push_str("&quot;"),
            '\'' => escaped.push_str("&#39;"),
            _ => escaped.push(character),
        }
    }
    escaped
}

#[derive(Debug, Serialize)]
struct TokenCheckResponse {
    valid: bool,
    user_id: String,
    token_id: String,
    label: String,
    scope: String,
    expires_at: Option<chrono::DateTime<Utc>>,
}

#[derive(Debug, Serialize)]
struct TokenRevokeResponse {
    revoked: bool,
}

#[derive(Debug, Serialize)]
struct IndexStatusResponse {
    status: String,
    last_full_rebuild_at: Option<chrono::DateTime<Utc>>,
    last_incremental_at: Option<chrono::DateTime<Utc>>,
    last_checked_at: Option<chrono::DateTime<Utc>>,
    updated_at: Option<chrono::DateTime<Utc>>,
}

#[derive(Debug, sqlx::FromRow)]
struct IndexStatusRow {
    status: String,
    last_full_rebuild_at: Option<chrono::DateTime<Utc>>,
    last_incremental_at: Option<chrono::DateTime<Utc>>,
    last_checked_at: Option<chrono::DateTime<Utc>>,
    updated_at: Option<chrono::DateTime<Utc>>,
}

const BOOTSTRAP_RATE_LIMIT_MAX_IP_KEYS: usize = 4096;
const BOOTSTRAP_RATE_LIMIT_MAX_EMAIL_KEYS: usize = 16_384;

#[derive(Debug, Clone, Default)]
struct BootstrapRateLimiter {
    inner: Arc<Mutex<BootstrapRateLimitState>>,
}

#[derive(Debug, Default)]
struct BootstrapRateLimitState {
    per_ip: BTreeMap<String, WindowCounter>,
    per_email: BTreeMap<String, WindowCounter>,
}

#[derive(Debug)]
struct WindowCounter {
    window_start: Instant,
    count: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BootstrapRateLimitScope {
    Ip,
    Email,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct BootstrapRateLimitRejection {
    scope: BootstrapRateLimitScope,
}

impl BootstrapRateLimiter {
    fn new() -> Self {
        Self::default()
    }

    fn check(
        &self,
        remote_ip: Option<&str>,
        email: &str,
        config: &BootstrapRateLimitConfig,
    ) -> Result<(), BootstrapRateLimitRejection> {
        let now = Instant::now();
        let ip_key = remote_ip.unwrap_or("unknown").to_string();
        let email_key = email.trim().to_ascii_lowercase();
        let mut state = self.inner.lock().expect("bootstrap rate limiter lock");

        if !allow_ip_window(&mut state, ip_key, now, config) {
            return Err(BootstrapRateLimitRejection {
                scope: BootstrapRateLimitScope::Ip,
            });
        }

        if !allow_windowed_request(
            &mut state.per_email,
            email_key,
            now,
            Duration::from_secs(60 * 60),
            config.per_email_per_hour,
            BOOTSTRAP_RATE_LIMIT_MAX_EMAIL_KEYS,
        ) {
            return Err(BootstrapRateLimitRejection {
                scope: BootstrapRateLimitScope::Email,
            });
        }

        Ok(())
    }

    fn check_ip(
        &self,
        remote_ip: Option<&str>,
        config: &BootstrapRateLimitConfig,
    ) -> Result<(), BootstrapRateLimitRejection> {
        let now = Instant::now();
        let ip_key = remote_ip.unwrap_or("unknown").to_string();
        let mut state = self.inner.lock().expect("bootstrap rate limiter lock");

        if allow_ip_window(&mut state, ip_key, now, config) {
            Ok(())
        } else {
            Err(BootstrapRateLimitRejection {
                scope: BootstrapRateLimitScope::Ip,
            })
        }
    }

    #[cfg(test)]
    fn ip_count(&self, remote_ip: Option<&str>) -> Option<u32> {
        let state = self.inner.lock().expect("bootstrap rate limiter lock");
        state
            .per_ip
            .get(remote_ip.unwrap_or("unknown"))
            .map(|counter| counter.count)
    }
}

fn allow_ip_window(
    state: &mut BootstrapRateLimitState,
    ip_key: String,
    now: Instant,
    config: &BootstrapRateLimitConfig,
) -> bool {
    allow_windowed_request(
        &mut state.per_ip,
        ip_key,
        now,
        Duration::from_secs(60),
        config.per_ip_per_minute,
        BOOTSTRAP_RATE_LIMIT_MAX_IP_KEYS,
    )
}

fn allow_windowed_request(
    counters: &mut BTreeMap<String, WindowCounter>,
    key: String,
    now: Instant,
    window: Duration,
    limit: u32,
    max_keys: usize,
) -> bool {
    if limit == 0 {
        return false;
    }

    counters.retain(|_, counter| now.duration_since(counter.window_start) < window);
    if !counters.contains_key(&key) && counters.len() >= max_keys {
        return false;
    }

    let counter = counters.entry(key).or_insert(WindowCounter {
        window_start: now,
        count: 0,
    });
    if now.duration_since(counter.window_start) >= window {
        counter.window_start = now;
        counter.count = 0;
    }
    if counter.count >= limit {
        return false;
    }
    counter.count += 1;
    true
}

async fn bootstrap_login(
    State(state): State<AppState>,
    remote_ip: Option<axum::Extension<RemoteIp>>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let remote_ip = remote_ip.map(|axum::Extension(ip)| ip.0.to_string());
    if !has_json_content_type(&headers) {
        return malformed_bootstrap_response(&state, remote_ip).await;
    }
    let request = match serde_json::from_slice::<BootstrapLoginRequest>(&body) {
        Ok(request) => request,
        Err(_) => return malformed_bootstrap_response(&state, remote_ip).await,
    };
    match mint_bootstrap_token(&state, request, remote_ip)
        .instrument(tracing::info_span!("bootstrap.login"))
        .await
    {
        Ok(response) => Json(response).into_response(),
        Err(response) => response,
    }
}

async fn web_login_form() -> Html<&'static str> {
    Html(LOGIN_FORM_HTML)
}

async fn web_login_submit(
    State(state): State<AppState>,
    remote_ip: Option<axum::Extension<RemoteIp>>,
    form: Result<Form<BootstrapLoginRequest>, FormRejection>,
) -> Response {
    let remote_ip = remote_ip.map(|axum::Extension(ip)| ip.0.to_string());
    let Ok(Form(request)) = form else {
        return malformed_bootstrap_response(&state, remote_ip).await;
    };
    match mint_bootstrap_token(&state, request, remote_ip)
        .instrument(tracing::info_span!("bootstrap.login"))
        .await
    {
        Ok(response) => Html(render_login_success(&response.token)).into_response(),
        Err(response) => response,
    }
}

fn has_json_content_type(headers: &HeaderMap) -> bool {
    headers
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .map(|value| {
            let media_type = value
                .split(';')
                .next()
                .unwrap_or_default()
                .trim()
                .to_ascii_lowercase();
            media_type == "application/json" || media_type.ends_with("+json")
        })
        .unwrap_or(false)
}

async fn malformed_bootstrap_response(state: &AppState, remote_ip: Option<String>) -> Response {
    if let Err(rejection) = state
        .bootstrap_rate_limiter
        .check_ip(remote_ip.as_deref(), &state.config.bootstrap_rate_limit)
    {
        metrics::record_bootstrap_login("rate_limited");
        if let ApiBackend::Repository { mcp_pool, .. } = &state.api_backend {
            audit_bootstrap_rate_limit(mcp_pool, "joplin-mcp-client", remote_ip.clone(), rejection)
                .await;
        }
        return bootstrap_error_response(BootstrapAuthError::new(
            BootstrapAuthFailure::RateLimited,
        ));
    }

    bootstrap_error_response(BootstrapAuthError::new(
        BootstrapAuthFailure::JoplinRejected,
    ))
}

async fn mint_bootstrap_token(
    state: &AppState,
    request: BootstrapLoginRequest,
    remote_ip: Option<String>,
) -> Result<BootstrapLoginResponse, Response> {
    let ApiBackend::Repository {
        mcp_pool,
        joplin_pool,
        token_repository,
        active_hmac_key,
        ..
    } = &state.api_backend
    else {
        return Err(StatusCode::NOT_IMPLEMENTED.into_response());
    };

    let client_label = request
        .client_label
        .as_deref()
        .map(str::trim)
        .filter(|label| !label.is_empty())
        .unwrap_or("joplin-mcp-client");
    if let Err(rejection) = state.bootstrap_rate_limiter.check(
        remote_ip.as_deref(),
        &request.email,
        &state.config.bootstrap_rate_limit,
    ) {
        metrics::record_bootstrap_login("rate_limited");
        audit_bootstrap_rate_limit(mcp_pool, client_label, remote_ip.clone(), rejection).await;
        return Err(bootstrap_error_response(BootstrapAuthError::new(
            BootstrapAuthFailure::RateLimited,
        )));
    }
    let authenticator = HttpJoplinAuthenticator::new(state.config.joplin.base_url.clone());
    let session = match async {
        authenticator
            .authenticate(&request.email, &request.password)
            .await
    }
    .instrument(tracing::info_span!("bootstrap.joplin_auth"))
    .await
    {
        Ok(session) => session,
        Err(error) => {
            metrics::record_bootstrap_login("failed");
            audit_bootstrap_failure(mcp_pool, None, client_label, remote_ip.clone(), &error).await;
            return Err(bootstrap_error_response(error));
        }
    };
    let joplin_user = match async { resolve_joplin_user_by_id(joplin_pool, &session.user_id).await }
        .instrument(tracing::info_span!("bootstrap.user_resolve"))
        .await
    {
        Ok(user) => user,
        Err(error) => {
            let _ = authenticator.invalidate_session(&session.id).await;
            metrics::record_bootstrap_login("failed");
            audit_bootstrap_failure(mcp_pool, None, client_label, remote_ip.clone(), &error).await;
            return Err(bootstrap_error_response(error));
        }
    };
    let mcp_user = match async { upsert_mcp_user_by_joplin_user(mcp_pool, &joplin_user).await }
        .instrument(tracing::info_span!("bootstrap.user_upsert"))
        .await
    {
        Ok(user) => user,
        Err(error) => {
            let _ = authenticator.invalidate_session(&session.id).await;
            metrics::record_bootstrap_login("failed");
            audit_bootstrap_failure(mcp_pool, None, client_label, remote_ip.clone(), &error).await;
            return Err(bootstrap_error_response(error));
        }
    };
    if let Err(error) = authenticator.invalidate_session(&session.id).await {
        metrics::record_bootstrap_login("failed");
        audit_bootstrap_failure(
            mcp_pool,
            Some(mcp_user.id),
            client_label,
            remote_ip.clone(),
            &error,
        )
        .await;
        return Err(bootstrap_error_response(error));
    }

    let expires_at = if state.config.tokens.allow_non_expiring_tokens
        && state.config.tokens.expires_after_days == 0
    {
        None
    } else {
        Some(Utc::now() + ChronoDuration::days(state.config.tokens.expires_after_days as i64))
    };
    let token = match async {
        token_repository
            .create_token(
                mcp_user.id,
                active_hmac_key,
                client_label,
                &state.config.tokens.default_scope,
                expires_at,
                remote_ip.clone(),
            )
            .await
    }
    .instrument(tracing::info_span!("bootstrap.token_mint"))
    .await
    {
        Ok(token) => token,
        Err(error) => {
            metrics::record_bootstrap_login("failed");
            audit_or_warn(
                mcp_pool,
                AuditRecord::new(AuditEventType::TokenMint, AuditOutcome::Failed)
                    .user_id(mcp_user.id)
                    .client_label(client_label)
                    .remote_ip(remote_ip.clone())
                    .metadata(json!({ "error_kind": "token_insert_failed" })),
            )
            .await;
            audit_or_warn(
                mcp_pool,
                AuditRecord::new(AuditEventType::BootstrapLogin, AuditOutcome::Failed)
                    .user_id(mcp_user.id)
                    .client_label(client_label)
                    .remote_ip(remote_ip.clone())
                    .metadata(json!({ "failure": "token_insert_failed" })),
            )
            .await;
            tracing::warn!(
                operation = "bootstrap_login",
                outcome = "failed",
                error = %error,
                "failed to mint MCP token"
            );
            return Err(StatusCode::INTERNAL_SERVER_ERROR.into_response());
        }
    };

    if let Err(error) = request_index_refresh_now(mcp_pool, mcp_user.id)
        .instrument(tracing::info_span!("bootstrap.index_wake_up"))
        .await
    {
        tracing::warn!(
            operation = "bootstrap_login",
            user_id = %mcp_user.id,
            %error,
            "failed to request immediate index refresh after bootstrap"
        );
    }

    audit_or_warn(
        mcp_pool,
        AuditRecord::new(AuditEventType::TokenMint, AuditOutcome::Success)
            .user_id(mcp_user.id)
            .client_label(client_label)
            .remote_ip(remote_ip.clone())
            .metadata(json!({
                "token_id": token.insert.id,
                "scope": token.insert.scope,
                "expires_at": token.insert.expires_at,
            })),
    )
    .await;
    audit_or_warn(
        mcp_pool,
        AuditRecord::new(AuditEventType::BootstrapLogin, AuditOutcome::Success)
            .user_id(mcp_user.id)
            .client_label(client_label)
            .remote_ip(remote_ip.clone())
            .metadata(json!({ "joplin_user_resolved": true })),
    )
    .await;
    tracing::info!(
        operation = "bootstrap_login",
        outcome = "completed",
        client.label = %client_label,
        "bootstrap login completed"
    );
    metrics::record_bootstrap_login("success");
    Ok(BootstrapLoginResponse {
        token: token.raw_token,
    })
}

async fn token_check(State(state): State<AppState>, headers: HeaderMap) -> Response {
    let Ok(token) = authenticate_api_token(&state, &headers).await else {
        return StatusCode::UNAUTHORIZED.into_response();
    };
    Json(TokenCheckResponse {
        valid: true,
        user_id: token.user_id.to_string(),
        token_id: token.id.to_string(),
        label: token.label,
        scope: token.scope,
        expires_at: token.expires_at,
    })
    .into_response()
}

async fn token_revoke(
    State(state): State<AppState>,
    remote_ip: Option<axum::Extension<RemoteIp>>,
    headers: HeaderMap,
) -> Response {
    let ApiBackend::Repository {
        token_repository, ..
    } = &state.api_backend
    else {
        return StatusCode::NOT_IMPLEMENTED.into_response();
    };
    let Ok(token) = authenticate_api_token(&state, &headers).await else {
        return StatusCode::UNAUTHORIZED.into_response();
    };
    let revoked_from_ip = remote_ip.map(|axum::Extension(ip)| ip.0.to_string());
    match token_repository
        .revoke_token(
            token.id,
            Some(token.user_id),
            Some("client_logout"),
            revoked_from_ip.as_deref(),
        )
        .await
    {
        Ok(revoked) => {
            audit_or_warn(
                token_repository.pool(),
                AuditRecord::new(
                    AuditEventType::TokenRevoke,
                    if revoked {
                        AuditOutcome::Success
                    } else {
                        AuditOutcome::Noop
                    },
                )
                .user_id(token.user_id)
                .client_label(token.label)
                .remote_ip(revoked_from_ip.clone())
                .metadata(json!({
                    "token_id": token.id,
                    "reason": "client_logout",
                    "revoked": revoked,
                })),
            )
            .await;
            Json(TokenRevokeResponse { revoked }).into_response()
        }
        Err(error) => {
            audit_or_warn(
                token_repository.pool(),
                AuditRecord::new(AuditEventType::TokenRevoke, AuditOutcome::Failed)
                    .user_id(token.user_id)
                    .client_label(token.label)
                    .remote_ip(revoked_from_ip.clone())
                    .metadata(json!({
                        "token_id": token.id,
                        "error_kind": "token_revoke_failed",
                    })),
            )
            .await;
            tracing::warn!(
                operation = "token_revoke",
                outcome = "failed",
                error = %error,
                "failed to revoke token"
            );
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}

async fn audit_bootstrap_failure(
    pool: &PgPool,
    user_id: Option<uuid::Uuid>,
    client_label: &str,
    remote_ip: Option<String>,
    error: &BootstrapAuthError,
) {
    let mut record = AuditRecord::new(AuditEventType::BootstrapLogin, AuditOutcome::Failed)
        .client_label(client_label)
        .remote_ip(remote_ip)
        .metadata(json!({ "failure": error.sanitized_detail() }));
    if let Some(user_id) = user_id {
        record = record.user_id(user_id);
    }
    audit_or_warn(pool, record).await;
}

async fn audit_bootstrap_rate_limit(
    pool: &PgPool,
    client_label: &str,
    remote_ip: Option<String>,
    rejection: BootstrapRateLimitRejection,
) {
    let limit_scope = match rejection.scope {
        BootstrapRateLimitScope::Ip => "ip",
        BootstrapRateLimitScope::Email => "email",
    };
    audit_or_warn(
        pool,
        AuditRecord::new(AuditEventType::BootstrapLogin, AuditOutcome::Failed)
            .client_label(client_label)
            .remote_ip(remote_ip)
            .metadata(json!({
                "failure": "rate_limited",
                "limit_scope": limit_scope,
            })),
    )
    .await;
}

async fn audit_or_warn(pool: &PgPool, record: AuditRecord) {
    if let Err(error) = write_audit_log(pool, record).await {
        tracing::warn!(
            operation = "audit_log_write",
            outcome = "failed",
            error = %error,
            "failed to write audit log"
        );
    }
}

async fn index_status(State(state): State<AppState>, headers: HeaderMap) -> Response {
    let ApiBackend::Repository { mcp_pool, .. } = &state.api_backend else {
        return StatusCode::NOT_IMPLEMENTED.into_response();
    };
    let Ok(token) = authenticate_api_token(&state, &headers).await else {
        return StatusCode::UNAUTHORIZED.into_response();
    };
    let row = match sqlx::query_as::<_, IndexStatusRow>(
        r#"
        SELECT status, last_full_rebuild_at, last_incremental_at, last_checked_at, updated_at
        FROM joplin_mcp.index_state
        WHERE user_id = $1
        "#,
    )
    .bind(token.user_id)
    .fetch_optional(mcp_pool)
    .await
    {
        Ok(row) => row,
        Err(error) => {
            tracing::warn!(
                operation = "index_status",
                outcome = "failed",
                error = %error,
                "failed to read index status"
            );
            return StatusCode::INTERNAL_SERVER_ERROR.into_response();
        }
    };

    let response = row.map_or(
        IndexStatusResponse {
            status: "missing".to_string(),
            last_full_rebuild_at: None,
            last_incremental_at: None,
            last_checked_at: None,
            updated_at: None,
        },
        |row| IndexStatusResponse {
            status: row.status,
            last_full_rebuild_at: row.last_full_rebuild_at,
            last_incremental_at: row.last_incremental_at,
            last_checked_at: row.last_checked_at,
            updated_at: row.updated_at,
        },
    );
    Json(response).into_response()
}

async fn authenticate_api_token(
    state: &AppState,
    headers: &HeaderMap,
) -> Result<TokenRecord, StatusCode> {
    let ApiBackend::Repository {
        token_repository,
        hmac_keys,
        ..
    } = &state.api_backend
    else {
        return Err(StatusCode::NOT_IMPLEMENTED);
    };
    let authorization = headers
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok());
    token_repository
        .authenticate_bearer(authorization, hmac_keys, Utc::now())
        .await
        .map_err(|_| StatusCode::UNAUTHORIZED)
}

fn bootstrap_error_response(error: BootstrapAuthError) -> Response {
    let status = match error.failure() {
        BootstrapAuthFailure::JoplinUnavailable => StatusCode::SERVICE_UNAVAILABLE,
        BootstrapAuthFailure::JoplinRejected
        | BootstrapAuthFailure::InvalidJoplinResponse
        | BootstrapAuthFailure::JoplinUserNotFound
        | BootstrapAuthFailure::RateLimited => StatusCode::UNAUTHORIZED,
        BootstrapAuthFailure::McpUserUpsertFailed
        | BootstrapAuthFailure::SessionInvalidationFailed => StatusCode::INTERNAL_SERVER_ERROR,
    };
    tracing::warn!(
        operation = "bootstrap_login",
        outcome = "failed",
        error.kind = error.sanitized_detail(),
        "bootstrap login failed"
    );
    (status, error.client_message()).into_response()
}

pub fn is_loopback_address(addr: SocketAddr) -> bool {
    addr.ip().is_loopback()
}

async fn attach_remote_ip(
    State(state): State<AppState>,
    mut request: AxumRequest<Body>,
    next: Next,
) -> Response {
    let peer = request
        .extensions()
        .get::<ConnectInfo<SocketAddr>>()
        .copied()
        .map(|ConnectInfo(peer)| peer)
        .unwrap_or_else(|| SocketAddr::from(([127, 0, 0, 1], 0)));
    match extract_client_ip(peer, request.headers(), &state.config.server) {
        Ok(ip) => {
            request.extensions_mut().insert(RemoteIp(ip));
            next.run(request).await
        }
        Err(status) => status.into_response(),
    }
}

async fn attach_request_context(mut request: AxumRequest<Body>, next: Next) -> Response {
    let test_id = request
        .headers()
        .get(TEST_ID_HEADER)
        .and_then(|value| value.to_str().ok())
        .map(str::to_string);
    let context = RequestContext::new(test_id);
    let request_id = context.request_id;
    let test_id = context.test_id.clone();
    let method = request.method().clone();
    let path = request.uri().path().to_string();
    request.extensions_mut().insert(context);

    tracing::info!(
        request.id = %request_id,
        test.id = test_id.as_deref().unwrap_or(""),
        http.request.method = %method,
        url.path = %path,
        operation = "http_request",
        outcome = "started",
        "request started"
    );

    let response = next.run(request).await;
    tracing::info!(
        request.id = %request_id,
        test.id = test_id.as_deref().unwrap_or(""),
        http.request.method = %method,
        url.path = %path,
        http.response.status_code = response.status().as_u16(),
        operation = "http_request",
        outcome = "completed",
        "request completed"
    );
    response
}

async fn enforce_origin_policy(
    State(state): State<AppState>,
    request: AxumRequest<Body>,
    next: Next,
) -> Response {
    let Some(class) = endpoint_class(request.method(), request.uri().path()) else {
        return next.run(request).await;
    };

    match validate_origin_policy(
        class,
        request.headers(),
        &state.config.server.allowed_origins,
    ) {
        Ok(()) => next.run(request).await,
        Err(error) => (error.status(), error.to_string()).into_response(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::tokens::generate_raw_token;
    use axum::{
        body::{Body, to_bytes},
        http::{Request, header},
    };
    use std::time::Duration;
    use tokio::time::sleep;
    use tower::ServiceExt;

    #[tokio::test]
    async fn healthz_returns_success() {
        let app = router(Config::default(), Readiness::new(ReadinessStatus::Ready));

        let response = app
            .oneshot(
                Request::get("/healthz")
                    .body(Body::empty())
                    .expect("request"),
            )
            .await
            .expect("response");

        assert_eq!(response.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn readyz_reports_ready() {
        let app = router(Config::default(), Readiness::new(ReadinessStatus::Ready));

        let response = app
            .oneshot(
                Request::get("/readyz")
                    .body(Body::empty())
                    .expect("request"),
            )
            .await
            .expect("response");

        assert_eq!(response.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn readyz_reports_not_ready_with_503() {
        let app = router(Config::default(), Readiness::new(ReadinessStatus::NotReady));

        let response = app
            .oneshot(
                Request::get("/readyz")
                    .body(Body::empty())
                    .expect("request"),
            )
            .await
            .expect("response");

        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    }

    #[tokio::test]
    async fn required_non_mcp_routes_exist_as_stubs() {
        let app = router(Config::default(), Readiness::new(ReadinessStatus::Ready));

        for path in [
            "/api/bootstrap/login",
            "/api/token/check",
            "/api/token/revoke",
            "/api/index/status",
            "/login",
        ] {
            let response = app
                .clone()
                .oneshot(Request::get(path).body(Body::empty()).expect("request"))
                .await
                .expect("response");
            assert_ne!(response.status(), StatusCode::NOT_FOUND, "route {path}");
        }
    }

    #[tokio::test]
    async fn login_get_renders_minimal_form_without_secret_values() {
        let app = router(Config::default(), Readiness::new(ReadinessStatus::Ready));

        let response = app
            .oneshot(Request::get("/login").body(Body::empty()).expect("request"))
            .await
            .expect("response");

        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response.headers().get(header::CONTENT_TYPE),
            Some(&axum::http::HeaderValue::from_static(
                "text/html; charset=utf-8"
            ))
        );
        let body = to_bytes(response.into_body(), 4096).await.expect("body");
        let body = std::str::from_utf8(body.as_ref()).expect("utf8");
        assert!(body.contains(r#"<form method="post" action="/login">"#));
        assert!(body.contains(r#"name="email""#));
        assert!(body.contains(r#"name="password""#));
        assert!(body.contains(r#"name="client_label""#));
        assert!(!body.contains("mcp_"));
        assert!(!body.contains(r#"value=""#));
    }

    #[tokio::test]
    async fn login_post_bad_form_uses_uniform_bootstrap_error() {
        let app = router(Config::default(), Readiness::new(ReadinessStatus::Ready));

        let response = app
            .oneshot(
                Request::post("/login")
                    .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                    .body(Body::from("email=user%40example.test"))
                    .expect("request"),
            )
            .await
            .expect("response");

        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        let body = to_bytes(response.into_body(), 1024).await.expect("body");
        assert_eq!(
            body.as_ref(),
            crate::auth::joplin::BOOTSTRAP_AUTH_FAILED_MESSAGE.as_bytes()
        );
    }

    #[tokio::test]
    async fn malformed_form_bootstrap_attempts_are_rate_limited_by_ip() {
        let state = test_state(bootstrap_rate_limit_config(1, 10));
        let app = router_from_state(state.clone());

        for _ in 0..2 {
            let response = app
                .clone()
                .oneshot(
                    Request::post("/login")
                        .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                        .body(Body::from("email=user%40example.test"))
                        .expect("request"),
                )
                .await
                .expect("response");

            assert_uniform_bootstrap_error(response).await;
        }

        assert_eq!(
            state.bootstrap_rate_limiter.ip_count(Some("127.0.0.1")),
            Some(1)
        );
        let rejection = state
            .bootstrap_rate_limiter
            .check_ip(Some("127.0.0.1"), &state.config.bootstrap_rate_limit)
            .expect_err("same IP remains rate limited after malformed form attempts");
        assert_eq!(rejection.scope, BootstrapRateLimitScope::Ip);
        assert!(
            state
                .bootstrap_rate_limiter
                .check_ip(Some("127.0.0.2"), &state.config.bootstrap_rate_limit)
                .is_ok()
        );
    }

    #[tokio::test]
    async fn malformed_json_bootstrap_attempts_are_rate_limited_by_ip() {
        let state = test_state(bootstrap_rate_limit_config(1, 10));
        let app = router_from_state(state.clone());

        for _ in 0..2 {
            let response = app
                .clone()
                .oneshot(
                    Request::post("/api/bootstrap/login")
                        .header(header::CONTENT_TYPE, "application/json")
                        .header(header::ORIGIN, "http://127.0.0.1:8081")
                        .body(Body::from("{"))
                        .expect("request"),
                )
                .await
                .expect("response");

            assert_uniform_bootstrap_error(response).await;
        }

        assert_eq!(
            state.bootstrap_rate_limiter.ip_count(Some("127.0.0.1")),
            Some(1)
        );
        let rejection = state
            .bootstrap_rate_limiter
            .check_ip(Some("127.0.0.1"), &state.config.bootstrap_rate_limit)
            .expect_err("same IP remains rate limited after malformed JSON attempts");
        assert_eq!(rejection.scope, BootstrapRateLimitScope::Ip);
        assert!(
            state
                .bootstrap_rate_limiter
                .check_ip(Some("127.0.0.2"), &state.config.bootstrap_rate_limit)
                .is_ok()
        );
    }

    #[tokio::test]
    async fn login_post_rejects_disallowed_origin_and_referer() {
        let app = router(Config::default(), Readiness::new(ReadinessStatus::Ready));

        let origin_response = app
            .clone()
            .oneshot(
                Request::post("/login")
                    .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                    .header("origin", "https://evil.test")
                    .body(Body::from(
                        "email=user%40example.test&password=secret&client_label=browser",
                    ))
                    .expect("request"),
            )
            .await
            .expect("response");
        let referer_response = app
            .oneshot(
                Request::post("/login")
                    .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                    .header("referer", "https://evil.test/login")
                    .body(Body::from(
                        "email=user%40example.test&password=secret&client_label=browser",
                    ))
                    .expect("request"),
            )
            .await
            .expect("response");

        assert_eq!(origin_response.status(), StatusCode::FORBIDDEN);
        assert_eq!(referer_response.status(), StatusCode::FORBIDDEN);
    }

    #[test]
    fn login_success_html_escapes_visible_token() {
        let body = render_login_success(r#"mcp_<token>&"'"#);

        assert!(body.contains("mcp_&lt;token&gt;&amp;&quot;&#39;"));
        assert!(!body.contains(r#"mcp_<token>&"'"#));
    }

    #[tokio::test]
    async fn request_timeout_layer_returns_timeout_status() {
        async fn slow() -> &'static str {
            sleep(Duration::from_millis(50)).await;
            "done"
        }

        let app = Router::new().route("/slow", get(slow)).layer(
            ServiceBuilder::new()
                .layer(HandleErrorLayer::new(|_error: BoxError| async move {
                    crate::lifecycle::timeout_response()
                }))
                .layer(TimeoutLayer::new(Duration::from_millis(1))),
        );

        let response = app
            .oneshot(Request::get("/slow").body(Body::empty()).expect("request"))
            .await
            .expect("response");

        assert_eq!(response.status(), StatusCode::REQUEST_TIMEOUT);
    }

    #[tokio::test]
    async fn new_mcp_post_is_rejected_after_shutdown_begins() {
        let readiness = Readiness::new(ReadinessStatus::Ready);
        let app = router(Config::default(), readiness.clone());
        readiness.set(ReadinessStatus::ShuttingDown).await;

        let response = app
            .oneshot(
                Request::post("/mcp")
                    .header("authorization", format!("Bearer {}", generate_raw_token()))
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{"jsonrpc":"2.0","id":1,"method":"ping"}"#))
                    .expect("request"),
            )
            .await
            .expect("response");

        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    }

    #[tokio::test]
    async fn malformed_forwarded_header_is_rejected_by_middleware() {
        let config = Config {
            server: crate::config::ServerConfig {
                trusted_proxies: vec!["127.0.0.1/32".parse().expect("net")],
                ..crate::config::ServerConfig::default()
            },
            ..Config::default()
        };
        let app = router(config, Readiness::new(ReadinessStatus::Ready));

        let response = app
            .oneshot(
                Request::get("/healthz")
                    .header("x-forwarded-for", "not-an-ip")
                    .body(Body::empty())
                    .expect("request"),
            )
            .await
            .expect("response");

        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn mcp_rejects_unexpected_origin_before_handler() {
        let app = router(Config::default(), Readiness::new(ReadinessStatus::Ready));

        let response = app
            .oneshot(
                Request::post("/mcp")
                    .header("authorization", format!("Bearer {}", generate_raw_token()))
                    .header("content-type", "application/json")
                    .header("origin", "https://evil.test")
                    .body(Body::from(r#"{"jsonrpc":"2.0","id":1,"method":"ping"}"#))
                    .expect("request"),
            )
            .await
            .expect("response");

        assert_eq!(response.status(), StatusCode::FORBIDDEN);
    }

    #[test]
    fn bootstrap_login_requests_index_wake_up_after_token_mint() {
        let source = include_str!("http.rs");
        let token_mint = source
            .find("tracing::info_span!(\"bootstrap.token_mint\")")
            .expect("token mint span exists");
        let wake_up = source
            .find("request_index_refresh_now(mcp_pool, mcp_user.id)")
            .expect("index wake-up request exists");
        let success_metric = source
            .find("metrics::record_bootstrap_login(\"success\")")
            .expect("success metric exists");

        assert!(token_mint < wake_up);
        assert!(wake_up < success_metric);
    }

    #[test]
    fn bootstrap_rate_limiter_rejects_per_ip_before_window_reset() {
        let limiter = BootstrapRateLimiter::new();
        let config = BootstrapRateLimitConfig {
            per_ip_per_minute: 2,
            per_email_per_hour: 10,
        };

        assert!(
            limiter
                .check(Some("192.0.2.10"), "one@example.test", &config)
                .is_ok()
        );
        assert!(
            limiter
                .check(Some("192.0.2.10"), "two@example.test", &config)
                .is_ok()
        );

        let rejection = limiter
            .check(Some("192.0.2.10"), "three@example.test", &config)
            .expect_err("third request from same IP is rejected");
        assert_eq!(rejection.scope, BootstrapRateLimitScope::Ip);

        assert!(
            limiter
                .check(Some("192.0.2.11"), "three@example.test", &config)
                .is_ok()
        );
    }

    #[test]
    fn bootstrap_rate_limiter_rejects_per_email_independently() {
        let limiter = BootstrapRateLimiter::new();
        let config = BootstrapRateLimitConfig {
            per_ip_per_minute: 10,
            per_email_per_hour: 2,
        };

        assert!(
            limiter
                .check(Some("192.0.2.20"), "User@Example.Test", &config)
                .is_ok()
        );
        assert!(
            limiter
                .check(Some("192.0.2.21"), " user@example.test ", &config)
                .is_ok()
        );

        let rejection = limiter
            .check(Some("192.0.2.22"), "USER@example.test", &config)
            .expect_err("third request for same normalized email is rejected");
        assert_eq!(rejection.scope, BootstrapRateLimitScope::Email);

        assert!(
            limiter
                .check(Some("192.0.2.22"), "other@example.test", &config)
                .is_ok()
        );
    }

    #[test]
    fn bootstrap_rate_limiter_prunes_expired_ip_and_email_keys() {
        let limiter = BootstrapRateLimiter::new();
        let config = BootstrapRateLimitConfig {
            per_ip_per_minute: 10,
            per_email_per_hour: 10,
        };
        let stale_start = Instant::now() - Duration::from_secs(2 * 60 * 60);
        {
            let mut state = limiter.inner.lock().expect("limiter state");
            state.per_ip.insert(
                "192.0.2.100".to_string(),
                WindowCounter {
                    window_start: stale_start,
                    count: 1,
                },
            );
            state.per_email.insert(
                "stale@example.test".to_string(),
                WindowCounter {
                    window_start: stale_start,
                    count: 1,
                },
            );
        }

        assert!(
            limiter
                .check(Some("192.0.2.101"), "fresh@example.test", &config)
                .is_ok()
        );

        let state = limiter.inner.lock().expect("limiter state");
        assert!(!state.per_ip.contains_key("192.0.2.100"));
        assert!(!state.per_email.contains_key("stale@example.test"));
        assert!(state.per_ip.contains_key("192.0.2.101"));
        assert!(state.per_email.contains_key("fresh@example.test"));
    }

    #[test]
    fn bootstrap_rate_limiter_rejects_new_keys_when_active_window_is_full() {
        let now = Instant::now();
        let mut counters = BTreeMap::new();
        counters.insert(
            "one".to_string(),
            WindowCounter {
                window_start: now,
                count: 1,
            },
        );
        counters.insert(
            "two".to_string(),
            WindowCounter {
                window_start: now,
                count: 1,
            },
        );

        assert!(!allow_windowed_request(
            &mut counters,
            "three".to_string(),
            now,
            Duration::from_secs(60),
            10,
            2,
        ));
        assert_eq!(counters.len(), 2);
        assert!(allow_windowed_request(
            &mut counters,
            "one".to_string(),
            now,
            Duration::from_secs(60),
            10,
            2,
        ));
    }

    #[tokio::test]
    async fn bootstrap_rate_limited_error_is_uniform() {
        let response =
            bootstrap_error_response(BootstrapAuthError::new(BootstrapAuthFailure::RateLimited));

        assert_uniform_bootstrap_error(response).await;
    }

    async fn assert_uniform_bootstrap_error(response: Response) {
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        let body = to_bytes(response.into_body(), 1024).await.expect("body");
        assert_eq!(
            body.as_ref(),
            crate::auth::joplin::BOOTSTRAP_AUTH_FAILED_MESSAGE.as_bytes()
        );
    }

    fn bootstrap_rate_limit_config(per_ip_per_minute: u32, per_email_per_hour: u32) -> Config {
        Config {
            bootstrap_rate_limit: BootstrapRateLimitConfig {
                per_ip_per_minute,
                per_email_per_hour,
            },
            ..Config::default()
        }
    }

    fn test_state(config: Config) -> AppState {
        AppState {
            config,
            readiness: Readiness::new(ReadinessStatus::Ready),
            mcp_auth: McpAuth::SyntaxOnly,
            api_backend: ApiBackend::NotConfigured,
            bootstrap_rate_limiter: BootstrapRateLimiter::new(),
        }
    }

    fn bearer() -> String {
        format!("Bearer {}", generate_raw_token())
    }

    fn mcp_request(method: &str) -> Request<Body> {
        Request::post("/mcp")
            .header("authorization", bearer())
            .header("content-type", "application/json")
            .header("accept", "application/json")
            .header("mcp-protocol-version", MCP_PROTOCOL_VERSION)
            .body(Body::from(format!(
                r#"{{"jsonrpc":"2.0","id":1,"method":"{method}"}}"#
            )))
            .expect("request")
    }

    #[tokio::test]
    async fn mcp_post_request_returns_json_without_session_header() {
        let app = router(Config::default(), Readiness::new(ReadinessStatus::Ready));

        let response = app
            .oneshot(mcp_request("tools/list"))
            .await
            .expect("response");

        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response.headers().get(header::CONTENT_TYPE),
            Some(&axum::http::HeaderValue::from_static("application/json"))
        );
        assert!(response.headers().get("mcp-session-id").is_none());
    }

    #[tokio::test]
    async fn mcp_tools_list_returns_registered_tools() {
        let app = router(Config::default(), Readiness::new(ReadinessStatus::Ready));

        let response = app
            .oneshot(mcp_request("tools/list"))
            .await
            .expect("response");

        assert_eq!(response.status(), StatusCode::OK);
        let body = to_bytes(response.into_body(), 8192).await.expect("body");
        let value: serde_json::Value = serde_json::from_slice(&body).expect("json");
        assert_eq!(value["jsonrpc"], "2.0");
        assert_eq!(value["result"]["tools"][0]["name"], "status");
        assert!(value["result"]["tools"].as_array().expect("tools").len() > 1);
        assert!(value["result"].get("serverInfo").is_none());
    }

    #[tokio::test]
    async fn mcp_unknown_method_returns_json_rpc_error() {
        let app = router(Config::default(), Readiness::new(ReadinessStatus::Ready));

        let response = app
            .oneshot(mcp_request("unknown/method"))
            .await
            .expect("response");

        assert_eq!(response.status(), StatusCode::OK);
        let body = to_bytes(response.into_body(), 4096).await.expect("body");
        let value: serde_json::Value = serde_json::from_slice(&body).expect("json");
        assert_eq!(value["error"]["code"], -32601);
        assert_eq!(value["error"]["message"], "method not found");
    }

    #[tokio::test]
    async fn mcp_notification_returns_accepted_with_no_body() {
        let app = router(Config::default(), Readiness::new(ReadinessStatus::Ready));

        let response = app
            .oneshot(
                Request::post("/mcp")
                    .header("authorization", bearer())
                    .header("content-type", "application/json")
                    .header("accept", "application/json")
                    .header("mcp-protocol-version", MCP_PROTOCOL_VERSION)
                    .body(Body::from(
                        r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#,
                    ))
                    .expect("request"),
            )
            .await
            .expect("response");

        assert_eq!(response.status(), StatusCode::ACCEPTED);
        let body = to_bytes(response.into_body(), 1024).await.expect("body");
        assert!(body.is_empty());
    }

    #[tokio::test]
    async fn mcp_get_and_delete_return_method_not_allowed() {
        let app = router(Config::default(), Readiness::new(ReadinessStatus::Ready));

        let get_response = app
            .clone()
            .oneshot(Request::get("/mcp").body(Body::empty()).expect("request"))
            .await
            .expect("response");
        let delete_response = app
            .oneshot(
                Request::delete("/mcp")
                    .body(Body::empty())
                    .expect("request"),
            )
            .await
            .expect("response");

        assert_eq!(get_response.status(), StatusCode::METHOD_NOT_ALLOWED);
        assert_eq!(delete_response.status(), StatusCode::METHOD_NOT_ALLOWED);
    }

    #[tokio::test]
    async fn mcp_rejects_unsupported_or_missing_post_initialize_protocol_version() {
        let app = router(Config::default(), Readiness::new(ReadinessStatus::Ready));

        let unsupported = app
            .clone()
            .oneshot(
                Request::post("/mcp")
                    .header("authorization", bearer())
                    .header("content-type", "application/json")
                    .header("accept", "application/json")
                    .header("mcp-protocol-version", "2024-11-05")
                    .body(Body::from(
                        r#"{"jsonrpc":"2.0","id":1,"method":"initialize"}"#,
                    ))
                    .expect("request"),
            )
            .await
            .expect("response");
        let missing = app
            .oneshot(
                Request::post("/mcp")
                    .header("authorization", bearer())
                    .header("content-type", "application/json")
                    .header("accept", "application/json")
                    .body(Body::from(
                        r#"{"jsonrpc":"2.0","id":1,"method":"tools/list"}"#,
                    ))
                    .expect("request"),
            )
            .await
            .expect("response");

        assert_eq!(unsupported.status(), StatusCode::BAD_REQUEST);
        assert_eq!(missing.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn mcp_initialize_may_omit_protocol_version() {
        let app = router(Config::default(), Readiness::new(ReadinessStatus::Ready));

        let response = app
            .oneshot(
                Request::post("/mcp")
                    .header("authorization", bearer())
                    .header("content-type", "application/json")
                    .header("accept", "application/json")
                    .body(Body::from(
                        r#"{"jsonrpc":"2.0","id":1,"method":"initialize"}"#,
                    ))
                    .expect("request"),
            )
            .await
            .expect("response");

        assert_eq!(response.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn mcp_rejects_unacceptable_accept_and_bad_bearer() {
        let app = router(Config::default(), Readiness::new(ReadinessStatus::Ready));

        let unacceptable = app
            .clone()
            .oneshot(
                Request::post("/mcp")
                    .header("authorization", bearer())
                    .header("content-type", "application/json")
                    .header("accept", "text/event-stream")
                    .header("mcp-protocol-version", MCP_PROTOCOL_VERSION)
                    .body(Body::from(
                        r#"{"jsonrpc":"2.0","id":1,"method":"tools/list"}"#,
                    ))
                    .expect("request"),
            )
            .await
            .expect("response");
        let bad_bearer = app
            .oneshot(
                Request::post("/mcp")
                    .header("authorization", "Bearer mcp_short")
                    .header("content-type", "application/json")
                    .header("accept", "application/json")
                    .header("mcp-protocol-version", MCP_PROTOCOL_VERSION)
                    .body(Body::from(
                        r#"{"jsonrpc":"2.0","id":1,"method":"tools/list"}"#,
                    ))
                    .expect("request"),
            )
            .await
            .expect("response");

        assert_eq!(unacceptable.status(), StatusCode::NOT_ACCEPTABLE);
        assert_eq!(bad_bearer.status(), StatusCode::UNAUTHORIZED);
    }
}
