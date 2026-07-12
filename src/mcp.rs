//! MCP reverse proxy (Phase 1: read-only).
//!
//! Proxies MCP Streamable HTTP traffic to the GitHub-hosted MCP server
//! (default: https://api.githubcopilot.com/mcp/readonly), injecting a pooled
//! GitHub credential upstream so agents never hold a GitHub token.
//!
//! Key behaviors:
//! - Session pinning: the upstream bearer token is selected once per MCP
//!   session (at `initialize`, before an `Mcp-Session-Id` exists) and pinned
//!   for the session lifetime via a `session_id → identity_id` cache.
//! - Streaming passthrough: upstream responses may be `application/json` or
//!   `text/event-stream`; bodies are streamed through untouched.
//! - Header rewrite: client `Authorization` is stripped; pooled token and
//!   optional `X-MCP-Toolsets` are injected.
//! - Audit log: JSON-RPC request frames are parsed best-effort to log
//!   `method` (and tool name for `tools/call`) per request.

use axum::{
    body::{Body, Bytes},
    extract::State,
    http::{HeaderMap, Method, StatusCode},
    response::Response,
};
use std::sync::Arc;

use crate::{pool, AppState};

/// Response headers propagated back to the MCP client.
const RESP_HEADERS: &[&str] = &["content-type", "mcp-session-id", "mcp-protocol-version"];

/// Client request headers forwarded upstream (Authorization is deliberately absent).
const FWD_HEADERS: &[&str] = &[
    "content-type",
    "accept",
    "mcp-session-id",
    "mcp-protocol-version",
    "last-event-id",
];

pub async fn mcp_proxy(
    State(state): State<Arc<AppState>>,
    method: Method,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Response, StatusCode> {
    let session_id = headers
        .get("mcp-session-id")
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);

    let identity = pick_identity(&state, session_id.as_deref()).await?;

    // Audit log (best-effort JSON-RPC frame parse)
    if method == Method::POST {
        if let Some((rpc_method, tool)) = frame_summary(&body) {
            match tool {
                Some(t) => tracing::info!(
                    "MCP {} {} [via {}]{}",
                    rpc_method, t, identity.id, session_suffix(session_id.as_deref())
                ),
                None => tracing::info!(
                    "MCP {} [via {}]{}",
                    rpc_method, identity.id, session_suffix(session_id.as_deref())
                ),
            }
        }
    }

    let upstream = &state.config.mcp.upstream;
    let req = match method {
        Method::POST => state.http.post(upstream).body(body.to_vec()),
        Method::GET => state.http.get(upstream),
        Method::DELETE => state.http.delete(upstream),
        _ => return Err(StatusCode::METHOD_NOT_ALLOWED),
    };

    let resp = req
        .headers(build_upstream_headers(&headers, &identity.token, &state.config.mcp.toolsets))
        .send()
        .await
        .map_err(|e| {
            tracing::error!("mcp upstream request failed: {}", e);
            StatusCode::BAD_GATEWAY
        })?;

    // Best-effort rate budget accounting, if upstream exposes it
    let rate_remaining = resp.headers()
        .get("x-ratelimit-remaining")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<u32>().ok());
    let rate_reset = resp.headers()
        .get("x-ratelimit-reset")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<u64>().ok());
    state.pool.update_rate(&identity.id, rate_remaining, rate_reset);

    // Pin new sessions: upstream returns Mcp-Session-Id on initialize
    if let Some(sid) = resp.headers().get("mcp-session-id").and_then(|v| v.to_str().ok()) {
        if state.mcp_sessions.get(sid).await.is_none() {
            tracing::info!(
                "MCP session pinned to identity {}{}",
                identity.id,
                session_suffix(Some(sid))
            );
            state.mcp_sessions.insert(sid.to_string(), identity.id.clone()).await;
        }
    }

    // Session termination: drop the pin
    if method == Method::DELETE {
        if let Some(sid) = &session_id {
            state.mcp_sessions.invalidate(sid).await;
        }
    }

    let status = StatusCode::from_u16(resp.status().as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);
    let mut builder = Response::builder().status(status);
    for name in RESP_HEADERS {
        if let Some(v) = resp.headers().get(*name) {
            builder = builder.header(*name, v.clone());
        }
    }

    builder
        .body(Body::from_stream(resp.bytes_stream()))
        .map_err(|_| StatusCode::BAD_GATEWAY)
}

