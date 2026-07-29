# Notification API

A Rust-based notification API with Docker deployment and automated versioning.

## Features
- Written in Rust using Actix Web
- Multi-channel notifications: **Pushover** and/or **ntfy**
- Heartbeat staleness watcher with configurable timeout/debounce
- `/healthz` process health endpoint for probes
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
2. Copy `.env.example` to `.env` and fill in config. Secrets should be set via environment variables or GitHub secrets.
3. Build and run:
   ```bash
   cargo build --release
   cargo run
   ```
4. Run tests:
   ```bash
   cargo test
   ```

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

## HTTP endpoints

| Method | Path | Description |
|--------|------|-------------|
| `GET` | `/healthz` | Process health for liveness probes (JSON) |
| `GET` | `/heartbeat/poop` | Record a heartbeat (resets staleness timer) |

### `/healthz` example response
```json
{
  "status": "ok",
  "uptime_secs": 42,
  "backends": ["pushover", "ntfy"],
  "last_heartbeat_secs_ago": 3
}
```

## Environment Variables

At least **one** notification backend must be configured.

### Notification backends

| Variable | Required | Description |
|----------|----------|-------------|
| `PUSHOVER_TOKEN` | with user | Pushover application token |
| `PUSHOVER_USER` | with token | Pushover user/group key |
| `NTFY_TOPIC` | to enable ntfy | ntfy topic name |
| `NTFY_SERVER` | no | ntfy server base URL (default `https://ntfy.sh`) |
| `NTFY_TOKEN` | no | Optional bearer token for private ntfy topics |

Sensitive values (`PUSHOVER_*`, `NTFY_TOKEN`) should be set as GitHub secrets or runtime environment variables, not committed.

### Watcher / app

| Variable | Default | Description |
|----------|---------|-------------|
| `HEARTBEAT_TIMEOUT_SECS` | `90` | Seconds without heartbeat before alerting |
| `CHECK_INTERVAL_SECS` | `10` | How often to check for staleness |
| `DEBOUNCE_SECS` | `300` | Wait after an alert before checking again |
| `RUST_LOG` | (unset) | `env_logger` filter, e.g. `info` |

### Examples

Pushover only:
```bash
export PUSHOVER_TOKEN=...
export PUSHOVER_USER=...
```

ntfy only:
```bash
export NTFY_TOPIC=home-alerts
# optional:
export NTFY_SERVER=https://ntfy.sh
export NTFY_TOKEN=tk_...
```

Both channels (alerts fan out):
```bash
export PUSHOVER_TOKEN=...
export PUSHOVER_USER=...
export NTFY_TOPIC=home-alerts
```

## Contributing
Pull requests are welcome! For major changes, please open an issue first to discuss what you would like to change.

## License
MIT
