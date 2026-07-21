//! Git-over-HTTPS credential issuance (`/git-credential`).
//!
//! Repository-scoped agents exchange their `X-Ghpool-Key` for a short-lived
//! GitHub App installation token scoped to EXACTLY ONE repository, usable as
//! a git HTTPS credential (`x-access-token:<token>`). This closes the last
//! long-lived-credential gap for agents: pushes authenticate as the App
//! (`<app>[bot]`), expire within the hour, and every issuance is fail-closed
//! audited.
//!
//! Request:  GET /git-credential?repo=<owner>/<name>   (X-Ghpool-Key header)
//! Response: {"username":"x-access-token","password":"…","expires_at":…}
//!
//! Policy stack (all fail-closed):
//! key auth → repo-scoped agent → repo allowlist → installation coverage →
//! audited issuance → single-repo token mint (GitHub enforces the boundary).

use axum::{
    extract::{Query, State},
    http::{HeaderMap, StatusCode},
    response::Response,
};
use std::collections::HashMap;
use std::sync::Arc;

use crate::mcp::{authenticate, rpc_error};
use crate::AppState;

pub async fn git_credential(
    State(state): State<Arc<AppState>>,
    Query(params): Query<HashMap<String, String>>,
    headers: HeaderMap,
) -> Response {
    if !state.config.mcp.enable_git_credentials {
        return rpc_error(StatusCode::NOT_FOUND, "git credentials are not enabled");
    }
    // Authenticated agents only. Startup validation guarantees agents exist
    // when the endpoint is enabled, so network-trust mode (None) is denied.
    let agent = match authenticate(&state, &headers) {
        Ok(Some(a)) => a,
        Ok(None) => {
            return rpc_error(StatusCode::UNAUTHORIZED, "agent authentication required")
        }
        Err(resp) => return *resp,
    };

    // Exactly one repository per credential: owner/name, strict shape.
    let Some((owner, name)) = params
        .get("repo")
        .and_then(|r| r.split_once('/'))
        .filter(|(o, n)| {
            !o.is_empty() && !n.is_empty() && !n.contains('/') && *o == o.trim() && *n == n.trim()
        })
    else {
        return rpc_error(StatusCode::BAD_REQUEST, "repo=<owner>/<name> query required");
    };

    // Repository-scoped agents only — a repo-less agent has no installation
    // envelope, and git credentials are never PAT-backed.
    if agent.repos.is_empty() {
        tracing::warn!(
            "git-credential DENIED (repo-less agent) [agent={}]",
            agent.id
        );
        return rpc_error(
            StatusCode::FORBIDDEN,
            "git credentials require a repository-scoped agent",
        );
    }
    if !crate::policy::repo_allowed(&agent.repos, owner, name) {
        tracing::warn!(
            "git-credential DENIED (repo {}/{} not allowlisted) [agent={}]",
            owner, name, agent.id
        );
        return rpc_error(
            StatusCode::FORBIDDEN,
            "repository not permitted by agent policy",
        );
    }

    // Resolve the installation: multi-app routes by owner; single-app must
    // match the configured owner when one is set.
    let owner_key = owner.to_lowercase();
    let provider = if let Some(multi) = &state.multi_app_tokens {
        match multi.get(&owner_key) {
            Some(p) => p,
            None => {
                tracing::warn!(
                    "git-credential DENIED (no installation for owner {}) [agent={}]",
                    owner, agent.id
                );
                return rpc_error(
                    StatusCode::FORBIDDEN,
                    "no GitHub App installation configured for repository owner",
                );
            }
        }
    } else if let Some(single) = &state.app_tokens {
        if let Some(configured) = state
            .config
            .mcp
            .github_app
            .as_ref()
            .and_then(|a| a.owner.as_deref())
        {
            if !configured.eq_ignore_ascii_case(owner) {
                return rpc_error(
                    StatusCode::FORBIDDEN,
                    "no GitHub App installation configured for repository owner",
                );
            }
        }
        single
    } else {
        // Unreachable: validation requires an App backend.
        return rpc_error(StatusCode::BAD_GATEWAY, "no GitHub App backend configured");
    };

    // Single-repo token: the tightest scope GitHub can enforce.
    let token = match provider.token_scoped(&[name.to_string()]).await {
        Ok(t) => t,
        Err(e) => {
            tracing::error!(
                "git-credential mint failed for {}/{}: {}",
                owner, name, e
            );
            return rpc_error(StatusCode::BAD_GATEWAY, "credential mint failed");
        }
    };

    // FAIL-CLOSED audit: no record, no credential.
    let Some(sink) = &state.audit else {
        return rpc_error(StatusCode::SERVICE_UNAVAILABLE, "audit backend unavailable");
    };
    let cred_label = format!("github-app:{}", owner_key);
    if let Err(e) = sink.record_git_credential(
        &agent.id,
        &cred_label,
        &format!("{}/{}", owner, name),
        token.expires_at,
    ) {
        tracing::error!(
            "audit unavailable — rejecting git-credential issuance (fail-closed): {}",
            e
        );
        return rpc_error(
            StatusCode::SERVICE_UNAVAILABLE,
            "audit backend unavailable — credential rejected",
        );
    }

    tracing::info!(
        "git-credential issued for {}/{} [agent={} via {}] (expires_at={})",
        owner, name, agent.id, cred_label, token.expires_at
    );
    let body = serde_json::json!({
        "username": "x-access-token",
        "password": token.token,
        "expires_at": token.expires_at,
    });
    Response::builder()
        .status(StatusCode::OK)
        .header("content-type", "application/json")
        .header("cache-control", "no-store")
        .body(axum::body::Body::from(body.to_string()))
        .unwrap_or_else(|_| rpc_error(StatusCode::INTERNAL_SERVER_ERROR, "response build failed"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{cache, config, pool};
    use axum::body::Body;
    use axum::http::Request;
    use tower::ServiceExt;

    async fn spawn_mock_github() -> String {
        async fn mint(token: &'static str) -> axum::Json<serde_json::Value> {
            let exp = time::OffsetDateTime::from_unix_timestamp(
                (std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_secs()
                    + 3600) as i64,
            )
            .unwrap()
            .format(&time::format_description::well_known::Rfc3339)
            .unwrap();
            axum::Json(serde_json::json!({"token": token, "expires_at": exp}))
        }
        let app = axum::Router::new()
            .route(
                "/app/installations/41/access_tokens",
                axum::routing::post(|| mint("ghs_git_openabdev")),
            )
            .route(
                "/app/installations/42/access_tokens",
                axum::routing::post(|| mint("ghs_git_oablab")),
            );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        format!("http://{}", addr)
    }

    fn agent(id: &str, key: &str, repos: &[&str]) -> config::McpAgentConfig {
        config::McpAgentConfig {
            id: id.into(),
            key: None,
            keys: vec![key.into()],
            tools: vec![],
            repos: repos.iter().map(|s| s.to_string()).collect(),
        }
    }

    async fn test_state(
        enabled: bool,
        sink: Option<crate::audit::AuditSink>,
    ) -> Arc<AppState> {
        let gh = spawn_mock_github().await;
        let entries = vec![
            config::GithubAppsEntry {
                app_id: "111".into(),
                private_key: crate::app_token::tests::TEST_RSA_PEM.into(),
                installation_id: Some(41),
                owner: "openabdev".into(),
            },
            config::GithubAppsEntry {
                app_id: "222".into(),
                private_key: crate::app_token::tests::TEST_RSA_PEM.into(),
                installation_id: Some(42),
                owner: "oablab".into(),
            },
        ];
        let multi = crate::app_token::MultiAppTokenProvider::new(&entries, gh).unwrap();
        let cache_config = config::CacheConfig::default();
        Arc::new(AppState {
            pool: pool::PatPool::new(&[]),
            cache: cache::Cache::new(&cache_config),
            config: config::Config {
                port: 8080,
                identities: vec![],
                allowed_owners: vec![],
                cache: cache_config,
                mcp: config::McpConfig {
                    enabled: true,
                    enable_writes: false,
                    enable_git_credentials: enabled,
                    upstream: None,
                    toolsets: vec![],
                    session_ttl_secs: 3600,
                    max_inflight_writes: 4,
                    agents: vec![
                        agent("b0", "key-b0", &["openabdev/openab", "oablab/chi"]),
                        agent("norepo", "key-norepo", &[]),
                    ],
                    github_app: None,
                    github_apps: entries,
                    audit: None,
                },
            },
            token_users: moka::future::Cache::builder().max_capacity(10).build(),
            http: reqwest::Client::new(),
            mcp_sessions: moka::future::Cache::builder().max_capacity(10).build(),
            app_tokens: None,
            multi_app_tokens: Some(multi),
            audit: sink,
            write_inflight: Arc::new(std::sync::Mutex::new(std::collections::HashMap::new())),
        })
    }

    fn app(state: Arc<AppState>) -> axum::Router {
        axum::Router::new()
            .route("/git-credential", axum::routing::get(git_credential))
            .with_state(state)
    }

    fn req(repo: &str, key: Option<&str>) -> Request<Body> {
        let mut b = Request::builder()
            .method("GET")
            .uri(format!("/git-credential?repo={}", repo));
        if let Some(k) = key {
            b = b.header("x-ghpool-key", k);
        }
        b.body(Body::empty()).unwrap()
    }

    fn audit_tmp(name: &str) -> String {
        std::env::temp_dir()
            .join(format!("ghpool-gitcred-{}-{}.jsonl", name, std::process::id()))
            .to_str()
            .unwrap()
            .to_string()
    }

    #[tokio::test]
    async fn test_disabled_is_404() {
        let state = test_state(false, None).await;
        let resp = app(state)
            .oneshot(req("openabdev/openab", Some("key-b0")))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn test_missing_or_bad_key_is_401() {
        let path = audit_tmp("auth");
        let sink = crate::audit::AuditSink::open(&path).unwrap();
        let state = test_state(true, Some(sink)).await;
        for key in [None, Some("wrong")] {
            let resp = app(state.clone())
                .oneshot(req("openabdev/openab", key))
                .await
                .unwrap();
            assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
        }
        std::fs::remove_file(&path).ok();
    }

    #[tokio::test]
    async fn test_issues_single_repo_token_and_audits() {
        let path = audit_tmp("ok");
        let sink = crate::audit::AuditSink::open(&path).unwrap();
        let state = test_state(true, Some(sink)).await;
        let resp = app(state)
            .oneshot(req("openabdev/openab", Some("key-b0")))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(resp.headers().get("cache-control").unwrap(), "no-store");
        let body = axum::body::to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(v["username"], "x-access-token");
        assert_eq!(v["password"], "ghs_git_openabdev");
        assert!(v["expires_at"].as_u64().unwrap() > 0);

        let records: Vec<serde_json::Value> = std::fs::read_to_string(&path)
            .unwrap()
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0]["phase"], "git_credential");
        assert_eq!(records[0]["agent"], "b0");
        assert_eq!(records[0]["cred"], "github-app:openabdev");
        assert_eq!(records[0]["repo"], "openabdev/openab");
        // the token value itself is never audited
        assert!(!records[0].to_string().contains("ghs_git_openabdev"));
        std::fs::remove_file(&path).ok();
    }

    #[tokio::test]
    async fn test_routes_by_owner() {
        let path = audit_tmp("route");
        let sink = crate::audit::AuditSink::open(&path).unwrap();
        let state = test_state(true, Some(sink)).await;
        let resp = app(state)
            .oneshot(req("oablab/chi", Some("key-b0")))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = axum::body::to_bytes(resp.into_body(), usize::MAX).await.unwrap();
        let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(v["password"], "ghs_git_oablab");
        std::fs::remove_file(&path).ok();
    }

    #[tokio::test]
    async fn test_policy_denials() {
        let path = audit_tmp("deny");
        let sink = crate::audit::AuditSink::open(&path).unwrap();
        let state = test_state(true, Some(sink)).await;
        // off-allowlist repo
        let resp = app(state.clone())
            .oneshot(req("openabdev/secret-repo", Some("key-b0")))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::FORBIDDEN);
        // repo-less agent
        let resp = app(state.clone())
            .oneshot(req("openabdev/openab", Some("key-norepo")))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::FORBIDDEN);
        // malformed repo param
        let resp = app(state.clone())
            .oneshot(req("justanowner", Some("key-b0")))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
        // denials never mint, never audit
        assert!(std::fs::read_to_string(&path).unwrap().is_empty());
        std::fs::remove_file(&path).ok();
    }

    #[tokio::test]
    async fn test_audit_fail_closed() {
        let sink = crate::audit::AuditSink::failing_for_tests();
        let state = test_state(true, Some(sink)).await;
        let resp = app(state)
            .oneshot(req("openabdev/openab", Some("key-b0")))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::SERVICE_UNAVAILABLE);
    }
}