/// Resolve the identity for this request: pinned identity if the session is
/// known, otherwise the highest-budget identity from the pool (new session).
async fn pick_identity(
    state: &AppState,
    session_id: Option<&str>,
) -> Result<pool::Identity, StatusCode> {
    if let Some(sid) = session_id {
        if let Some(id) = state.mcp_sessions.get(sid).await {
            if let Some(ident) = state.pool.get(&id) {
                return Ok(ident);
            }
        }
    }
    state.pool.select().map_err(|_| StatusCode::SERVICE_UNAVAILABLE)
}

/// Build the upstream header set from scratch: the client's Authorization (and
/// anything else unexpected) is never forwarded; the pooled token is injected.
fn build_upstream_headers(client: &HeaderMap, token: &str, toolsets: &[String]) -> HeaderMap {
    let mut h = HeaderMap::new();
    h.insert(
        "authorization",
        format!("Bearer {}", token).parse().expect("valid bearer header"),
    );
    h.insert(
        "user-agent",
        concat!("ghpool/", env!("CARGO_PKG_VERSION")).parse().expect("valid ua header"),
    );
    for name in FWD_HEADERS {
        if let Some(v) = client.get(*name) {
            h.insert(*name, v.clone());
        }
    }
    // MCP Streamable HTTP requires clients to accept both content types
    if !h.contains_key("accept") {
        h.insert("accept", "application/json, text/event-stream".parse().unwrap());
    }
    if !toolsets.is_empty() {
        if let Ok(v) = toolsets.join(",").parse() {
            h.insert("x-mcp-toolsets", v);
        }
    }
    h
}

/// Best-effort parse of a JSON-RPC request frame.
/// Returns (method, tool_name) where tool_name is set for tools/call.
fn frame_summary(body: &[u8]) -> Option<(String, Option<String>)> {
    let v: serde_json::Value = serde_json::from_slice(body).ok()?;
    let method = v.get("method")?.as_str()?.to_string();
    let tool = if method == "tools/call" {
        v.get("params")
            .and_then(|p| p.get("name"))
            .and_then(|n| n.as_str())
            .map(str::to_string)
    } else {
        None
    };
    Some((method, tool))
}

/// Whether this frame is an MCP `initialize` request (start of a new session).
#[allow(dead_code)]
fn is_initialize(body: &[u8]) -> bool {
    matches!(frame_summary(body), Some((m, _)) if m == "initialize")
}

