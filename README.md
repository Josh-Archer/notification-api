# Notification API

A Rust-based notification API with Docker deployment and automated versioning.

## Features
- Written in Rust using Actix Web
- Sends notifications via Pushover
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
- Non-sensitive config (e.g., timeouts) can be set in `.env`.

## Contributing
Pull requests are welcome! For major changes, please open an issue first to discuss what you would like to change.

## License
MIT

