//! Smoke tests for the packaged executable (the crate binary `main` starts).
//!
//! These hit the same process the Dockerfile copies, not a second application
//! factory. Credential placeholders are `REPLACE_ME` and are never real secrets.

use std::io::{Read, Write};
use std::net::TcpStream;
use std::process::{Child, Command, ExitStatus, Stdio};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

const CONFIG_KEYS: &[&str] = &[
    "PUSHOVER_TOKEN",
    "PUSHOVER_USER",
    "PUSHOVER_URL",
    "NTFY_TOPIC",
    "NTFY_SERVER",
    "NTFY_TOKEN",
    "OUTAGE_MESSAGE",
    "RECOVERY_MESSAGE",
    "HEARTBEAT_TIMEOUT_SECS",
    "CHECK_INTERVAL_SECS",
    "DEBOUNCE_SECS",
    "PORT",
];

struct Running {
    child: Child,
    stderr: Option<JoinHandle<String>>,
}

impl Running {
    fn stderr_text(&mut self) -> String {
        match self.stderr.take() {
            Some(handle) => handle.join().unwrap_or_default(),
            None => String::new(),
        }
    }
}

impl Drop for Running {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        if let Some(handle) = self.stderr.take() {
            let _ = handle.join();
        }
    }
}

fn free_port() -> u16 {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("ephemeral port");
    listener.local_addr().expect("local addr").port()
}

fn binary_path() -> std::path::PathBuf {
    for key in [
        "CARGO_BIN_EXE_notification_api",
        "CARGO_BIN_EXE_notification-api",
    ] {
        if let Ok(path) = std::env::var(key) {
            if !path.is_empty() {
                return std::path::PathBuf::from(path);
            }
        }
    }
    let target = std::env::var("CARGO_TARGET_DIR")
        .unwrap_or_else(|_| format!("{}/target", env!("CARGO_MANIFEST_DIR")));
    let profile = if cfg!(debug_assertions) {
        "debug"
    } else {
        "release"
    };
    std::path::PathBuf::from(target)
        .join(profile)
        .join("notification-api")
}

fn spawn_binary(extra: &[(&str, &str)]) -> Running {
    let bin = binary_path();
    assert!(
        bin.is_file(),
        "shipped binary not found at {}",
        bin.display()
    );
    let mut cmd = Command::new(&bin);
    cmd.current_dir(std::env::temp_dir())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .env("RUST_LOG", "error");
    for key in CONFIG_KEYS {
        cmd.env_remove(key);
    }
    for (key, value) in extra {
        cmd.env(key, value);
    }

    let mut child = cmd.spawn().expect("spawn notification-api");
    let stderr_pipe = child.stderr.take().expect("stderr");
    let stderr = thread::spawn(move || {
        let mut pipe = stderr_pipe;
        let mut buf = String::new();
        let _ = pipe.read_to_string(&mut buf);
        buf
    });
    Running {
        child,
        stderr: Some(stderr),
    }
}

fn try_exit(child: &mut Child) -> Option<ExitStatus> {
    child.try_wait().expect("try_wait")
}

fn http_get(port: u16, path: &str) -> (u16, String) {
    let mut stream = TcpStream::connect(("127.0.0.1", port)).expect("connect");
    stream
        .set_read_timeout(Some(Duration::from_secs(2)))
        .expect("read timeout");
    stream
        .set_write_timeout(Some(Duration::from_secs(2)))
        .expect("write timeout");
    let request = format!("GET {path} HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n");
    stream.write_all(request.as_bytes()).expect("write request");
    let mut raw = String::new();
    stream.read_to_string(&mut raw).expect("read response");
    let (head, body) = raw
        .split_once("\r\n\r\n")
        .unwrap_or_else(|| panic!("incomplete HTTP response: {raw}"));
    let status_line = head.lines().next().unwrap_or("");
    let code: u16 = status_line
        .split_whitespace()
        .nth(1)
        .unwrap_or_else(|| panic!("status line {status_line}"))
        .parse()
        .unwrap_or_else(|_| panic!("status code in {status_line}"));
    (code, body.to_string())
}