fn session_suffix(session_id: Option<&str>) -> String {
    match session_id {
        Some(sid) => format!(" [session={}]", &sid[..sid.len().min(8)]),
        None => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{cache, config};

    fn test_state(identity_ids: &[&str]) -> Arc<AppState> {
        let identities: Vec<config::IdentityConfig> = identity_ids
            .iter()
            .map(|id| config::IdentityConfig {
                id: id.to_string(),
                token: format!("token-{}", id),
            })
            .collect();
        let pool = pool::PatPool::new(&identities);
        let cache_config = config::CacheConfig::default();
        let cache = cache::Cache::new(&cache_config);
        Arc::new(AppState {
            pool,
            cache,
            config: config::Config {
                port: 8080,
                identities,
                allowed_owners: vec!["openabdev".to_string()],
                cache: cache_config,
                mcp: config::McpConfig::default(),
            },
            token_users: moka::future::Cache::builder().max_capacity(10).build(),
            http: reqwest::Client::new(),
            mcp_sessions: moka::future::Cache::builder().max_capacity(100).build(),
        })
    }

    #[test]
    fn test_frame_summary_tools_call() {
        let body = br#"{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"get_issue","arguments":{"owner":"openabdev"}}}"#;
        let (method, tool) = frame_summary(body).unwrap();
        assert_eq!(method, "tools/call");
        assert_eq!(tool.as_deref(), Some("get_issue"));
    }

    #[test]
    fn test_frame_summary_initialize() {
        let body = br#"{"jsonrpc":"2.0","id":0,"method":"initialize","params":{"protocolVersion":"2025-06-18"}}"#;
        let (method, tool) = frame_summary(body).unwrap();
        assert_eq!(method, "initialize");
        assert!(tool.is_none());
        assert!(is_initialize(body));
        assert!(!is_initialize(br#"{"method":"tools/list"}"#));
    }

    #[test]
    fn test_frame_summary_invalid() {
        assert!(frame_summary(b"not json").is_none());
        assert!(frame_summary(br#"{"jsonrpc":"2.0","id":1,"result":{}}"#).is_none());
    }

    #[test]
    fn test_header_rewrite_strips_client_auth() {
        let mut client = HeaderMap::new();
        client.insert("authorization", "Bearer client-secret".parse().unwrap());
        client.insert("mcp-session-id", "sess-abc".parse().unwrap());
        client.insert("mcp-protocol-version", "2025-06-18".parse().unwrap());
        client.insert("x-random-header", "should-not-forward".parse().unwrap());

        let h = build_upstream_headers(&client, "pool-token", &[]);

        assert_eq!(h.get("authorization").unwrap(), "Bearer pool-token");
        assert_eq!(h.get("mcp-session-id").unwrap(), "sess-abc");
        assert_eq!(h.get("mcp-protocol-version").unwrap(), "2025-06-18");
        assert!(h.get("x-random-header").is_none());
        // default accept injected when client omits it
        assert_eq!(h.get("accept").unwrap(), "application/json, text/event-stream");
        assert!(h.get("x-mcp-toolsets").is_none());
    }

    #[test]
    fn test_header_rewrite_injects_toolsets() {
        let client = HeaderMap::new();
        let toolsets = vec!["issues".to_string(), "pull_requests".to_string()];
        let h = build_upstream_headers(&client, "t", &toolsets);
        assert_eq!(h.get("x-mcp-toolsets").unwrap(), "issues,pull_requests");
    }

    #[tokio::test]
    async fn test_session_pinning_returns_pinned_identity() {
        let state = test_state(&["alice", "bob"]);
        state.mcp_sessions.insert("sess-1".to_string(), "bob".to_string()).await;

        let ident = pick_identity(&state, Some("sess-1")).await.unwrap();
        assert_eq!(ident.id, "bob");
        assert_eq!(ident.token, "token-bob");
    }

    #[tokio::test]
    async fn test_unknown_session_falls_back_to_pool() {
        let state = test_state(&["alice"]);
        let ident = pick_identity(&state, Some("never-seen")).await.unwrap();
        assert_eq!(ident.id, "alice");
    }

    #[tokio::test]
    async fn test_no_session_selects_from_pool() {
        let state = test_state(&["alice"]);
        let ident = pick_identity(&state, None).await.unwrap();
        assert_eq!(ident.id, "alice");
    }

    #[tokio::test]
    async fn test_no_identities_returns_503() {
        let state = test_state(&[]);
        match pick_identity(&state, None).await {
            Err(code) => assert_eq!(code, StatusCode::SERVICE_UNAVAILABLE),
            Ok(_) => panic!("expected SERVICE_UNAVAILABLE with empty pool"),
        }
    }

    #[tokio::test]
    async fn test_stale_pin_falls_back_to_pool() {
        // Session pinned to an identity that no longer exists in the pool
        let state = test_state(&["alice"]);
        state.mcp_sessions.insert("sess-x".to_string(), "gone".to_string()).await;
        let ident = pick_identity(&state, Some("sess-x")).await.unwrap();
        assert_eq!(ident.id, "alice");
    }
}
