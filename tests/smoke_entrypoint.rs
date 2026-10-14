//! Smoke-test the crate binary that `main` builds.
//!
//! Cargo sets `CARGO_BIN_EXE_notification-api` when this integration test is
//! built (hyphen kept). The script starts that executable with synthetic
//! configuration. It does not use the library test factory. See
//! `docs/smoke-test-pattern.md`.

use std::process::Command;

#[test]
fn smoke_shipped_entrypoint() {
    // Binary target name is `notification-api`; Cargo keeps the hyphen.
    let bin = env!("CARGO_BIN_EXE_notification-api");
    let script = concat!(env!("CARGO_MANIFEST_DIR"), "/scripts/smoke-entrypoint.sh");
    let output = Command::new("bash")
        .arg(script)
        .arg(&bin)
        .output()
        .expect("spawn smoke-entrypoint.sh");
    if !output.status.success() {
        panic!(
            "smoke-entrypoint.sh failed ({})\n--- stdout ---\n{}\n--- stderr ---\n{}",
            output.status,
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }
}
