#!/usr/bin/env bash
# Smoke-test the shipped notification-api executable.
#
# Starts that binary (not the library's in-process test factory) with synthetic
# configuration and a local notification fixture. See docs/smoke-test-pattern.md.
#
# Usage: scripts/smoke-entrypoint.sh /path/to/notification-api

set -euo pipefail

if [[ $# -ne 1 ]]; then
  echo "usage: $0 /path/to/notification-api" >&2
  exit 2
fi

for tool in python3 curl timeout; do
  if ! command -v "$tool" >/dev/null 2>&1; then
    echo "required tool not found: $tool" >&2
    exit 1
  fi
done

SCRIPT_DIR=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
FIXTURE_PY="${SCRIPT_DIR}/smoke_notification_fixture.py"
BIN=$1
if [[ "$BIN" != /* ]]; then
  BIN="$(pwd)/${BIN}"
fi
if [[ ! -x "$BIN" && ! -f "$BIN" ]]; then
  echo "binary not found: $BIN" >&2
  exit 1
fi

WORKDIR=$(mktemp -d)
SERVER_PIDS=()
FIXTURE_PID=""

cleanup() {
  local status=$?
  local pid
  for pid in "${SERVER_PIDS[@]+"${SERVER_PIDS[@]}"}"; do
    if [[ -n "$pid" ]] && kill -0 "$pid" 2>/dev/null; then
      kill -TERM "$pid" 2>/dev/null || true
    fi
  done
  if [[ -n "${FIXTURE_PID}" ]] && kill -0 "$FIXTURE_PID" 2>/dev/null; then
    kill -TERM "$FIXTURE_PID" 2>/dev/null || true
  fi
  sleep 0.2
  for pid in "${SERVER_PIDS[@]+"${SERVER_PIDS[@]}"}" ${FIXTURE_PID}; do
    if [[ -n "$pid" ]] && kill -0 "$pid" 2>/dev/null; then
      kill -KILL "$pid" 2>/dev/null || true
    fi
  done
  rm -rf "$WORKDIR"
  exit "$status"
}
trap cleanup EXIT

cd "$WORKDIR"
FIXTURE_LOG="${WORKDIR}/fixture.jsonl"

die() {
  echo "FAIL: $*" >&2
  exit 1
}

dump_log() {
  local log=$1
  if [[ -f "$log" ]]; then
    echo "----- $log -----" >&2
    cat "$log" >&2
    echo "----- end -----" >&2
  fi
}

free_port() {
  python3 -c 'import socket; s=socket.socket(); s.bind(("127.0.0.1", 0)); print(s.getsockname()[1]); s.close()'
}

# Clear inherited notification settings so a developer environment cannot leak
# into the smoke process or turn a failure case into a real send.
launch_server() {
  local log=$1
  shift
  env \
    -u PUSHOVER_TOKEN \
    -u PUSHOVER_USER \
    -u PUSHOVER_URL \
    -u OUTAGE_MESSAGE \
    -u RECOVERY_MESSAGE \
    -u HEARTBEAT_TIMEOUT_SECS \
    -u CHECK_INTERVAL_SECS \
    -u DEBOUNCE_SECS \
    -u PORT \
    -u NTFY_TOPIC \
    -u NTFY_SERVER \
    -u NTFY_TOKEN \
    -u HEARTBEAT_SECRET \
    -u RUST_LOG \
    RUST_LOG=info \
    HEARTBEAT_SECRET="$HEARTBEAT_SECRET_VALUE" \
    "$@" \
    "$BIN" >"$log" 2>&1 &
  LAUNCHED_PID=$!
  SERVER_PIDS+=("$LAUNCHED_PID")
}

expect_startup_failure() {
  local label=$1
  local needle=$2
  shift 2
  local log="${WORKDIR}/${label}.log"
  set +e
  timeout 8s env \
    -u PUSHOVER_TOKEN \
    -u PUSHOVER_USER \
    -u PUSHOVER_URL \
    -u OUTAGE_MESSAGE \
    -u RECOVERY_MESSAGE \
    -u HEARTBEAT_TIMEOUT_SECS \
    -u CHECK_INTERVAL_SECS \
    -u DEBOUNCE_SECS \
    -u PORT \
    -u NTFY_TOPIC \
    -u NTFY_SERVER \
    -u NTFY_TOKEN \
    -u HEARTBEAT_SECRET \
    RUST_LOG=info \
    HEARTBEAT_SECRET="$HEARTBEAT_SECRET_VALUE" \
    "$@" \
    "$BIN" >"$log" 2>&1
  local code=$?
  set -e
  if [[ "$code" -eq 0 || "$code" -eq 124 ]]; then
    dump_log "$log"
    die "${label}: expected a startup failure, got exit ${code}"
  fi
  if ! grep -q "$needle" "$log"; then
    dump_log "$log"
    die "${label}: stderr missing '${needle}'"
  fi
  echo "ok startup failure: ${label} (exit ${code})"
}

wait_for_log() {
  local log=$1
  local needle=$2
  local pid=$3
  local i
  for ((i = 0; i < 75; i++)); do
    if [[ -f "$log" ]] && grep -q "$needle" "$log"; then
      return 0
    fi
    if ! kill -0 "$pid" 2>/dev/null; then
      dump_log "$log"
      die "process ${pid} exited while waiting for '${needle}'"
    fi
    sleep 0.2
  done
  dump_log "$log"
  die "timed out waiting for '${needle}'"
}

wait_for_http() {
  local url=$1
  local pid=$2
  local log=$3
  local i
  for ((i = 0; i < 75; i++)); do
    if curl -fsS --max-time 1 "$url" >/dev/null 2>"${WORKDIR}/curl.err"; then
      return 0
    fi
    if ! kill -0 "$pid" 2>/dev/null; then
      dump_log "$log"
      die "process ${pid} exited while waiting for ${url}"
    fi
    sleep 0.2
  done
  dump_log "$log"
  if [[ -f "${WORKDIR}/curl.err" ]]; then
    cat "${WORKDIR}/curl.err" >&2
  fi
  die "timed out waiting for ${url}"
}

stop_expect_clean() {
  local pid=$1
  local log=$2
  kill -TERM "$pid" || die "failed to signal ${pid}"
  local i code
  for ((i = 0; i < 50; i++)); do
    if ! kill -0 "$pid" 2>/dev/null; then
      break
    fi
    sleep 0.2
  done
  if kill -0 "$pid" 2>/dev/null; then
    dump_log "$log"
    kill -KILL "$pid" 2>/dev/null || true
    die "process ${pid} ignored SIGTERM"
  fi
  set +e
  wait "$pid"
  code=$?
  set -e
  if [[ "$code" -ne 0 ]]; then
    dump_log "$log"
    die "SIGTERM exit code ${code} (want 0)"
  fi
  echo "ok clean shutdown (exit 0)"
}

assert_health() {
  local port=$1
  local expected_backends=$2
  local body
  if ! body=$(curl -fsS --max-time 2 -H "Authorization: Bearer ${HEARTBEAT_SECRET_VALUE}" "http://127.0.0.1:${port}/healthz"); then
    die "GET /healthz failed"
  fi
  BODY="$body" EXPECTED_BACKENDS="$expected_backends" python3 - <<'PY'
import json, os, sys
body = json.loads(os.environ["BODY"])
expected = json.loads(os.environ["EXPECTED_BACKENDS"])
if body.get("status") != "ok":
    sys.exit(f"status: {body}")
if body.get("backends") != expected:
    sys.exit(f"backend selection: {body} expected {expected}")
for key in ("uptime_secs", "last_heartbeat_secs_ago"):
    value = body.get(key)
    if not isinstance(value, int) or value < 0:
        sys.exit(f"{key}: {body}")
if body["last_heartbeat_secs_ago"] >= 60:
    sys.exit(f"stale heartbeat age: {body}")
print("ok healthz", json.dumps(body, separators=(",", ":")))
PY
}

echo "smoke binary: ${BIN}"

# GET /heartbeat/poop requires a shared bearer secret (#29).
HEARTBEAT_SECRET_VALUE=REPLACE_ME_SMOKE_SECRET
FIXTURE_PORT=$(free_port)
python3 "$FIXTURE_PY" "$FIXTURE_PORT" "$FIXTURE_LOG" >"${WORKDIR}/fixture.stdout" 2>"${WORKDIR}/fixture.stderr" &
FIXTURE_PID=$!
fixture_ready=0
for ((i = 0; i < 50; i++)); do
  if curl -s -o /dev/null --max-time 1 "http://127.0.0.1:${FIXTURE_PORT}/" 2>/dev/null; then
    fixture_ready=1
    break
  fi
  if ! kill -0 "$FIXTURE_PID" 2>/dev/null; then
    break
  fi
  sleep 0.1
done
if [[ "$fixture_ready" -ne 1 ]]; then
  dump_log "${WORKDIR}/fixture.stderr"
  die "notification fixture failed to listen on ${FIXTURE_PORT}"
fi
echo "ok fixture on 127.0.0.1:${FIXTURE_PORT}"

# --- startup failures (no listener left behind) ---
# Blank Pushover values count as unset. The image ships those blanks.
expect_startup_failure no-backend "At least one notification backend"
expect_startup_failure blank-pushover "At least one notification backend" \
  PUSHOVER_TOKEN= \
  PUSHOVER_USER=
expect_startup_failure half-token "Both PUSHOVER_TOKEN and PUSHOVER_USER must be set" \
  PUSHOVER_USER=REPLACE_ME
expect_startup_failure half-user "Both PUSHOVER_TOKEN and PUSHOVER_USER must be set" \
  PUSHOVER_TOKEN=REPLACE_ME
expect_startup_failure bad-timeout "HEARTBEAT_TIMEOUT_SECS must be a number" \
  PUSHOVER_TOKEN=REPLACE_ME \
  PUSHOVER_USER=REPLACE_ME \
  HEARTBEAT_TIMEOUT_SECS=nope
expect_startup_failure bad-interval "CHECK_INTERVAL_SECS must be a number" \
  PUSHOVER_TOKEN=REPLACE_ME \
  PUSHOVER_USER=REPLACE_ME \
  CHECK_INTERVAL_SECS=nope
expect_startup_failure bad-debounce "DEBOUNCE_SECS must be a number" \
  PUSHOVER_TOKEN=REPLACE_ME \
  PUSHOVER_USER=REPLACE_ME \
  DEBOUNCE_SECS=nope
expect_startup_failure no-heartbeat-secret "HEARTBEAT_SECRET must be set" \
  PUSHOVER_TOKEN=REPLACE_ME \
  PUSHOVER_USER=REPLACE_ME \
  HEARTBEAT_SECRET=
expect_startup_failure bad-port "PORT must be a number" \
  PUSHOVER_TOKEN=REPLACE_ME \
  PUSHOVER_USER=REPLACE_ME \
  PORT=nope

# --- routes, backend name, and clean shutdown; watcher must not notify ---
ROUTES_PORT=$(free_port)
ROUTES_LOG="${WORKDIR}/routes.log"
launch_server "$ROUTES_LOG" \
  PUSHOVER_TOKEN=REPLACE_ME \
  PUSHOVER_USER=REPLACE_ME \
  PUSHOVER_URL="http://127.0.0.1:${FIXTURE_PORT}/fixture/pushover" \
  PORT="$ROUTES_PORT" \
  HEARTBEAT_TIMEOUT_SECS=86400 \
  CHECK_INTERVAL_SECS=3600 \
  DEBOUNCE_SECS=3600 \
  OUTAGE_MESSAGE=OUTAGE_FIXTURE_OK \
  RECOVERY_MESSAGE=RECOVERY_FIXTURE_OK
ROUTES_PID=$LAUNCHED_PID
wait_for_log "$ROUTES_LOG" "Listening on 0.0.0.0:${ROUTES_PORT}" "$ROUTES_PID"
wait_for_http "http://127.0.0.1:${ROUTES_PORT}/healthz" "$ROUTES_PID" "$ROUTES_LOG"
assert_health "$ROUTES_PORT" '["pushover"]'

heartbeat_body=$(curl -fsS --max-time 2 -H "Authorization: Bearer ${HEARTBEAT_SECRET_VALUE}" "http://127.0.0.1:${ROUTES_PORT}/heartbeat/poop")
if [[ "$heartbeat_body" != "OK" ]]; then
  dump_log "$ROUTES_LOG"
  die "GET /heartbeat/poop body was '${heartbeat_body}'"
fi
echo "ok heartbeat"
unauth_code=$(curl -s -o /dev/null -w '%{http_code}' --max-time 2 "http://127.0.0.1:${ROUTES_PORT}/heartbeat/poop")
if [[ "$unauth_code" != "401" ]]; then
  dump_log "$ROUTES_LOG"
  die "GET /heartbeat/poop without the secret returned ${unauth_code}, expected 401"
fi
echo "ok heartbeat rejects missing secret"
assert_health "$ROUTES_PORT" '["pushover"]'

# Second process on the same port must fail closed.
expect_startup_failure port-in-use "Address already in use" \
  PUSHOVER_TOKEN=REPLACE_ME \
  PUSHOVER_USER=REPLACE_ME \
  PUSHOVER_URL="http://127.0.0.1:${FIXTURE_PORT}/fixture/pushover" \
  PORT="$ROUTES_PORT" \
  HEARTBEAT_TIMEOUT_SECS=86400 \
  CHECK_INTERVAL_SECS=3600 \
  DEBOUNCE_SECS=3600

stop_expect_clean "$ROUTES_PID" "$ROUTES_LOG"
if [[ -s "$FIXTURE_LOG" ]]; then
  dump_log "$FIXTURE_LOG"
  die "healthy smoke sent a notification"
fi
echo "ok no notification while healthy"

# --- backend selection: outage and recovery hit only the configured fixture ---
DELIVERY_PORT=$(free_port)
DELIVERY_LOG="${WORKDIR}/delivery.log"
launch_server "$DELIVERY_LOG" \
  PUSHOVER_TOKEN=REPLACE_ME \
  PUSHOVER_USER=REPLACE_ME \
  PUSHOVER_URL="http://127.0.0.1:${FIXTURE_PORT}/fixture/pushover" \
  PORT="$DELIVERY_PORT" \
  HEARTBEAT_TIMEOUT_SECS=0 \
  CHECK_INTERVAL_SECS=1 \
  DEBOUNCE_SECS=3600 \
  OUTAGE_MESSAGE=OUTAGE_FIXTURE_OK \
  RECOVERY_MESSAGE=RECOVERY_FIXTURE_OK
DELIVERY_PID=$LAUNCHED_PID
wait_for_http "http://127.0.0.1:${DELIVERY_PORT}/healthz" "$DELIVERY_PID" "$DELIVERY_LOG"
assert_health "$DELIVERY_PORT" '["pushover"]'

delivery_ready=0
for ((i = 0; i < 75; i++)); do
  if [[ -f "$FIXTURE_LOG" ]] && grep -q "OUTAGE_FIXTURE_OK" "$FIXTURE_LOG"; then
    delivery_ready=1
    break
  fi
  if ! kill -0 "$DELIVERY_PID" 2>/dev/null; then
    dump_log "$DELIVERY_LOG"
    die "delivery process exited before the outage post"
  fi
  sleep 0.2
done
if [[ "$delivery_ready" -ne 1 ]]; then
  dump_log "$DELIVERY_LOG"
  dump_log "$FIXTURE_LOG"
  die "fixture never received the outage alert"
fi

recovery_body=$(curl -fsS --max-time 2 -H "Authorization: Bearer ${HEARTBEAT_SECRET_VALUE}" "http://127.0.0.1:${DELIVERY_PORT}/heartbeat/poop")
if [[ "$recovery_body" != "OK" ]]; then
  dump_log "$DELIVERY_LOG"
  die "recovery heartbeat body was '${recovery_body}'"
fi

python3 - "$FIXTURE_LOG" <<'PY'
import json
import sys
from urllib.parse import parse_qs

rows = []
with open(sys.argv[1], encoding="utf-8") as handle:
    for line in handle:
        line = line.strip()
        if line:
            rows.append(json.loads(line))
if len(rows) != 2:
    sys.exit(f"expected 2 fixture posts, got {len(rows)}: {rows}")
messages = []
for row in rows:
    if row.get("path") != "/fixture/pushover":
        sys.exit(f"backend URL was not selected, path={row.get('path')!r}")
    form = parse_qs(row.get("body", ""), keep_blank_values=True)
    token = form.get("token", [""])[0]
    user = form.get("user", [""])[0]
    if token != "REPLACE_ME" or user != "REPLACE_ME":
        sys.exit("fixture post did not use the synthetic credentials")
    messages.append(form.get("message", [""])[0])
outage = [item for item in messages if item.startswith("OUTAGE_FIXTURE_OK")]
recovery = [item for item in messages if item == "RECOVERY_FIXTURE_OK"]
if len(outage) != 1 or len(recovery) != 1:
    sys.exit(f"expected one outage and one recovery, got {messages}")
if "down for " not in outage[0]:
    sys.exit(f"outage message missing measured downtime: {outage[0]}")
print("ok backend delivery", messages)
PY

if grep -q "api.pushover.net" "$DELIVERY_LOG" "$ROUTES_LOG"; then
  die "smoke log mentioned the real Pushover host"
fi
stop_expect_clean "$DELIVERY_PID" "$DELIVERY_LOG"

# --- ntfy-only: blank Pushover must not block startup; posts stay on the fixture ---
: >"$FIXTURE_LOG"
NTFY_PORT=$(free_port)
NTFY_LOG="${WORKDIR}/ntfy.log"
launch_server "$NTFY_LOG" \
  PUSHOVER_TOKEN= \
  PUSHOVER_USER= \
  NTFY_TOPIC=solo \
  NTFY_SERVER="http://127.0.0.1:${FIXTURE_PORT}" \
  PORT="$NTFY_PORT" \
  HEARTBEAT_TIMEOUT_SECS=0 \
  CHECK_INTERVAL_SECS=1 \
  DEBOUNCE_SECS=3600 \
  OUTAGE_MESSAGE=OUTAGE_FIXTURE_OK \
  RECOVERY_MESSAGE=RECOVERY_FIXTURE_OK
NTFY_PID=$LAUNCHED_PID
wait_for_http "http://127.0.0.1:${NTFY_PORT}/healthz" "$NTFY_PID" "$NTFY_LOG"
assert_health "$NTFY_PORT" '["ntfy"]'
ntfy_ready=0
for ((i = 0; i < 75; i++)); do
  if [[ -f "$FIXTURE_LOG" ]] && grep -q "OUTAGE_FIXTURE_OK" "$FIXTURE_LOG"; then
    ntfy_ready=1
    break
  fi
  if ! kill -0 "$NTFY_PID" 2>/dev/null; then
    dump_log "$NTFY_LOG"
    die "ntfy-only process exited before the outage post"
  fi
  sleep 0.2
done
if [[ "$ntfy_ready" -ne 1 ]]; then
  dump_log "$NTFY_LOG"
  dump_log "$FIXTURE_LOG"
  die "fixture never received the ntfy outage alert"
fi
ntfy_beat=$(curl -fsS --max-time 2 -H "Authorization: Bearer ${HEARTBEAT_SECRET_VALUE}" "http://127.0.0.1:${NTFY_PORT}/heartbeat/poop")
if [[ "$ntfy_beat" != "OK" ]]; then
  dump_log "$NTFY_LOG"
  die "ntfy-only heartbeat body was '${ntfy_beat}'"
fi
python3 - "$FIXTURE_LOG" <<'PY'
import json
import sys

rows = []
with open(sys.argv[1], encoding="utf-8") as handle:
    for line in handle:
        line = line.strip()
        if line:
            rows.append(json.loads(line))
if len(rows) != 2:
    sys.exit(f"expected 2 ntfy posts, got {len(rows)}: {rows}")
messages = []
for row in rows:
    if row.get("path") != "/solo":
        sys.exit(f"ntfy URL was not selected, path={row.get('path')!r}")
    messages.append(row.get("body", ""))
outage = [item for item in messages if item.startswith("OUTAGE_FIXTURE_OK")]
recovery = [item for item in messages if item == "RECOVERY_FIXTURE_OK"]
if len(outage) != 1 or len(recovery) != 1:
    sys.exit(f"expected one ntfy outage and one recovery, got {messages}")
if "down for " not in outage[0]:
    sys.exit(f"ntfy outage missing measured downtime: {outage[0]}")
print("ok ntfy-only delivery", messages)
PY
if grep -q "ntfy.sh" "$NTFY_LOG"; then
  die "ntfy-only log mentioned the public ntfy host"
fi
stop_expect_clean "$NTFY_PID" "$NTFY_LOG"

# --- both backends selected, and a healthy process does not notify ---
: >"$FIXTURE_LOG"
BOTH_PORT=$(free_port)
BOTH_LOG="${WORKDIR}/both.log"
launch_server "$BOTH_LOG" \
  PUSHOVER_TOKEN=REPLACE_ME \
  PUSHOVER_USER=REPLACE_ME \
  PUSHOVER_URL="http://127.0.0.1:${FIXTURE_PORT}/fixture/pushover" \
  NTFY_TOPIC=combo \
  NTFY_SERVER="http://127.0.0.1:${FIXTURE_PORT}" \
  PORT="$BOTH_PORT" \
  HEARTBEAT_TIMEOUT_SECS=86400 \
  CHECK_INTERVAL_SECS=3600 \
  DEBOUNCE_SECS=3600
BOTH_PID=$LAUNCHED_PID
wait_for_http "http://127.0.0.1:${BOTH_PORT}/healthz" "$BOTH_PID" "$BOTH_LOG"
assert_health "$BOTH_PORT" '["pushover","ntfy"]'
both_beat=$(curl -fsS --max-time 2 -H "Authorization: Bearer ${HEARTBEAT_SECRET_VALUE}" "http://127.0.0.1:${BOTH_PORT}/heartbeat/poop")
if [[ "$both_beat" != "OK" ]]; then
  dump_log "$BOTH_LOG"
  die "combined heartbeat body was '${both_beat}'"
fi
if [[ -s "$FIXTURE_LOG" ]]; then
  dump_log "$FIXTURE_LOG"
  die "combined healthy process sent a notification"
fi
echo "ok combined backends"
stop_expect_clean "$BOTH_PID" "$BOTH_LOG"

# --- failure behavior: a refused local endpoint does not kill the process ---
FAILURE_PORT=$(free_port)
FAILURE_LOG="${WORKDIR}/send-failure.log"
launch_server "$FAILURE_LOG" \
  PUSHOVER_TOKEN=REPLACE_ME \
  PUSHOVER_USER=REPLACE_ME \
  PUSHOVER_URL="http://127.0.0.1:1/fixture/pushover" \
  PORT="$FAILURE_PORT" \
  HEARTBEAT_TIMEOUT_SECS=0 \
  CHECK_INTERVAL_SECS=1 \
  DEBOUNCE_SECS=3600
FAILURE_PID=$LAUNCHED_PID
wait_for_http "http://127.0.0.1:${FAILURE_PORT}/healthz" "$FAILURE_PID" "$FAILURE_LOG"
assert_health "$FAILURE_PORT" '["pushover"]'
wait_for_log "$FAILURE_LOG" "Failed to send Pushover" "$FAILURE_PID"
if ! kill -0 "$FAILURE_PID" 2>/dev/null; then
  dump_log "$FAILURE_LOG"
  die "process exited after a notification send failure"
fi
still_ok=$(curl -fsS --max-time 2 -H "Authorization: Bearer ${HEARTBEAT_SECRET_VALUE}" "http://127.0.0.1:${FAILURE_PORT}/heartbeat/poop")
if [[ "$still_ok" != "OK" ]]; then
  dump_log "$FAILURE_LOG"
  die "heartbeat failed after a notification send failure"
fi
echo "ok send failure kept the process up"
stop_expect_clean "$FAILURE_PID" "$FAILURE_LOG"

echo "smoke-entrypoint: ok"
