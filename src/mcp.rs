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
//!
//! NOTE: `allowed_owners` is NOT enforced on /mcp in Phase 1 — doing so
//! requires tool-argument inspection. Access is bounded by the pooled token's
//! own permissions and the read-only upstream. Per-agent policy is tracked in
//! https://github.com/openabdev/ghpool/issues/17.

use axum::{
    body::{Body, Bytes},
    extract::State,
    http::{HeaderMap, Method, StatusCode},
    response::Response,
};
use std::sync::Arc;

use crate::{pool, AppState};

/// Max accepted request body (JSON-RPC frames are typically <10 KB).
pub const MAX_BODY_BYTES: usize = 1_048_576;

/// POST covers initialize/tools calls — bounded responses, generous ceiling.
const POST_TIMEOUT_SECS: u64 = 120;
/// DELETE is a small control-plane call.
const DELETE_TIMEOUT_SECS: u64 = 30;

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
) -> Response {
    let session_id = headers
        .get("mcp-session-id")
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);

    // Session termination without a session identifier is semantically invalid
    if method == Method::DELETE && session_id.is_none() {
        return rpc_error(StatusCode::BAD_REQUEST, "Mcp-Session-Id header required");
    }

    let identity = match pick_identity(&state, session_id.as_deref()).await {
        Ok(i) => i,
        Err(StatusCode::NOT_FOUND) => {
            // Per MCP Streamable HTTP spec: unknown/expired sessions get 404,
            // prompting the client to re-initialize. Never rotate identities
            // mid-session.
            return rpc_error(StatusCode::NOT_FOUND, "session not found or expired");
        }
        Err(code) => return rpc_error(code, "no upstream identity available"),
    };

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
    // Timeouts are method-specific: POST responses (including SSE tool-call
    // results) complete within a bounded window, but GET is the stream
    // resumption channel and may legitimately stay open indefinitely — a
    // total timeout there would sever healthy streams.
    let req = match method {
        Method::POST => state
            .http
            .post(upstream)
            .body(reqwest::Body::from(body))
            .timeout(std::time::Duration::from_secs(POST_TIMEOUT_SECS)),
        Method::GET => state.http.get(upstream),
        Method::DELETE => state
            .http
            .delete(upstream)
            .timeout(std::time::Duration::from_secs(DELETE_TIMEOUT_SECS)),
        _ => return rpc_error(StatusCode::METHOD_NOT_ALLOWED, "method not allowed"),
    };

    let resp = match req
        .headers(build_upstream_headers(&headers, &identity.token, &state.config.mcp.toolsets))
        .send()
        .await
    {
        Ok(r) => r,
        Err(e) => {
            tracing::error!("mcp upstream request failed: {}", e);
            return rpc_error(StatusCode::BAD_GATEWAY, "upstream request failed");
        }
    };

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

    // Upstream throttled this identity: zero its budget so the pool avoids it
    // for new sessions until the reported (or a short default) reset.
    if resp.status() == reqwest::StatusCode::TOO_MANY_REQUESTS {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        state.pool.update_rate(&identity.id, Some(0), Some(rate_reset.unwrap_or(now + 60)));
        tracing::warn!("MCP upstream 429 for identity {} — budget zeroed", identity.id);
    }

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
        .unwrap_or_else(|_| rpc_error(StatusCode::BAD_GATEWAY, "failed to build response"))
}

/// Resolve the identity for this request per MCP Streamable HTTP session
/// semantics:
/// - No session ID (i.e. `initialize`): select the highest-budget identity.
/// - Known session ID: return the pinned identity — never rotate mid-session.
/// - Unknown/expired session ID (including TTL/capacity eviction of the pin,
///   or the pinned identity leaving the pool): 404, so the client
///   re-initializes.
async fn pick_identity(
    state: &AppState,
    session_id: Option<&str>,
) -> Result<pool::Identity, StatusCode> {
    if let Some(sid) = session_id {
        if let Some(id) = state.mcp_sessions.get(sid).await {
            if let Some(ident) = state.pool.get(&id) {
                return Ok(ident);
            }
            // Pinned identity no longer in the pool — treat as terminated
            state.mcp_sessions.invalidate(sid).await;
        }
        return Err(StatusCode::NOT_FOUND);
    }
    state.pool.select().map_err(|_| StatusCode::SERVICE_UNAVAILABLE)
}

