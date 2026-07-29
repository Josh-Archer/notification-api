# Notification API

A Rust-based notification API with Docker deployment and automated versioning.

## Features
- Written in Rust using Actix Web
- Multi-device heartbeat monitoring via `/heartbeat/{device_id}`
- Per-device last-seen tracking and optional per-device timeouts
- Sends notifications via Pushover when a device goes silent
- Automated Docker builds and publishing via GitHub Actions
- Versioning is managed automatically (minor version bump on push)
- Uses [lefthook](https://github.com/evilmartians/lefthook) for Git hooks

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
4. Run tests:
   ```bash
   cargo test
   ```

### Heartbeat API

Devices should periodically hit the heartbeat endpoint so the API can detect outages.

```http
GET /heartbeat/{device_id}
```

- `{device_id}`: 1–64 characters; ascii letters, digits, `_`, or `-`
- **200 OK** (`OK`) — device is configured and last-seen was updated
- **400 Bad Request** — invalid device id
- **404 Not Found** — device is not listed in `HEARTBEAT_DEVICES`

#### Example

Configure two devices in `.env`:

```env
HEARTBEAT_DEVICES=poop:90,fridge:120
HEARTBEAT_TIMEOUT_SECS=90
CHECK_INTERVAL_SECS=10
DEBOUNCE_SECS=300
```

From each device (cron, script, or agent):

```bash
# every minute from the "poop" monitor
curl -fsS http://notification-api:3000/heartbeat/poop

# fridge uses a longer 120s timeout
curl -fsS http://notification-api:3000/heartbeat/fridge
```

If `poop` stops checking in for more than 90s (or `fridge` for more than 120s), the service sends a Pushover alert naming the offline device.

### Docker
Build and run the Docker container:
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
- On every push to `main`, the workflow:
  - Builds the Rust app
  - Tags the commit with the current version
  - Builds and pushes Docker images tagged with the version and `latest`
  - Loads secrets from GitHub repository secrets

### Environment Variables
- Sensitive values (e.g., `PUSHOVER_TOKEN`, `PUSHOVER_USER`) should be set as GitHub secrets or environment variables, not in `.env`.
- Non-sensitive config (timeouts, device list) can be set in `.env` (see `.env.example`).

| Variable | Default | Description |
|----------|---------|-------------|
| `PUSHOVER_TOKEN` | _(required)_ | Pushover application token |
| `PUSHOVER_USER` | _(required)_ | Pushover user/group key |
| `HEARTBEAT_TIMEOUT_SECS` | `90` | Default silence timeout (seconds) |
| `CHECK_INTERVAL_SECS` | `10` | How often to scan devices |
| `DEBOUNCE_SECS` | `300` | Wait after an alert before the next check cycle |
| `HEARTBEAT_DEVICES` | `poop` | Comma-separated `device_id[:timeout]` list |

`HEARTBEAT_DEVICES` examples:
- `poop` — one device, default timeout
- `poop:90,fridge:120,server` — per-device timeouts; `server` uses `HEARTBEAT_TIMEOUT_SECS`

## Contributing
Pull requests are welcome! For major changes, please open an issue first to discuss what you would like to change.

## License
MIT

