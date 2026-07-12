#!/usr/bin/env bash
# E2E test: ghpool MCP reverse proxy against GitHub's hosted MCP server.
#
# Requires:
#   GITHUB_TOKEN  — a user token (PAT or `gh auth token`); the Actions built-in
#                   installation token is NOT accepted by the hosted MCP server.
#   GHPOOL_BIN    — path to the ghpool binary (default: ./target/debug/ghpool)
#
# Usage: GITHUB_TOKEN=$(gh auth token) ./scripts/e2e-mcp.sh
set -euo pipefail

PORT="${PORT:-18080}"
BIN="${GHPOOL_BIN:-./target/debug/ghpool}"
BASE="http://localhost:${PORT}"
WORKDIR="$(mktemp -d)"
LOG="${WORKDIR}/ghpool.log"

pass=0
fail=0
check() { # check <name> <condition-exit-code>
  if [ "$2" -eq 0 ]; then
    echo "  ✓ $1"; pass=$((pass + 1))
  else
    echo "  ✗ $1"; fail=$((fail + 1))
  fi
}

cleanup() {
  [ -n "${GHPOOL_PID:-}" ] && kill "${GHPOOL_PID}" 2>/dev/null || true
  rm -rf "${WORKDIR}"
}
trap cleanup EXIT

if [ -z "${GITHUB_TOKEN:-}" ]; then
  echo "GITHUB_TOKEN not set — skipping e2e (this is expected on forks)"
  exit 0
fi

cat > "${WORKDIR}/config.toml" <<EOF
port = ${PORT}
allowed_owners = ["openabdev"]
[[identities]]
id = "e2e"
token = "env:GITHUB_TOKEN"
[mcp]
enabled = true
EOF

echo "starting ghpool (${BIN}) on :${PORT}"
GHPOOL_CONFIG="${WORKDIR}/config.toml" "${BIN}" > "${LOG}" 2>&1 &
GHPOOL_PID=$!

for _ in $(seq 1 20); do
  curl -sf "${BASE}/healthz" > /dev/null 2>&1 && break
  sleep 0.5
done
curl -sf "${BASE}/healthz" > /dev/null || { echo "ghpool failed to start"; cat "${LOG}"; exit 1; }

JSON_H=(-H "Content-Type: application/json" -H "Accept: application/json, text/event-stream")

echo "1. initialize (no client Authorization header)"
curl -s -D "${WORKDIR}/init-headers.txt" -o "${WORKDIR}/init-body.txt" \
  -X POST "${BASE}/mcp" "${JSON_H[@]}" \
  -d '{"jsonrpc":"2.0","id":0,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"ghpool-e2e","version":"0"}}}'
grep -q "^HTTP/1.1 200" "${WORKDIR}/init-headers.txt"; check "initialize returns 200" $?
SID="$(grep -i "^mcp-session-id:" "${WORKDIR}/init-headers.txt" | tr -d '\r' | awk '{print $2}')"
[ -n "${SID}" ]; check "Mcp-Session-Id returned" $?
grep -q '"result"' "${WORKDIR}/init-body.txt"; check "initialize result frame streamed" $?

SESS_H=(-H "Mcp-Session-Id: ${SID}")

echo "2. notifications/initialized"
CODE="$(curl -s -o /dev/null -w "%{http_code}" -X POST "${BASE}/mcp" "${JSON_H[@]}" "${SESS_H[@]}" \
  -d '{"jsonrpc":"2.0","method":"notifications/initialized"}')"
[ "${CODE}" = "202" ] || [ "${CODE}" = "200" ]; check "initialized accepted (${CODE})" $?

echo "3. tools/list"
curl -s -X POST "${BASE}/mcp" "${JSON_H[@]}" "${SESS_H[@]}" \
  -d '{"jsonrpc":"2.0","id":1,"method":"tools/list"}' \
  | grep "^data:" | sed 's/^data: //' > "${WORKDIR}/tools.json"
grep -q '"issue_read"' "${WORKDIR}/tools.json"; check "read tool present (issue_read)" $?
! grep -q '"create_issue"' "${WORKDIR}/tools.json"; check "write tool absent (create_issue) — readonly enforced" $?

echo "4. tools/call issue_read on openabdev/ghpool#15"
curl -s -X POST "${BASE}/mcp" "${JSON_H[@]}" "${SESS_H[@]}" \
  -d '{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"issue_read","arguments":{"method":"get","owner":"openabdev","repo":"ghpool","issue_number":15}}}' \
  | grep "^data:" | sed 's/^data: //' > "${WORKDIR}/issue.json"
grep -q 'MCP reverse proxy' "${WORKDIR}/issue.json"; check "issue_read returned RFC #15 content" $?

echo "5. server-side behavior"
grep -q "MCP session pinned to identity e2e" "${LOG}"; check "session pinned in audit log" $?
grep -q "MCP tools/call issue_read" "${LOG}"; check "tools/call audit-logged" $?

echo
echo "e2e result: ${pass} passed, ${fail} failed"
[ "${fail}" -eq 0 ]
