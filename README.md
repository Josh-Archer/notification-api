# Notification API

A Rust-based notification API with Docker deployment and automated versioning.

## Features
- Written in Rust using Actix Web
- Sends notifications via Pushover, ntfy, or both
- `GET /healthz` reports liveness, uptime, enabled backends, and monitor counts (no names)
- Multiple named heartbeat monitors, each with its own timeout, messages, and optional secret
- Heartbeat watcher with **outage** and **recovery** alerts, plus an explicit `/fail` signal
- Automated Docker builds and publishing via GitHub Actions
- Versioning is managed automatically (minor version bump on push)
- Uses [lefthook](https://github.com/evilmartians/lefthook) for Git hooks

## Heartbeat outage & recovery

The service exposes `GET|POST /heartbeat/{name}` for each configured monitor; `/heartbeat/poop` is the default monitor. A background task watches each monitor's last heartbeat time:

| Event | Condition | Notification |
|-------|-----------|--------------|
| **Outage** | No heartbeat for longer than the monitor's timeout (`HEARTBEAT_TIMEOUT_SECS` for `poop`) |
| **Outage (explicit)** | `POST /heartbeat/{name}/fail` | The request body if non-empty (trimmed, max 500 chars), otherwise the monitor's outage message, sent immediately | `OUTAGE_MESSAGE` (with measured downtime, e.g. `(down for 100s)`) on every enabled backend |
| **Recovery** | First heartbeat after an outage alert was sent | `RECOVERY_MESSAGE` on every enabled backend |

Recovery fires **only** after a prior outage alert (not on ordinary heartbeats or startup). While still offline, outage alerts may repeat after `DEBOUNCE_SECS`.

### Named monitors

The `poop` monitor always exists and takes its settings from `HEARTBEAT_TIMEOUT_SECS`, `DEBOUNCE_SECS`, `OUTAGE_MESSAGE` and `RECOVERY_MESSAGE`, so existing deployments behave exactly as before. Add more monitors with `MONITORS_JSON` and/or `MONITOR_<NAME>_*` variables:

```bash
MONITORS_JSON='{"backup":{"timeout_secs":93600,"secret":"..."},"cleanarr":{"timeout_secs":7200,"outage_message":"Cleanarr job missed"}}'
MONITOR_BACKUP_TIMEOUT_SECS=90000   # overrides MONITORS_JSON
MONITOR_CLEANARR_RECOVERY_MESSAGE="Cleanarr is back"
```

- JSON fields: `timeout_secs`, `debounce_secs`, `outage_message`, `recovery_message`, `secret`. Precedence per field: defaults < `MONITORS_JSON` < `MONITOR_<NAME>_*`. Environment variables: `_TIMEOUT_SECS`, `_DEBOUNCE_SECS`, `_OUTAGE_MESSAGE`, `_RECOVERY_MESSAGE`, `_SECRET`. `poop` can be overridden the same way.
- Names are case-insensitive and `-` equals `_` (`cleanarr-job` is `MONITOR_CLEANARR_JOB_*`). Allowed characters are letters, digits, `_` and `-`.
- A monitor that does not set a timeout or debounce inherits `HEARTBEAT_TIMEOUT_SECS` / `DEBOUNCE_SECS`. Its default messages name the monitor.
- Each request needs `Authorization: Bearer <secret>`: the monitor's own `secret` if set, otherwise `HEARTBEAT_SECRET`.
- A wrong or missing secret returns `401`, checked before the name is looked up, so unauthenticated callers cannot probe for names. With a valid default secret, an unknown name returns `404`; monitors are never created on demand.
- `/fail` does not touch the last-heartbeat time. The next successful ping on that monitor sends its recovery message. It returns `502` if no alert could be delivered. `/fail` is POST only (GET matches no route and returns `404`). It shares the monitor's `DEBOUNCE_SECS` window: a repeat inside the window of the last outage alert is not sent and returns `202 suppressed`. A failed delivery does not start the window, and a recovery heartbeat clears it.

## Getting Started

### Prerequisites
- Rust (https://rustup.rs/)
- Docker
- Git
- Homebrew (for macOS, to install lefthook)

### Local Development
1. Clone the repository:
   ```bash
   git clone <repo-url>
   cd notification-api
   ```
2. Copy `.env.example` to `.env` and fill in non-sensitive config. Secrets should be set via environment variables or GitHub secrets.
3. Build and run:
   ```bash
   cargo build --release
   cargo run
   ```

### Docker
The maintained prebuilt image is published to GHCR:
```bash
docker pull ghcr.io/josh-archer/notification-api:latest
docker run --env-file .env ghcr.io/josh-archer/notification-api:latest
```

For a local build:
```bash
docker build -t notification-api .
docker run --env-file .env notification-api
```

### Automated Versioning with Lefthook
This project uses [lefthook](https://github.com/evilmartians/lefthook) to automatically bump the minor version in `Cargo.toml` and amend the last commit before each push.

#### Setup Lefthook
1. Install lefthook:
   ```bash
   brew install lefthook
   ```
2. Install hooks:
   ```bash
   lefthook install
   ```
3. Make sure the hook script is executable:
   ```bash
   chmod +x scripts/bump_minor.sh
   ```

Now, every time you push, lefthook will bump the minor version and amend your commit.

### GitHub Actions
- On every push to `main` (and via `workflow_dispatch`), the workflow:
  - Tags the commit with the current Cargo.toml version (if missing)
  - Builds a multi-arch image (`linux/amd64`, `linux/arm64`)
  - Publishes version and `latest` tags to GHCR: `ghcr.io/josh-archer/notification-api`
- **GHCR is the permanent and only supported publication registry.** The workflow does not publish to Docker Hub and does not require Docker Hub credentials. Existing Docker Hub copies, if any, are legacy and will not receive updates; consumers should use the GHCR image above.
- Pull requests and pushes to `main` also run `.github/workflows/ci.yml`, which tests and smoke-starts the release binary. That job does not publish an image.
- Required secrets:
  - `PUSHOVER_TOKEN`, `PUSHOVER_USER` (app runtime/config used by CI env)

### Environment Variables
- Sensitive values (e.g., `PUSHOVER_TOKEN`, `PUSHOVER_USER`) should be set as GitHub secrets or environment variables, not in `.env`.
- Non-sensitive config (e.g., timeouts) can be set in `.env`.

| Variable | Required | Default | Description |
|----------|----------|---------|-------------|
| `PUSHOVER_TOKEN` | with `PUSHOVER_USER`, unless ntfy is set | — | Pushover application token. Blank is ignored |
| `PUSHOVER_USER` | with `PUSHOVER_TOKEN`, unless ntfy is set | — | Pushover user/group key. Blank is ignored |
| `PUSHOVER_URL` | no | `https://api.pushover.net/1/messages.json` | Pushover messages endpoint. Point this at a local fixture in tests. |
| `PUSHOVER_TITLE` | no | — | Optional Pushover notification title |
| `PUSHOVER_PRIORITY` | no | — | Optional Pushover notification priority (-2 to 2); values outside this range fail startup |
| `PUSHOVER_RETRY` | no | `60` | Emergency priority retry interval in seconds; sent only with priority `2` and clamped to `30..=10800` |
| `PUSHOVER_EXPIRE` | no | `3600` | Emergency priority expiration in seconds; sent only with priority `2`, capped at `10800` and raised to at least retry |
| `PUSHOVER_SOUND` | no | — | Optional Pushover notification sound |
| `PUSHOVER_DEVICE` | no | — | Optional Pushover device name |
| `NTFY_TOPIC` | unless Pushover is set | — | ntfy topic. Enables ntfy, including ntfy-only mode |
| `NTFY_SERVER` | no | `https://ntfy.sh` | ntfy base URL, no trailing slash required |
| `NTFY_TOKEN` | no | — | Optional ntfy bearer token |
| `HEARTBEAT_SECRET` | yes | — | Default secret required as `Authorization: Bearer <secret>` on heartbeat routes (monitors may override with their own secret) |
| `HEARTBEAT_TIMEOUT_SECS` | no | `90` | Seconds without a heartbeat before outage alert (`poop` monitor; default for the others) |
| `CHECK_INTERVAL_SECS` | no | `10` | How often to check staleness |
| `DEBOUNCE_SECS` | no | `300` | Minimum gap between repeat outage alerts. Does not reset measured downtime |
| `OUTAGE_MESSAGE` | no | `❌ Poop Monitor is offline!` | Outage text (appended with measured downtime, e.g. `(down for 100s)`) |
| `RECOVERY_MESSAGE` | no | `✅ Poop Monitor is back online!` | Text when heartbeats resume after an outage alert |
| `MONITORS_JSON` | no | — | JSON object of extra monitors, see [Named monitors](#named-monitors) |
| `MONITOR_<NAME>_TIMEOUT_SECS` / `_DEBOUNCE_SECS` / `_OUTAGE_MESSAGE` / `_RECOVERY_MESSAGE` / `_SECRET` | no | — | Per-monitor settings |
| `PORT` | no | `3000` | HTTP port (`GET /healthz`, `/heartbeat/{name}`) |

`GET /healthz` returns JSON: `status`, `uptime_secs`, `backends` (`pushover` and/or `ntfy`), `last_heartbeat_secs_ago` (the `poop` monitor's age, kept for compatibility), `monitors_total`, and `monitors_down` (monitors with an alerted outage or past their timeout). Monitor names are not listed because the endpoint is unauthenticated. Heartbeat routes require the `Authorization: Bearer <secret>` header; the process exits at startup if `HEARTBEAT_SECRET` is unset or blank.

### Tests
```bash
cargo test --locked
cargo build --release --locked
bash scripts/smoke-entrypoint.sh target/release/notification-api
```

Unit tests cover the outage/recovery state machine (recovery only after a prior outage alert, independent heartbeat and alert timestamps) and Pushover-only, ntfy-only, and combined delivery against synthetic endpoints. `GET /healthz` is exercised on the same application factory the binary serves. Multi-monitor tests cover two monitors, per-monitor secrets, unknown-name 401/404 (authentication first), and the `/fail` path.

`scripts/smoke-entrypoint.sh` starts the shipped binary (the file the Dockerfile copies) with synthetic configuration and a local fixture. It checks `/healthz`, `/heartbeat/poop`, a second monitor with its own secret, unknown-name 401/404 (authentication first), `/fail` alerts and recovery, Pushover-only, ntfy-only, and combined backend selection, startup failures, a refused send, and clean `SIGTERM` shutdown. CI runs that script on `target/release/notification-api`. The reusable pattern is `docs/smoke-test-pattern.md`. Other repositories track their own adoption.

## Contributing
Pull requests are welcome! For major changes, please open an issue first to discuss what you would like to change.

## License
MIT
