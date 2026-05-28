mod cache;
mod config;
mod pool;

use axum::{
    extract::{Path, Query, State},
    http::{HeaderMap, StatusCode},
    response::Json,
    routing::get,
    Router,
};
use serde_json::Value;
use std::{collections::HashMap, sync::Arc};
use tracing_subscriber::EnvFilter;

struct AppState {
    pool: pool::PatPool,
    cache: cache::Cache,
    config: config::Config,
}

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env().add_directive("ghpool=info".parse().unwrap()))
        .init();

    let config = config::Config::load();
    let pool = pool::PatPool::new(&config.identities);
    let cache = cache::Cache::new(&config.cache);

    let state = Arc::new(AppState { pool, cache, config: config.clone() });

    let app = Router::new()
        .route("/healthz", get(healthz))
        .route("/stats", get(stats))
        .route("/{*path}", get(proxy))
        .with_state(state);

    let addr = format!("0.0.0.0:{}", config.port);
    tracing::info!("ghpool listening on {}", addr);
    let listener = tokio::net::TcpListener::bind(&addr).await.unwrap();
    axum::serve(listener, app).await.unwrap();
}

async fn healthz() -> &'static str {
    "ok"
}

async fn stats(State(state): State<Arc<AppState>>) -> Json<Value> {
    let identities = state.pool.snapshot();
    let cache_stats = state.cache.stats();
    Json(serde_json::json!({
        "identities": identities,
        "cache": cache_stats,
    }))
}

async fn proxy(
    State(state): State<Arc<AppState>>,
    Path(path): Path<String>,
    Query(query): Query<HashMap<String, String>>,
    headers: HeaderMap,
) -> Result<Json<Value>, StatusCode> {
    let api_path = format!("/{}", path);

    // Check allowed owners
    if !is_allowed_path(&api_path, &state.config.allowed_owners) {
        return Err(StatusCode::FORBIDDEN);
    }

    // Build cache key
    let cache_key = cache::build_key(&api_path, &query);

    // Check cache
    if let Some(cached) = state.cache.get(&cache_key).await {
        tracing::debug!("cache hit: {}", api_path);
        return Ok(Json(cached));
    }

    // Select identity from pool
    let identity = state.pool.select().map_err(|_| StatusCode::SERVICE_UNAVAILABLE)?;

    // Build GitHub API URL
    let mut url = format!("https://api.github.com{}", api_path);
    if !query.is_empty() {
        let qs: Vec<String> = query.iter().map(|(k, v)| format!("{}={}", k, v)).collect();
        url = format!("{}?{}", url, qs.join("&"));
    }

    // Forward request
    let client = reqwest::Client::new();
    let mut req = client.get(&url)
        .header("Authorization", format!("Bearer {}", identity.token))
        .header("User-Agent", "ghpool/0.1.0")
        .header("Accept", "application/vnd.github+json");

    if let Some(version) = headers.get("x-github-api-version") {
        req = req.header("X-GitHub-Api-Version", version);
    }

    let resp = req.send().await.map_err(|e| {
        tracing::error!("github request failed: {}", e);
        StatusCode::BAD_GATEWAY
    })?;

    // Update rate limit from response headers
    let rate_remaining = resp.headers()
        .get("x-ratelimit-remaining")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<u32>().ok());
    let rate_reset = resp.headers()
        .get("x-ratelimit-reset")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<u64>().ok());

    state.pool.update_rate(&identity.id, rate_remaining, rate_reset);

    let status = resp.status();
    let body: Value = resp.json().await.map_err(|e| {
        tracing::error!("failed to parse github response: {}", e);
        StatusCode::BAD_GATEWAY
    })?;

    if !status.is_success() {
        tracing::warn!("github returned {}: {}", status, api_path);
        return Err(StatusCode::from_u16(status.as_u16()).unwrap_or(StatusCode::BAD_GATEWAY));
    }

    // Write to cache
    let route_kind = cache::classify_route(&api_path);
    state.cache.insert(&cache_key, &body, route_kind).await;

    Ok(Json(body))
}

fn is_allowed_path(path: &str, allowed_owners: &[String]) -> bool {
    let parts: Vec<&str> = path.split('/').collect();
    if parts.len() >= 3 && parts[1] == "repos" {
        let owner = parts[2].to_lowercase();
        return allowed_owners.iter().any(|a| a.to_lowercase() == owner);
    }
    // Non-repo paths (e.g. /rate_limit) are allowed
    true
}
