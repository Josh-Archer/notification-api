# Shipped entrypoint smoke test

Helper tests can miss the program that an image or package actually starts. A library test that builds its own application factory does not prove the packaged `main` serves the same routes, reads the same configuration, or shuts down. This pattern smoke-tests that shipped entrypoint in CI.

Adoption outside this repository is tracked separately. Copy the checklist into the other service's own issue. Do not treat this document as a bulk change to those repositories.

## Checklist

1. Name the artifact CI ships. For a container, that is the binary or command in the image `CMD`, not a test harness.
2. Build that artifact in CI (release profile, locked dependencies) and start it. Do not stop at unit tests.
3. Give it synthetic configuration only. Credentials are placeholders such as `REPLACE_ME`. Do not read production secrets and do not call production hosts.
4. Point every outbound call at a local fixture that records the request and returns a canned success or failure. The fixture must not forward traffic.
5. Use an ephemeral port so the job does not depend on a fixed host port.
6. Check all of the following against the running process:
   - Health and primary routes respond from that process.
   - Backend selection matches the synthetic configuration (reported by the process, and proven by which fixture URL received the call).
   - Startup fails closed for missing required configuration, invalid configuration, and a port that is already taken.
   - A failed outbound call does not kill the process.
   - `SIGTERM` stops the process with exit code 0.
7. Keep the fast unit suite. The smoke test complements it. It does not replace state-machine tests.
8. Run the same checks locally with the built binary before merging.

## What not to do

- Do not assert routes by mounting them in an in-process test server if production `main` builds a different server.
- Do not send real notifications, even to a "test" topic on a hosted service.
- Do not bake secrets into the workflow, the image, or the fixture log.
- Do not delete or mutate live infrastructure from the smoke job.

## notification-api adoption

This repository adopts the pattern as follows. Other services still need their own issues.

| Piece | Role |
| --- | --- |
| `Dockerfile` `CMD` | Runs `/app/notification-api`, copied from `target/release/notification-api`. |
| `.github/workflows/ci.yml` | On pull requests and pushes to `main`: `cargo test --locked`, `cargo build --release --locked`, then the smoke script on that release binary. |
| `scripts/smoke-entrypoint.sh` | Starts the given binary with synthetic Pushover settings and checks routes, backend selection, startup failures, send failure, and `SIGTERM`. |
| `scripts/smoke_notification_fixture.py` | Listens on `127.0.0.1` and records POSTs. It does not forward them. |
| `tests/smoke_entrypoint.rs` | Runs the same script against the binary Cargo just built, so `cargo test` fails when the entrypoint drifts. |

`GET /healthz` on the shipped process reports `status`, `uptime_secs`, `backends`, and `last_heartbeat_secs_ago`. With the synthetic Pushover configuration the smoke test uses, `backends` is `["pushover"]` and both the outage and recovery posts arrive at `PUSHOVER_URL` on the fixture. Placeholder token and user are `REPLACE_ME`.

ntfy-only startup is a separate change. This smoke test covers the executable the image runs today and will keep covering Pushover selection after that change, as long as `/healthz` still lists `pushover` when only Pushover is configured.

## Local run

```bash
cargo test --locked
cargo build --release --locked
bash scripts/smoke-entrypoint.sh target/release/notification-api
```

Requirements: `bash`, `python3`, `curl`, and `timeout` (GNU coreutils). No network access to Pushover or ntfy is required.
