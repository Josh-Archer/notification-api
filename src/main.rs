use actix_web::{get, web, App, HttpResponse, HttpServer, Responder};
use dotenv::dotenv;
use futures::FutureExt;
use log::{info, warn};
use reqwest::{Client, Error as ReqwestError, Response};
use std::{
    collections::HashMap,
    env,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};
use tokio::{spawn, time};

/// Runtime state for a single monitored device.
#[derive(Debug, Clone)]
struct DeviceRuntime {
    last_seen: Instant,
    timeout_secs: u64,
}

/// Shared application state: per-device last-seen + timeouts.
#[derive(Debug)]
struct AppState {
    devices: Mutex<HashMap<String, DeviceRuntime>>,
}

/// Validate device_id path segments: 1–64 chars, ascii alnum / `_` / `-`.
fn is_valid_device_id(device_id: &str) -> bool {
    let len = device_id.len();
    (1..=64).contains(&len)
        && device_id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

/// Parse `HEARTBEAT_DEVICES` spec into device_id → timeout_secs.
///
/// Format: `device_id[:timeout_secs],...`
/// Examples: `poop`, `poop:90,fridge:120`, `server,laptop:60`
///
/// When timeout is omitted, `default_timeout` is used.
fn parse_devices(spec: &str, default_timeout: u64) -> Result<HashMap<String, u64>, String> {
    let mut map = HashMap::new();
    for part in spec.split(',').map(str::trim).filter(|s| !s.is_empty()) {
        let (id, timeout) = match part.split_once(':') {
            Some((id, t)) => {
                let id = id.trim();
                if !is_valid_device_id(id) {
                    return Err(format!("invalid device id in HEARTBEAT_DEVICES: '{id}'"));
                }
                let timeout: u64 = t.trim().parse().map_err(|_| {
                    format!("invalid timeout for device '{id}' in HEARTBEAT_DEVICES: '{t}'")
                })?;
                if timeout == 0 {
                    return Err(format!("timeout for device '{id}' must be > 0"));
                }
                (id.to_string(), timeout)
            }
            None => {
                if !is_valid_device_id(part) {
                    return Err(format!("invalid device id in HEARTBEAT_DEVICES: '{part}'"));
                }
                (part.to_string(), default_timeout)
            }
        };
        map.insert(id, timeout);
    }
    Ok(map)
}

/// Build initial runtime map from configured device timeouts.
fn build_device_runtime(config: &HashMap<String, u64>) -> HashMap<String, DeviceRuntime> {
    let now = Instant::now();
    config
        .iter()
        .map(|(id, &timeout_secs)| {
            (
                id.clone(),
                DeviceRuntime {
                    // Initialize to now so we don't immediately alert on startup
                    last_seen: now,
                    timeout_secs,
                },
            )
        })
        .collect()
}

/// Record a heartbeat for `device_id`. Returns true if the device is known.
fn record_heartbeat(state: &AppState, device_id: &str) -> bool {
    let mut devices = state.devices.lock().unwrap();
    if let Some(dev) = devices.get_mut(device_id) {
        dev.last_seen = Instant::now();
        info!("Heartbeat for device '{}' at {:?}", device_id, dev.last_seen);
        true
    } else {
        false
    }
}

#[get("/heartbeat/{device_id}")]
async fn heartbeat(
    path: web::Path<String>,
    state: web::Data<Arc<AppState>>,
) -> impl Responder {
    let device_id = path.into_inner();

    if !is_valid_device_id(&device_id) {
        return HttpResponse::BadRequest().body(
            "invalid device_id: use 1-64 ascii alphanumeric, underscore, or hyphen characters",
        );
    }

    if record_heartbeat(state.get_ref(), &device_id) {
        HttpResponse::Ok().body("OK")
    } else {
        HttpResponse::NotFound().body(format!(
            "unknown device '{device_id}'; configure it via HEARTBEAT_DEVICES"
        ))
    }
}

/// Snapshot of devices that have exceeded their timeout.
fn stale_devices(state: &AppState) -> Vec<(String, u64, u64)> {
    let devices = state.devices.lock().unwrap();
    devices
        .iter()
        .filter_map(|(id, dev)| {
            let elapsed = dev.last_seen.elapsed().as_secs();
            if elapsed > dev.timeout_secs {
                Some((id.clone(), elapsed, dev.timeout_secs))
            } else {
                None
            }
        })
        .collect()
}

/// After alerting, refresh last_seen for a device so debounce can apply
/// (mirrors previous single-device behavior).
fn refresh_last_seen(state: &AppState, device_id: &str) {
    if let Some(dev) = state.devices.lock().unwrap().get_mut(device_id) {
        dev.last_seen = Instant::now();
    }
}

fn load_config_from_env() -> (HashMap<String, u64>, u64, u64, u64, String, String) {
    let pushover_token = env::var("PUSHOVER_TOKEN").expect("PUSHOVER_TOKEN must be set in .env");
    let pushover_user = env::var("PUSHOVER_USER").expect("PUSHOVER_USER must be set in .env");

    let default_timeout: u64 = env::var("HEARTBEAT_TIMEOUT_SECS")
        .unwrap_or_else(|_| "90".into())
        .parse()
        .expect("HEARTBEAT_TIMEOUT_SECS must be a number");
    let check_interval: u64 = env::var("CHECK_INTERVAL_SECS")
        .unwrap_or_else(|_| "10".into())
        .parse()
        .expect("CHECK_INTERVAL_SECS must be a number");
    let debounce_secs: u64 = env::var("DEBOUNCE_SECS")
        .unwrap_or_else(|_| "300".into())
        .parse()
        .expect("DEBOUNCE_SECS must be a number");

    // Comma-separated devices with optional per-device timeout:
    // HEARTBEAT_DEVICES=poop:90,fridge:120,server
    let devices_spec = env::var("HEARTBEAT_DEVICES").unwrap_or_else(|_| "poop".into());
    let device_timeouts = parse_devices(&devices_spec, default_timeout)
        .unwrap_or_else(|e| panic!("HEARTBEAT_DEVICES: {e}"));
    if device_timeouts.is_empty() {
        panic!("HEARTBEAT_DEVICES must list at least one device");
    }

    (
        device_timeouts,
        default_timeout,
        check_interval,
        debounce_secs,
        pushover_token,
        pushover_user,
    )
}

#[tokio::main]
async fn main() -> std::io::Result<()> {
    env_logger::init();
    dotenv().ok();

    let (device_timeouts, _default_timeout, check_interval, debounce_secs, pushover_token, pushover_user) =
        load_config_from_env();

    info!(
        "Monitoring {} device(s): {:?}",
        device_timeouts.len(),
        device_timeouts
    );

    let state = Arc::new(AppState {
        devices: Mutex::new(build_device_runtime(&device_timeouts)),
    });

    // Spawn the staleness-check task (per-device)
    let monitor_state = Arc::clone(&state);
    let client = Client::new();
    spawn(async move {
        // Per-device "recently alerted" flag for debounce spacing
        let mut alerted: HashMap<String, bool> = HashMap::new();
        loop {
            // Use debounce interval if any device was just alerted; else normal check interval
            let any_alerted = alerted.values().any(|&v| v);
            let time_interval = if any_alerted {
                info!("Using debounce interval after alert(s)");
                debounce_secs
            } else {
                check_interval
            };
            time::sleep(Duration::from_secs(time_interval)).await;
            alerted.clear();

            let stale = stale_devices(&monitor_state);
            for (device_id, elapsed, timeout_secs) in stale {
                warn!(
                    "No heartbeat from '{}' for {}s (> {}s). Sending Pushover alert.",
                    device_id, elapsed, timeout_secs
                );
                let message = format!("❌ Device '{device_id}' is offline!");
                let pushover_params = [
                    ("token", pushover_token.as_str()),
                    ("user", pushover_user.as_str()),
                    ("message", message.as_str()),
                ];
                let _ = client
                    .post("https://api.pushover.net/1/messages.json")
                    .form(&pushover_params)
                    .send()
                    .inspect(|res: &Result<Response, ReqwestError>| match res {
                        Ok(r) => {
                            info!("Pushover status for '{}': {}", device_id, r.status());
                            alerted.insert(device_id.clone(), true);
                        }
                        Err(e) => warn!("Failed to send Pushover for '{}': {}", device_id, e),
                    })
                    .await;
                // Prevent immediate re-alert until next check window (legacy behavior)
                refresh_last_seen(&monitor_state, &device_id);
            }
        }
    });

    let bind_state = Arc::clone(&state);
    HttpServer::new(move || {
        App::new()
            .app_data(web::Data::new(Arc::clone(&bind_state)))
            .service(heartbeat)
    })
    .bind(("0.0.0.0", 3000))?
    .run()
    .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use actix_web::{test as actix_test, web, App};

    fn test_state(devices: &[(&str, u64)]) -> Arc<AppState> {
        let mut map = HashMap::new();
        let now = Instant::now();
        for &(id, timeout) in devices {
            map.insert(
                id.to_string(),
                DeviceRuntime {
                    last_seen: now,
                    timeout_secs: timeout,
                },
            );
        }
        Arc::new(AppState {
            devices: Mutex::new(map),
        })
    }

    #[test]
    fn parse_devices_default_timeout() {
        let m = parse_devices("poop,fridge", 90).unwrap();
        assert_eq!(m.get("poop"), Some(&90));
        assert_eq!(m.get("fridge"), Some(&90));
    }

    #[test]
    fn parse_devices_per_device_timeout() {
        let m = parse_devices("poop:90,fridge:120,server", 60).unwrap();
        assert_eq!(m.get("poop"), Some(&90));
        assert_eq!(m.get("fridge"), Some(&120));
        assert_eq!(m.get("server"), Some(&60));
    }

    #[test]
    fn parse_devices_rejects_invalid_id() {
        assert!(parse_devices("bad device", 90).is_err());
        assert!(parse_devices("ok:nope", 90).is_err());
        assert!(parse_devices("x:0", 90).is_err());
    }

    #[test]
    fn parse_devices_empty_returns_empty_map() {
        let m = parse_devices("", 90).unwrap();
        assert!(m.is_empty());
        let m = parse_devices("  ,  ", 90).unwrap();
        assert!(m.is_empty());
    }

    #[test]
    fn is_valid_device_id_rules() {
        assert!(is_valid_device_id("poop"));
        assert!(is_valid_device_id("fridge-1"));
        assert!(is_valid_device_id("server_01"));
        assert!(!is_valid_device_id(""));
        assert!(!is_valid_device_id("has space"));
        assert!(!is_valid_device_id("slash/bad"));
        assert!(!is_valid_device_id(&"a".repeat(65)));
    }

    #[test]
    fn record_heartbeat_known_and_unknown() {
        let state = test_state(&[("poop", 90), ("fridge", 120)]);
        assert!(record_heartbeat(&state, "poop"));
        assert!(record_heartbeat(&state, "fridge"));
        assert!(!record_heartbeat(&state, "unknown"));
    }

    #[test]
    fn record_heartbeat_updates_last_seen() {
        let state = test_state(&[("poop", 90)]);
        {
            let mut devices = state.devices.lock().unwrap();
            devices.get_mut("poop").unwrap().last_seen =
                Instant::now() - Duration::from_secs(30);
        }
        let before = state.devices.lock().unwrap().get("poop").unwrap().last_seen;
        assert!(record_heartbeat(&state, "poop"));
        let after = state.devices.lock().unwrap().get("poop").unwrap().last_seen;
        assert!(after > before);
    }

    #[test]
    fn stale_devices_respects_per_device_timeout() {
        let state = test_state(&[("fast", 10), ("slow", 1000)]);
        {
            let mut devices = state.devices.lock().unwrap();
            let old = Instant::now() - Duration::from_secs(50);
            devices.get_mut("fast").unwrap().last_seen = old;
            devices.get_mut("slow").unwrap().last_seen = old;
        }
        let stale = stale_devices(&state);
        let ids: Vec<_> = stale.iter().map(|(id, _, _)| id.as_str()).collect();
        assert_eq!(ids, vec!["fast"]);
    }

    #[test]
    fn build_device_runtime_initializes_all() {
        let mut cfg = HashMap::new();
        cfg.insert("a".into(), 10);
        cfg.insert("b".into(), 20);
        let runtime = build_device_runtime(&cfg);
        assert_eq!(runtime.len(), 2);
        assert_eq!(runtime["a"].timeout_secs, 10);
        assert_eq!(runtime["b"].timeout_secs, 20);
    }

    #[actix_web::test]
    async fn http_heartbeat_ok_for_configured_device() {
        let state = test_state(&[("poop", 90)]);
        let app = actix_test::init_service(
            App::new()
                .app_data(web::Data::new(state))
                .service(heartbeat),
        )
        .await;

        let req = actix_test::TestRequest::get()
            .uri("/heartbeat/poop")
            .to_request();
        let resp = actix_test::call_service(&app, req).await;
        assert!(resp.status().is_success());
        let body = actix_test::read_body(resp).await;
        assert_eq!(body, "OK");
    }

    #[actix_web::test]
    async fn http_heartbeat_404_for_unknown_device() {
        let state = test_state(&[("poop", 90)]);
        let app = actix_test::init_service(
            App::new()
                .app_data(web::Data::new(state))
                .service(heartbeat),
        )
        .await;

        let req = actix_test::TestRequest::get()
            .uri("/heartbeat/unknown")
            .to_request();
        let resp = actix_test::call_service(&app, req).await;
        assert_eq!(resp.status(), 404);
    }

    #[actix_web::test]
    async fn http_heartbeat_400_for_invalid_device_id() {
        let state = test_state(&[("poop", 90)]);
        let app = actix_test::init_service(
            App::new()
                .app_data(web::Data::new(state))
                .service(heartbeat),
        )
        .await;

        // percent-encoded space in path → "bad id"
        let req = actix_test::TestRequest::get()
            .uri("/heartbeat/bad%20id")
            .to_request();
        let resp = actix_test::call_service(&app, req).await;
        assert_eq!(resp.status(), 400);
    }

    #[actix_web::test]
    async fn http_multi_device_independent_heartbeats() {
        let state = test_state(&[("poop", 90), ("fridge", 120)]);
        let app = actix_test::init_service(
            App::new()
                .app_data(web::Data::new(Arc::clone(&state)))
                .service(heartbeat),
        )
        .await;

        for id in ["poop", "fridge"] {
            let req = actix_test::TestRequest::get()
                .uri(&format!("/heartbeat/{id}"))
                .to_request();
            let resp = actix_test::call_service(&app, req).await;
            assert!(resp.status().is_success(), "device {id}");
        }

        let devices = state.devices.lock().unwrap();
        assert!(devices.contains_key("poop"));
        assert!(devices.contains_key("fridge"));
    }
}
