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
- Sensitive values (e.g., `PUSHOVER_TOKEN`, `PUSHOVER_USER`) should be set as GitHub secrets or environment variables, not in `.env`.
- Non-sensitive config (e.g., timeouts) can be set in `.env`.

## Contributing
Pull requests are welcome! For major changes, please open an issue first to discuss what you would like to change.

Use feature branches and open a PR into `main`. Version bumps are handled on `main` only (see [Versioning workflow](#versioning-workflow)); you do not need to bump `Cargo.toml` in feature PRs.

## License
MIT
