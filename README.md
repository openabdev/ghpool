# ghpool

A secure, cloud-native GitHub API proxy that pools PATs for rate limit sharing, caches read responses, and passes through mutations — built for coding agents running in private networks.

## Design Principles

- **Cloud-native** — runs on any Kubernetes (Amazon EKS, Google Cloud GKE, self-managed k8s) and Amazon ECS. Single static binary, no runtime dependencies.
- **Built for agents, not humans** — optimized for high-throughput, concurrent API access from multiple coding agents sharing the same repos.
- **Secrets-first** — credentials are resolved at runtime from AWS Secrets Manager, Google Cloud Secret Manager, or Kubernetes secrets. No plain text tokens stored at rest or in transit.
- **Private network isolation** — designed to run inside your trusted network (on-premises, cloud VPC, or service mesh). No public endpoints, no external dependencies beyond GitHub API.

## Architecture

```
┌─────────────────────────────────────────────────────────────────────┐
│                        Private Network / VPC                        │
│                                                                     │
│  ┌──────────┐  ┌──────────┐  ┌──────────┐                           │
│  │ Agent A  │  │ Agent B  │  │ gh CLI   │                           │
│  └────┬─────┘  └────┬─────┘  └────┬─────┘                           │
│       │              │              │                               │
│       └──────────────┼──────────────┘                               │
│                      │                                              │
│                      ▼                                              │
│            ┌───────────────────┐                                    │
│            │      ghpool       │                                    │
│            │                   │                                    │
│            │  ┌─────────────┐  │      ┌──────────────────────┐      │
│            │  │  PAT Pool   │  │      │  Secrets Manager     │      │
│            │  │             │◄─┼──────│  (AWS/K8s/Env)       │      │
│            │  │ chaodu: 4998│  │      └──────────────────────┘      │
│            │  │ thepagent:  │  │                                    │
│            │  │         1889│  │                                    │
│            │  └─────────────┘  │                                    │
│            │  ┌─────────────┐  │                                    │
│            │  │    Cache    │  │                                    │
│            │  │  (in-mem)   │  │                                    │
│            │  └─────────────┘  │                                    │
│            └────────┬──────────┘                                    │
│                     │                                               │
└─────────────────────┼───────────────────────────────────────────────┘
                      │
                      ▼
            ┌───────────────────┐
            │  api.github.com   │
            └───────────────────┘

Request Flow:

  GET /repos/org/repo/pulls
    → cache HIT? return cached
    → cache MISS: select PAT with highest remaining budget
    → forward to GitHub, cache response, update rate limits

  POST /graphql (query)
    → cache HIT? return cached
    → cache MISS: select pooled PAT, forward, cache response

  POST /graphql (mutation)
    → require client Authorization header
    → passthrough to GitHub (no pooling, no caching)
    → resolve + log GitHub username from token

  POST /mcp (opt-in, Phase 1: read-only)
    → MCP Streamable HTTP reverse proxy to GitHub's hosted MCP server
    → strip client auth, inject pooled credential, pin token per session
    → audit-log every tools/call
```

## What it does