/// Minimal JSON-RPC error body for proxy-level failures, so MCP clients that
/// only speak JSON-RPC degrade gracefully.
fn rpc_error(status: StatusCode, message: &str) -> Response {
    let body = serde_json::json!({
        "jsonrpc": "2.0",
        "id": null,
        "error": { "code": -32000, "message": message }
    });
    Response::builder()
        .status(status)
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .expect("static error response")
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
    use axum::http::Request;
    use tower::ServiceExt;

    fn test_state(identity_ids: &[&str]) -> Arc<AppState> {
        test_state_with(identity_ids, "http://unused.invalid", &[])
    }

    fn test_state_with(identity_ids: &[&str], upstream: &str, toolsets: &[&str]) -> Arc<AppState> {
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
                mcp: config::McpConfig {
                    enabled: true,
                    upstream: upstream.to_string(),
                    toolsets: toolsets.iter().map(|s| s.to_string()).collect(),
                    session_ttl_secs: 3600,
                },
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
    async fn test_unknown_session_returns_404() {
        let state = test_state(&["alice"]);
        match pick_identity(&state, Some("never-seen")).await {
            Err(code) => assert_eq!(code, StatusCode::NOT_FOUND),
            Ok(_) => panic!("unknown session must not resolve an identity"),
        }
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
    async fn test_stale_pin_returns_404_and_unpins() {
        // Session pinned to an identity that no longer exists in the pool:
        // treated as terminated (404), pin removed — never identity rotation.
        let state = test_state(&["alice"]);
        state.mcp_sessions.insert("sess-x".to_string(), "gone".to_string()).await;
        match pick_identity(&state, Some("sess-x")).await {
            Err(code) => assert_eq!(code, StatusCode::NOT_FOUND),
            Ok(_) => panic!("stale pin must not resolve an identity"),
        }
        assert!(state.mcp_sessions.get("sess-x").await.is_none());
    }

    // ---- Integration tests: real handler against an in-process mock upstream ----

    #[derive(Clone)]
    struct Captured {
        method: String,
        auth: Option<String>,
        toolsets: Option<String>,
        session: Option<String>,
        body: String,
    }

    type CapturedLog = Arc<std::sync::Mutex<Vec<Captured>>>;

    const MOCK_SSE_BODY: &str =
        "event: message\ndata: {\"jsonrpc\":\"2.0\",\"id\":0,\"result\":{}}\n\n";

    /// Plays the GitHub-hosted MCP server: records every request it receives,
    /// returns an SSE response with an Mcp-Session-Id for `initialize` frames,
    /// plain JSON otherwise, and 500 for frames containing "fail_500".
    async fn mock_upstream_handler(
        State(captured): State<CapturedLog>,
        method: Method,
        headers: HeaderMap,
        body: Bytes,
    ) -> Response {
        let get = |n: &str| headers.get(n).and_then(|v| v.to_str().ok()).map(str::to_string);
        let body_str = String::from_utf8_lossy(&body).to_string();
        captured.lock().unwrap().push(Captured {
            method: method.to_string(),
            auth: get("authorization"),
            toolsets: get("x-mcp-toolsets"),
            session: get("mcp-session-id"),
            body: body_str.clone(),
        });
        if body_str.contains("fail_500") {
            return Response::builder()
                .status(500)
                .body(Body::from("upstream error"))
                .unwrap();
        }
        if body_str.contains("\"initialize\"") {
            return Response::builder()
                .status(200)
                .header("content-type", "text/event-stream")
                .header("mcp-session-id", "mock-sess-1")
                .body(Body::from(MOCK_SSE_BODY))
                .unwrap();
        }
        Response::builder()
            .status(200)
            .header("content-type", "application/json")
            .body(Body::from(r#"{"jsonrpc":"2.0","id":1,"result":{}}"#))
            .unwrap()
    }

    async fn spawn_mock_upstream() -> (String, CapturedLog) {
        let captured: CapturedLog = Arc::new(std::sync::Mutex::new(Vec::new()));
        let app = axum::Router::new()
            .route("/", axum::routing::any(mock_upstream_handler))
            .with_state(captured.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        (format!("http://{}", addr), captured)
    }

    fn mcp_app(state: Arc<AppState>) -> axum::Router {
        axum::Router::new()
            .route(
                "/mcp",
                axum::routing::post(mcp_proxy).get(mcp_proxy).delete(mcp_proxy),
            )
            .with_state(state)
    }

    fn post_frame(frame: &str, extra_headers: &[(&str, &str)]) -> Request<Body> {
        let mut builder = Request::builder()
            .method("POST")
            .uri("/mcp")
            .header("content-type", "application/json");
        for (k, v) in extra_headers {
            builder = builder.header(*k, *v);
        }
        builder.body(Body::from(frame.to_string())).unwrap()
    }

    #[tokio::test]
    async fn test_proxy_strips_client_auth_and_injects_pool_token() {
        let (url, captured) = spawn_mock_upstream().await;
        let state = test_state_with(&["alice"], &url, &[]);
        let resp = mcp_app(state)
            .oneshot(post_frame(
                r#"{"jsonrpc":"2.0","id":1,"method":"tools/list"}"#,
                &[("authorization", "Bearer client-secret")],
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);

        let reqs = captured.lock().unwrap();
        assert_eq!(reqs.len(), 1);
        assert_eq!(reqs[0].auth.as_deref(), Some("Bearer token-alice"));
        assert!(reqs[0].toolsets.is_none());
        assert!(reqs[0].body.contains("tools/list"));
    }

    #[tokio::test]
    async fn test_proxy_forwards_configured_toolsets() {
        let (url, captured) = spawn_mock_upstream().await;
        let state = test_state_with(&["alice"], &url, &["issues", "pull_requests"]);
        let resp = mcp_app(state)
            .oneshot(post_frame(r#"{"jsonrpc":"2.0","id":1,"method":"tools/list"}"#, &[]))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);

        let reqs = captured.lock().unwrap();
        assert_eq!(reqs[0].toolsets.as_deref(), Some("issues,pull_requests"));
    }

    #[tokio::test]
    async fn test_proxy_sse_passthrough_and_session_capture() {
        let (url, _captured) = spawn_mock_upstream().await;
        let state = test_state_with(&["alice"], &url, &[]);
        let resp = mcp_app(state.clone())
            .oneshot(post_frame(r#"{"jsonrpc":"2.0","id":0,"method":"initialize"}"#, &[]))
            .await
            .unwrap();

        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(
            resp.headers().get("content-type").unwrap(),
            "text/event-stream"
        );
        assert_eq!(resp.headers().get("mcp-session-id").unwrap(), "mock-sess-1");

        // SSE body streamed byte-identical
        let body = axum::body::to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        assert_eq!(&body[..], MOCK_SSE_BODY.as_bytes());

        // Session pinned to the identity that served initialize
        assert_eq!(
            state.mcp_sessions.get("mock-sess-1").await.as_deref(),
            Some("alice")
        );
    }

    #[tokio::test]
    async fn test_proxy_session_pinned_across_requests() {
        let (url, captured) = spawn_mock_upstream().await;
        // Two identities: without pinning, the pool's least-used tie-break
        // would flip to the other identity on the second request.
        let state = test_state_with(&["alice", "bob"], &url, &[]);
        let app = mcp_app(state);

        let resp = app
            .clone()
            .oneshot(post_frame(r#"{"jsonrpc":"2.0","id":0,"method":"initialize"}"#, &[]))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);

        let resp = app
            .oneshot(post_frame(
                r#"{"jsonrpc":"2.0","id":1,"method":"tools/list"}"#,
                &[("mcp-session-id", "mock-sess-1")],
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);

        let reqs = captured.lock().unwrap();
        assert_eq!(reqs.len(), 2);
        // Same token on both requests proves the pin overrode pool selection
        assert_eq!(reqs[0].auth, reqs[1].auth);
        assert_eq!(reqs[1].session.as_deref(), Some("mock-sess-1"));
    }

    #[tokio::test]
    async fn test_proxy_delete_unpins_session() {
        let (url, captured) = spawn_mock_upstream().await;
        let state = test_state_with(&["alice"], &url, &[]);
        state
            .mcp_sessions
            .insert("dead-sess".to_string(), "alice".to_string())
            .await;

        let req = Request::builder()
            .method("DELETE")
            .uri("/mcp")
            .header("mcp-session-id", "dead-sess")
            .body(Body::empty())
            .unwrap();
        let resp = mcp_app(state.clone()).oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);

        // DELETE was forwarded upstream and the local pin was dropped
        assert_eq!(captured.lock().unwrap()[0].method, "DELETE");
        assert!(state.mcp_sessions.get("dead-sess").await.is_none());
    }

    #[tokio::test]
    async fn test_proxy_unknown_session_returns_404_jsonrpc_error() {
        let (url, captured) = spawn_mock_upstream().await;
        let state = test_state_with(&["alice"], &url, &[]);
        let resp = mcp_app(state)
            .oneshot(post_frame(
                r#"{"jsonrpc":"2.0","id":1,"method":"tools/list"}"#,
                &[("mcp-session-id", "ghost-session")],
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);

        // Error body is a JSON-RPC error object, not a bare status
        let body = axum::body::to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(v["jsonrpc"], "2.0");
        assert!(v["error"]["message"].is_string());

        // Upstream must never see a request for an unknown session
        assert!(captured.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn test_proxy_delete_without_session_returns_400() {
        let (url, captured) = spawn_mock_upstream().await;
        let state = test_state_with(&["alice"], &url, &[]);
        let req = Request::builder()
            .method("DELETE")
            .uri("/mcp")
            .body(Body::empty())
            .unwrap();
        let resp = mcp_app(state).oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
        assert!(captured.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn test_proxy_upstream_error_propagates() {
        let (url, _captured) = spawn_mock_upstream().await;
        let state = test_state_with(&["alice"], &url, &[]);
        let resp = mcp_app(state)
            .oneshot(post_frame(
                r#"{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"fail_500"}}"#,
                &[],
            ))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::INTERNAL_SERVER_ERROR);
    }
}