fn wait_http(running: &mut Running, port: u16, path: &str) -> (u16, String) {
    let deadline = Instant::now() + Duration::from_secs(15);
    let mut last_error = String::new();
    while Instant::now() < deadline {
        if let Some(status) = try_exit(&mut running.child) {
            let stderr = running.stderr_text();
            panic!("binary exited early ({status}): {stderr}");
        }
        match TcpStream::connect(("127.0.0.1", port)) {
            Ok(_) => return http_get(port, path),
            Err(err) => last_error = err.to_string(),
        }
        thread::sleep(Duration::from_millis(50));
    }
    if let Some(status) = try_exit(&mut running.child) {
        let stderr = running.stderr_text();
        panic!("binary exited ({status}) before {path}: {stderr}");
    }
    panic!("timed out waiting for {path} on {port}: {last_error}");
}

fn base_env(port: u16) -> Vec<(&'static str, String)> {
    vec![
        ("PORT", port.to_string()),
        ("CHECK_INTERVAL_SECS", "3600".into()),
        ("HEARTBEAT_TIMEOUT_SECS", "86400".into()),
        ("DEBOUNCE_SECS", "3600".into()),
    ]
}

#[test]
fn shipped_binary_starts_ntfy_only_and_serves_healthz() {
    let port = free_port();
    let mut env = base_env(port);
    // Blank Pushover values match the image ENV / .env and must not block startup.
    env.push(("PUSHOVER_TOKEN", "".into()));
    env.push(("PUSHOVER_USER", "".into()));
    env.push(("NTFY_TOPIC", "solo".into()));
    env.push(("NTFY_SERVER", "http://127.0.0.1:9".into()));
    let owned: Vec<(&str, &str)> = env.iter().map(|(k, v)| (*k, v.as_str())).collect();
    let mut running = spawn_binary(&owned);

    let (code, body) = wait_http(&mut running, port, "/healthz");
    assert_eq!(code, 200, "GET /healthz body: {body}");
    assert!(body.contains("\"status\":\"ok\""), "health payload: {body}");
    assert!(
        body.contains("\"backends\":[\"ntfy\"]"),
        "ntfy-only health payload: {body}"
    );
    assert!(
        body.contains("\"last_heartbeat_secs_ago\":"),
        "health payload: {body}"
    );

    let (beat_code, beat_body) = http_get(port, "/heartbeat/poop");
    assert_eq!(beat_code, 200, "{beat_body}");
    assert_eq!(beat_body, "OK");
}

#[test]
fn shipped_binary_healthz_lists_pushover_only_and_combined_backends() {
    let pushover_port = free_port();
    let mut pushover_env = base_env(pushover_port);
    pushover_env.push(("PUSHOVER_TOKEN", "REPLACE_ME".into()));
    pushover_env.push(("PUSHOVER_USER", "REPLACE_ME".into()));
    let owned: Vec<(&str, &str)> = pushover_env.iter().map(|(k, v)| (*k, v.as_str())).collect();
    let mut pushover = spawn_binary(&owned);
    let (code, body) = wait_http(&mut pushover, pushover_port, "/healthz");
    assert_eq!(code, 200, "{body}");
    assert!(
        body.contains("\"backends\":[\"pushover\"]"),
        "pushover-only health payload: {body}"
    );

    let both_port = free_port();
    let mut both_env = base_env(both_port);
    both_env.push(("PUSHOVER_TOKEN", "REPLACE_ME".into()));
    both_env.push(("PUSHOVER_USER", "REPLACE_ME".into()));
    both_env.push(("NTFY_TOPIC", "combo".into()));
    both_env.push(("NTFY_SERVER", "http://127.0.0.1:9".into()));
    let owned: Vec<(&str, &str)> = both_env.iter().map(|(k, v)| (*k, v.as_str())).collect();
    let mut both = spawn_binary(&owned);
    let (code, body) = wait_http(&mut both, both_port, "/healthz");
    assert_eq!(code, 200, "{body}");
    assert!(
        body.contains("\"backends\":[\"pushover\",\"ntfy\"]"),
        "combined health payload: {body}"
    );
}

#[test]
fn shipped_binary_exits_when_no_backend_is_configured() {
    let mut running = spawn_binary(&[]);
    let deadline = Instant::now() + Duration::from_secs(15);
    let status = loop {
        if let Some(status) = try_exit(&mut running.child) {
            break status;
        }
        if Instant::now() > deadline {
            panic!("binary kept running without a notification backend");
        }
        thread::sleep(Duration::from_millis(50));
    };
    let stderr = running.stderr_text();
    assert!(
        !status.success(),
        "expected non-zero exit, stderr: {stderr}"
    );
    assert!(
        stderr.contains("At least one notification backend"),
        "stderr: {stderr}"
    );
}
