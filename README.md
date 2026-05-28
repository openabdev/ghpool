# ghpool

Internal GitHub API proxy with PAT pooling and caching. Designed to run as a shared service for multiple coding agents that need to read from the same GitHub repos without exhausting individual rate limits.

## What it does

- Pools multiple GitHub PATs and routes each request through the identity with the most remaining rate limit budget
- Caches GitHub API responses in memory with configurable TTLs per route type
- Mirrors the GitHub API path structure — clients just change the base URL
- Restricts access to configured org/owner repos only

## Quick start

```sh
cp config.example.yaml config.yaml
# Edit config.yaml with your PATs and allowed owners

cargo run
# Listening on 0.0.0.0:8080

curl http://localhost:8080/repos/openclaw/chi/pulls/123
curl http://localhost:8080/stats
```

## Configuration

### YAML file

Set `GHPOOL_CONFIG` env var to point to your config file (defaults to `config.yaml`).

See [config.example.yaml](config.example.yaml) for all options.

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
docker run -p 8080:8080 -v ./config.yaml:/config.yaml ghpool
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

All GitHub API GET paths are proxied transparently:

```
GET /<github-api-path>
```

Additional endpoints:

| Path | Description |
|------|-------------|
| `GET /healthz` | Health check |
| `GET /stats` | Pool and cache statistics |

## How clients use it

Set the GitHub API base URL to point at ghpool instead of `api.github.com`:

```sh
# Direct curl
curl http://ghpool:8080/repos/org/repo/pulls/123

# In agent code — just change base URL
export GITHUB_API_BASE=http://ghpool:8080
```

## License

MIT
