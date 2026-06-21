//! Local HTTP agent API (`metis serve`) — Axum server for Windows and other OSes.
//!
//! Binds to loopback by default. Optional Bearer token protects `/v1/*`.

use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use axum::{
    extract::State,
    http::{header, Request, StatusCode},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use metis_agent::AgentLoop;
use metis_core::config::load_config;
use serde::{Deserialize, Serialize};
use tokio::net::TcpListener;
use tower_http::cors::CorsLayer;
use tower_http::timeout::TimeoutLayer;
use tower_http::trace::TraceLayer;
use tracing::{info, warn};

use crate::agent_builder::{build_agent_loop, init_logging};

#[derive(Clone)]
struct AppState {
    agent: Arc<AgentLoop>,
    model: String,
    /// When set, `/v1/*` requires `Authorization: Bearer …`.
    bearer_token: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ChatRequest {
    message: String,
    #[serde(default)]
    session: Option<String>,
}

#[derive(Debug, Serialize)]
struct ChatResponse {
    response: String,
}

#[derive(Debug, Serialize)]
struct ErrorBody {
    error: String,
}

/// Run the local HTTP server until Ctrl+C.
pub async fn run(
    host: Option<String>,
    port: Option<u16>,
    api_key: Option<String>,
    logs: bool,
) -> Result<()> {
    init_logging(logs);

    let mut config = load_config(None);
    let hs = &mut config.http_server;
    if let Some(h) = host {
        if !h.trim().is_empty() {
            hs.host = h;
        }
    }
    if let Some(p) = port {
        hs.port = p;
    }
    if let Some(k) = api_key {
        hs.api_key = k;
    }

    let host_str = hs.host.trim().to_string();
    let port_u16 = hs.port;

    let bearer_token = {
        let t = hs.api_key.trim();
        if t.is_empty() {
            None
        } else {
            Some(t.to_string())
        }
    };
    let auth_configured = bearer_token.is_some();

    if host_str == "0.0.0.0" && !auth_configured {
        warn!(
            "http_server.host is 0.0.0.0 but http_server.apiKey is empty — \
             the agent API will be reachable on your LAN without authentication"
        );
    }

    let agent = Arc::new(build_agent_loop(&config, None)?);
    let model = agent.model().to_string();
    let state = AppState {
        agent,
        model,
        bearer_token,
    };

    let addr = format!("{host_str}:{port_u16}");
    let app = build_router(state.clone());

    let listener = TcpListener::bind(&addr)
        .await
        .with_context(|| format!("failed to bind HTTP server to {addr}"))?;

    info!(%addr, "Metis HTTP agent API listening (POST /v1/chat)");
    println!();
    println!("  Metis HTTP agent API");
    println!("  Listening:  http://{addr}");
    println!("  Health:     GET  http://{addr}/health");
    println!("  Status:     GET  http://{addr}/v1/status");
    println!("  Chat:       POST http://{addr}/v1/chat  JSON {{ \"message\": \"...\", \"session\": \"optional\" }}");
    if !auth_configured {
        println!("  Auth:       none (set httpServer.apiKey in config or pass --api-key)");
    } else {
        println!("  Auth:       Bearer token required on /v1/*");
    }
    println!();
    println!("  Ctrl+C to stop");
    println!();

    axum::serve(listener, app.into_make_service())
        .with_graceful_shutdown(shutdown_signal())
        .await
        .context("HTTP server error")?;

    Ok(())
}

fn build_router(state: AppState) -> Router {
    let state_mw = state.clone();

    Router::new()
        .route("/health", get(health))
        .route("/v1/status", get(status))
        .route(
            "/v1/chat",
            post(chat).layer(TimeoutLayer::new(Duration::from_secs(600))),
        )
        .layer(middleware::from_fn_with_state(
            state_mw,
            v1_optional_bearer_auth,
        ))
        .layer(CorsLayer::permissive())
        .layer(TraceLayer::new_for_http())
        .with_state(state)
}

async fn health() -> impl IntoResponse {
    Json(serde_json::json!({ "status": "ok" }))
}

async fn status(State(s): State<AppState>) -> impl IntoResponse {
    Json(serde_json::json!({
        "model": s.model,
        "channel": "http",
    }))
}

async fn chat(
    State(s): State<AppState>,
    Json(body): Json<ChatRequest>,
) -> Result<Json<ChatResponse>, (StatusCode, Json<ErrorBody>)> {
    let msg = body.message.trim();
    if msg.is_empty() {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(ErrorBody {
                error: "message must not be empty".into(),
            }),
        ));
    }

    let session = body
        .session
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or("default")
        .to_string();

    let response = s
        .agent
        .process_direct_session("http", &session, msg)
        .await
        .map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(ErrorBody {
                    error: e.to_string(),
                }),
            )
        })?;

    Ok(Json(ChatResponse { response }))
}

async fn v1_optional_bearer_auth(
    State(state): State<AppState>,
    req: Request<axum::body::Body>,
    next: Next,
) -> Result<Response, (StatusCode, Json<ErrorBody>)> {
    let path = req.uri().path();
    if !path.starts_with("/v1") {
        return Ok(next.run(req).await);
    }

    let Some(ref expected) = state.bearer_token else {
        return Ok(next.run(req).await);
    };
    if expected.is_empty() {
        return Ok(next.run(req).await);
    }

    let ok = req
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|auth| auth == format!("Bearer {expected}"));

    if !ok {
        return Err((
            StatusCode::UNAUTHORIZED,
            Json(ErrorBody {
                error: "missing or invalid Authorization header (expected Bearer token)".into(),
            }),
        ));
    }

    Ok(next.run(req).await)
}

async fn shutdown_signal() {
    let _ = tokio::signal::ctrl_c().await;
    info!("shutdown signal received, stopping HTTP server");
}