- Pools multiple GitHub PATs and routes each read request through the identity with the most remaining rate limit budget
- Caches GitHub REST and GraphQL query responses in memory with configurable TTLs
- Proxies GraphQL mutations with passthrough auth (client's own token, no caching)
- **MCP reverse proxy** (opt-in) — agents connect an MCP client to `/mcp` and get GitHub's official MCP tools with **no GitHub credential on the agent**; ghpool injects a pooled credential upstream
- Mirrors the GitHub API path structure — clients just change the base URL
- Restricts access to configured org/owner repos only
- Auto-resolves GitHub username from tokens for audit logging

## Quick start

```sh
cp config.example.toml config.toml
# Edit config.toml with your PATs and allowed owners

cargo run --release
# Listening on 0.0.0.0:8080

curl http://localhost:8080/repos/openclaw/chi/pulls/123
curl http://localhost:8080/stats
```

## Configuration

### TOML file

Set `GHPOOL_CONFIG` env var to point to your config file (defaults to `config.toml`).

See [config.example.toml](config.example.toml) for all options.

### Secret references

The `token` field in `[[identities]]` supports multiple secret sources, so credentials never need to exist in plain text on disk:

| Format | Source |
|--------|--------|
| `ghp_xxx...` | Plain literal (local dev only) |
| `env:VAR_NAME` | Environment variable |
| `aws:secretsmanager:secret-name:json-key` | AWS Secrets Manager |
| `k8s:namespace/secret-name:key` | Kubernetes secret (mounted volume) |

#### AWS Secrets Manager

Store PATs as a JSON object in a single secret:

```sh
aws secretsmanager create-secret --name ghpool/pats \
  --secret-string '{"pat_alice":"ghp_xxx","pat_bob":"ghp_yyy"}'
```

```toml
[[identities]]
id = "alice"
token = "aws:secretsmanager:ghpool/pats:pat_alice"
```

ghpool uses the standard AWS credential chain (instance profile, ECS task role, SSO, env vars).

#### Google Cloud Secret Manager (planned)

```toml
[[identities]]
id = "alice"
token = "gcp:secretmanager:projects/my-proj/secrets/ghpool-pat:latest"
```

GCP support is on the roadmap. Contributions welcome.

#### Kubernetes Secrets

Mount your secret as a volume at `/etc/secrets/` and reference it:

```yaml
# K8s Secret
apiVersion: v1
kind: Secret
metadata:
  name: ghpool-pats
  namespace: default
stringData:
  pat_alice: ghp_xxx
```

```toml
[[identities]]
id = "alice"
token = "k8s:default/ghpool-pats:pat_alice"
```

Works with any Kubernetes distribution — EKS, GKE, AKS, k3s, or self-managed.

### Environment variables only

```sh
export GHPOOL_PORT=8080
export GHPOOL_ALLOWED_OWNERS=openclaw,openabdev
export GHPOOL_PAT_ALICE=ghp_xxx
export GHPOOL_PAT_BOB=ghp_yyy
```

PATs are discovered from any env var matching `GHPOOL_PAT_<ID>=<token>`.

## Deployment

### Docker

```sh
docker build -t ghpool .
docker run -p 8080:8080 -v ./config.toml:/config.toml ghpool
```

### ECS (Service Connect)

Deploy as a service in your ECS cluster with Cloud Map namespace. Other services access it via:
```
http://ghpool.<namespace>:8080/repos/owner/repo/pulls/123
```

### Kubernetes

Deploy as a ClusterIP Service. Other pods access it via:
```
http://ghpool.<namespace>.svc.cluster.local:8080/repos/owner/repo/pulls/123
```

## API

### REST (GET)

All GitHub REST API GET paths are proxied transparently with PAT pooling and caching:

```
GET /<github-api-path>
```

### GraphQL (POST /graphql)

```
POST /graphql
```

- **Queries** — routed through pooled PATs, responses cached
- **Mutations** — client's own `Authorization` header passed through to GitHub (no pooling, no caching)

If a mutation request has no `Authorization` header, ghpool returns `401`.

```
  ┌────────────────────────────────────────────────────────────────┐
  │                    POST /graphql                               │
  │                                                                │
  │  Parse request body → extract "query" field                    │
  │                                                                │
  │  ┌─────────────────────┐       ┌────────────────────────────┐  │
  │  │ starts with "query" │       │ starts with "mutation"     │  │
  │  └──────────┬──────────┘       └──────────────┬─────────────┘  │
  │             │                                  │               │
  │             ▼                                  ▼               │
  │  ┌─────────────────────┐       ┌────────────────────────────┐  │
  │  │ Check cache         │       │ Require client             │  │
  │  │  HIT → return       │       │ Authorization header       │  │
  │  │  MISS ↓             │       │  missing → 401             │  │
  │  └──────────┬──────────┘       └──────────────┬─────────────┘  │
  │             │                                  │               │
  │             ▼                                  ▼               │
  │  ┌─────────────────────┐       ┌────────────────────────────┐  │
  │  │ Select pooled PAT   │       │ Passthrough client's token │  │
  │  │ (highest budget)    │       │ (identity preserved)       │  │
  │  └──────────┬──────────┘       └──────────────┬─────────────┘  │
  │             │                                  │               │
  │             ▼                                  ▼               │
  │  ┌─────────────────────┐       ┌────────────────────────────┐  │
  │  │ Forward to GitHub   │       │ Forward to GitHub          │  │
  │  │ Cache response      │       │ No caching                 │  │
  │  │ Update rate limits  │       │ Log resolved username      │  │
  │  └─────────────────────┘       └────────────────────────────┘  │
  └────────────────────────────────────────────────────────────────┘
```

### MCP (POST/GET/DELETE /mcp) — opt-in

Reverse proxy to [GitHub's hosted MCP server](https://github.com/github/github-mcp-server): agents connect a Model Context Protocol client to ghpool and get GitHub's official MCP tools — **with no GitHub credential on the agent**. ghpool strips any client `Authorization` header and injects a pooled credential upstream.

> **Status: read-only.** The upstream is pinned to the `/readonly` tool surface. Per-agent authentication and default-deny tool allowlists are available (Phase 2a below). Write access ships with the rest of Phase 2 — see the [RFC](https://github.com/openabdev/ghpool/issues/15).

```
┌───────────────── Private Network / VPC ─────────────────┐
│                                                         │
│  ┌───────────────────┐         ┌────────────────────┐   │
│  │  MCP client       │         │  ghpool            │   │
│  │  (agent)          │  MCP    │                    │   │
│  │                   │ ──────► │  1. strip client   │   │
│  │  no GitHub        │  HTTP   │     Authorization  │   │
│  │  credential       │ ◄────── │  2. pin pooled     │   │
│  └───────────────────┘         │     token/session  │   │
│                                │  3. audit-log      │   │
│                                │     tools/call     │   │
│                                └─────────┬──────────┘   │
└──────────────────────────────────────────┼──────────────┘
                                           │ Bearer <pooled credential>
                                           ▼
                            ┌──────────────────────────────┐
                            │ api.githubcopilot.com/mcp/   │
                            │ readonly    (hosted, GitHub- │
                            │ maintained tool schemas)     │
                            └──────────────────────────────┘
```

Enable it in `config.toml` (or `GHPOOL_MCP_ENABLED=true`):

```toml
[mcp]
enabled = true
# upstream = "https://api.githubcopilot.com/mcp/readonly"  # default
# toolsets = ["issues", "pull_requests", "repos"]          # optional coarse filter
# session_ttl_secs = 3600                                  # session pin idle TTL
```

Behavior notes:

- **Sessions are pinned** — the pooled identity is selected at `initialize` and reused for the whole MCP session. An unknown or expired session gets `404` (per MCP spec) and the client re-initializes transparently.
- **Tool names differ from the write server** — the readonly surface uses e.g. `issue_read`, not `get_issue`. Discover them via `tools/list`.
- **Audit log** — every JSON-RPC frame is logged with method, tool name, identity, and session: `MCP tools/call issue_read [via alice] [session=7b86a7eb]`.
- **`allowed_owners` is not enforced on `/mcp`** in Phase 1 — access is bounded by the pooled credential's own permissions and the read-only upstream. Per-agent repo allowlists arrive in Phase 2.

#### Per-agent authentication (Phase 2a)

Add `[[mcp.agents]]` entries to require an API key on every `/mcp` request and enforce a **default-deny tool allowlist** per agent:

```toml
[[mcp.agents]]
id = "openab-bot"
key = "aws:secretsmanager:ghpool/mcp-keys:openab"   # env:/k8s: refs also work
tools = ["issue_read", "list_issues", "pull_request_read"]
```

- With any agent configured, requests without a valid `X-Ghpool-Key` get `401`; a `tools/call` for a tool not on the agent's allowlist gets `403` at the proxy — it never reaches GitHub. The allowlist is also injected upstream as `X-MCP-Tools`, so `tools/list` natively shows the agent only its permitted tools.
- New upstream tools are **denied by default** until added to an agent's `tools` list.
- The key is a **ghpool credential, not a GitHub credential** — a leak is bounded by that agent's allowlist and revoked by editing ghpool config, without touching GitHub.
- Audit lines include the agent: `MCP tools/call issue_read [agent=openab-bot via alice] [session=…]`.
- Terminate TLS in front of ghpool (ALB, ingress, mesh) in production — the key travels in a header.

Client config gains one line:

```json
{
  "mcpServers": {
    "github": {
      "url": "http://ghpool.<namespace>:8080/mcp",
      "headers": { "X-Ghpool-Key": "${GHPOOL_KEY}" }
    }
  }
}
```

Deliver `GHPOOL_KEY` to the agent container via ECS task secrets / K8s Secrets — most MCP clients expand `${ENV}` in config.

Deployment notes:

- Requires egress to `api.githubcopilot.com` (the only additional external dependency).
- Run a **single replica** while MCP is enabled — session pins live in process memory. A rolling deploy terminates sessions; clients recover by re-initializing.
- Inside a trusted network, any workload that can reach `/mcp` gets the same read-only access (same trust model as ghpool's REST reads). Put TLS and agent authentication in front before any write-capable phase.
- If the hosted endpoint is unreachable from your network, point `upstream` at a self-hosted [`github-mcp-server`](https://github.com/github/github-mcp-server) instead — same protocol and headers.

### Management

| Path | Description |
|------|-------------|
| `GET /healthz` | Health check |
| `GET /stats` | Pool and cache statistics |

## How clients use it

### ghp CLI (recommended)

`ghp` is a drop-in `gh` shim that routes read commands through ghpool's REST API (pooled + cached) and falls through to the real `gh` for writes.

```sh
export GHPOOL_URL=http://ghpool.openab.local:8080

# Reads — through ghpool (pooled + cached)
ghp api repos/org/repo --jq .stargazers_count
ghp issue list -R org/repo -L 10
ghp pr list -R org/repo
ghp pr view 123 -R org/repo
ghp run list -R org/repo

# Writes — falls through to real gh (direct to GitHub)
ghp issue create -R org/repo -t "title" -b "body"
ghp issue comment 123 -R org/repo -b "comment"
ghp pr create -R org/repo -t "title" -b "body"
```

To replace `gh` transparently:

```sh
ln -sf $(which ghp) ~/bin/gh
export PATH=~/bin:$PATH
```

### gh CLI

```sh
export GITHUB_API_URL=http://localhost:8080
```

REST calls (`gh api repos/...`) route through ghpool. Note: `gh` CLI's built-in commands (`gh issue list`, `gh pr list`) use GraphQL internally and bypass `GITHUB_API_URL` — use `ghp` for full coverage.

### Coding agents

Set the GitHub API base URL to point at ghpool:

```sh
export GITHUB_API_BASE=http://localhost:8080
```

### MCP clients (agents)

Point any Streamable-HTTP MCP client at ghpool — no GitHub token, no `gh` CLI, no git credentials in the agent container.

Kiro CLI (`~/.kiro/settings/mcp.json`) and most JSON-configured clients:

```json
{
  "mcpServers": {
    "github": {
      "url": "http://ghpool.<namespace>:8080/mcp"
    }
  }
}
```

Claude Code:

```sh
claude mcp add --transport http github http://ghpool.<namespace>:8080/mcp
```

Verify from the container (no `Authorization` header anywhere):

```sh
curl -s -X POST http://ghpool:8080/mcp \
  -H "Content-Type: application/json" \
  -H "Accept: application/json, text/event-stream" \
  -d '{"jsonrpc":"2.0","id":0,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"probe","version":"0"}}}' -i | grep -i mcp-session-id
```

In Phase 2, agents will additionally present a ghpool API key (`X-Ghpool-Key` header from an env-injected secret) mapped to a per-agent tool/repo allowlist — see [#17](https://github.com/openabdev/ghpool/issues/17).

### Direct curl

```sh
# REST
curl http://localhost:8080/repos/org/repo/pulls/123

# GraphQL query
curl -X POST http://localhost:8080/graphql \
  -H "Content-Type: application/json" \
  -d '{"query":"query { repository(owner:\"org\", name:\"repo\") { stargazerCount }}"}'

# GraphQL mutation (requires your own auth)
curl -X POST http://localhost:8080/graphql \
  -H "Content-Type: application/json" \
  -H "Authorization: Bearer ghp_your_token" \
  -d '{"query":"mutation { addStar(input:{starrableId:\"...\"}) { clientMutationId }}"}'
```

## License

MIT
