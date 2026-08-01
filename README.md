# Notification API

A Rust-based notification API with Docker deployment and automated versioning.

## Features
- Written in Rust using Actix Web
- Sends notifications via Pushover
- Automated Docker builds and publishing via GitHub Actions
- Versioning is managed automatically (minor version bump on push to `main`)
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

### Docker
Build and run the Docker container:
```bash
docker build -t notification-api .
docker run --env-file .env notification-api
```

### Versioning workflow

Version numbers live in `Cargo.toml` and are published as Git tags (`vX.Y.Z`) and Docker image tags.

#### How automatic bumps work
- **When:** Only when pushing the local `main` branch (via a lefthook `pre-push` hook).
- **What:** The minor version is incremented (e.g. `0.2.0` → `0.3.0`) and written to `Cargo.toml`.
- **How:** A **separate commit** is created (`chore: bump version to X.Y.Z`). The hook **never amends** existing commits, so history is never rewritten.
- **Push:** Because Git has already chosen the commits for the original `git push`, the hook pushes the new tip (and tag) itself with `--no-verify`, then cancels the original push. Your code and the version commit both land on `origin/main` in one step.
- **Skip conditions:**
  - Current branch is not `main` (feature branches / PRs are untouched)
  - Latest `v*` tag already matches `Cargo.toml` and points at `HEAD`
  - Re-entrant hook runs triggered by the hook’s own push

#### PR-based collaboration (safe defaults)
- Open feature branches and PRs as usual. Pushes to non-`main` branches **do not** bump the version or create tags.
- Do **not** force-push shared branches to “include” a version bump; bumps only happen on `main` as new commits.
- After a PR is merged to `main`, the next push of `main` (or the merge push, if hooks run in that environment) performs the bump. CI also tags from `Cargo.toml` if the tag is missing.

#### Setup Lefthook (maintainers pushing to `main`)
1. Install lefthook:
   ```bash
   brew install lefthook
   ```
2. Install hooks:
   ```bash
   lefthook install
   ```
3. Ensure the bump script is executable:
   ```bash
   chmod +x scripts/bump_minor.sh
   ```

#### Manual versioning (optional)
If you need a version change without the hook:

```bash
# edit version in Cargo.toml, then:
git add Cargo.toml
git commit -m "chore: bump version to X.Y.Z"
git tag vX.Y.Z
git push origin main --tags
```

### GitHub Actions
- On every push to `main`, the workflow:
  - Builds the Rust app
  - Tags the commit with the current version from `Cargo.toml` (if the tag does not already exist)
  - Builds and pushes Docker images tagged with the version and `latest`
  - Loads secrets from GitHub repository secrets

### Environment Variables
- Sensitive values (e.g., `PUSHOVER_TOKEN`, `PUSHOVER_USER`, `HEARTBEAT_AUTH_TOKEN`) should be set as GitHub secrets or environment variables, not in `.env`.
- Non-sensitive config (e.g., timeouts) can be set in `.env`.

| Variable | Required | Default | Description |
|----------|----------|---------|-------------|
| `PUSHOVER_TOKEN` | yes | — | Pushover application token |
| `PUSHOVER_USER` | yes | — | Pushover user/group key |
| `HEARTBEAT_TIMEOUT_SECS` | no | `90` | Seconds without heartbeat before alerting |
| `CHECK_INTERVAL_SECS` | no | `10` | How often to check staleness |
| `DEBOUNCE_SECS` | no | `300` | Wait after an alert before checking again |
| `HEARTBEAT_AUTH_TOKEN` | recommended | unset | Shared secret for `/heartbeat/*` (see Security) |

## Security

The `/heartbeat/poop` endpoint updates the in-memory last-seen timestamp. Anyone who can call it can suppress outage detection. Protect it.

### Application shared-secret auth (recommended)

Set `HEARTBEAT_AUTH_TOKEN` to a long random secret. When this variable is set (non-empty), the service **fails closed**: heartbeat requests without a matching secret receive `401 Unauthorized`.

Clients may send the secret in either header:

```http
X-Heartbeat-Token: <HEARTBEAT_AUTH_TOKEN>
```

or:

```http
Authorization: Bearer <HEARTBEAT_AUTH_TOKEN>
```

Example:

```bash
curl -H "X-Heartbeat-Token: $HEARTBEAT_AUTH_TOKEN" http://localhost:3000/heartbeat/poop
```

If `HEARTBEAT_AUTH_TOKEN` is **not** set, the endpoint remains open and the process logs a warning at startup. Use that mode only when network controls below fully isolate the port.

### Network policy / mTLS (defense in depth)

Even with app auth, prefer restricting who can reach port `3000`:

- **Host firewall / security groups**: allow only the devices that send heartbeats (and operators/metrics scrapers if needed).
- **Kubernetes NetworkPolicy** (or equivalent): ingress only from the monitoring / device namespaces.
- **mTLS / service mesh**: terminate client certificates at the mesh or reverse proxy so only trusted clients can connect; app auth remains an optional second factor.
- **Private network / VPN**: do not expose the service on the public internet.

### Operational notes

- Rotate `HEARTBEAT_AUTH_TOKEN` by updating the secret and redeploying clients and the API together.
- Do not commit secrets; inject them via the orchestrator, Docker secrets, or GitHub Actions repository secrets.

## Contributing
Pull requests are welcome! For major changes, please open an issue first to discuss what you would like to change.

Use feature branches and open a PR into `main`. Version bumps are handled on `main` only (see [Versioning workflow](#versioning-workflow)); you do not need to bump `Cargo.toml` in feature PRs.

## License
MIT
