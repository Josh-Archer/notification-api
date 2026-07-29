mod monitor;

use actix_web::{get, App, HttpServer, Responder};
use dotenv::dotenv;
use log::{info, warn};
use monitor::{evaluate_check, evaluate_heartbeat, CheckDecision, HeartbeatDecision};
use once_cell::sync::Lazy;
use reqwest::Client;
use std::{
    env,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};
use tokio::{spawn, time};

/// Shared monitor timestamps and outage flag.
///
/// `last_heartbeat` is only updated on a real `/heartbeat` request.
/// `last_alert` is only updated when an outage alert is successfully sent.
/// Alerts never touch `last_heartbeat` (fixes masking of continued outages).
#[derive(Debug)]
struct MonitorState {
    last_heartbeat: Instant,
    last_alert: Option<Instant>,
    /// True after an outage alert until a recovering heartbeat arrives.
    in_outage: bool,
    /// Anchor so Instant values can be converted to relative seconds for pure logic.
    epoch: Instant,
}

impl MonitorState {
    fn new() -> Self {
        let epoch = Instant::now();
        Self {
            last_heartbeat: epoch,
            last_alert: None,
            in_outage: false,
            epoch,
        }
    }

    fn secs_since_epoch(&self, instant: Instant) -> u64 {
        instant.duration_since(self.epoch).as_secs()
    }

    fn now_secs(&self) -> u64 {
        self.secs_since_epoch(Instant::now())
    }

    /// Process-global timeout from env (set in `main` after load).
    fn timeout_secs(&self) -> u64 {
        TIMEOUT_SECS.load(std::sync::atomic::Ordering::Relaxed)
    }
}

static STATE: Lazy<Arc<Mutex<MonitorState>>> =
    Lazy::new(|| Arc::new(Mutex::new(MonitorState::new())));

static TIMEOUT_SECS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(90);

#[get("/heartbeat/poop")]
async fn heartbeat() -> impl Responder {
    let mut state = STATE.lock().unwrap();
    let now = Instant::now();
    let now_secs = state.secs_since_epoch(now);
    let prev_secs = state.secs_since_epoch(state.last_heartbeat);

    let decision = evaluate_heartbeat(now_secs, prev_secs, state.in_outage, state.timeout_secs());

    match decision {
        HeartbeatDecision::Recorded => {
            info!("Heartbeat received (healthy)");
        }
        HeartbeatDecision::Recovered {
            outage_duration_secs,
        } => {
            info!(
                "Heartbeat received — recovered after {}s without heartbeat",
                outage_duration_secs
            );
            state.in_outage = false;
        }
    }

    // Only real heartbeats advance last_heartbeat.
    state.last_heartbeat = now;
    "OK"
}

async fn send_pushover(client: &Client, token: &str, user: &str, message: &str) -> bool {
    let params = [
        ("token", token),
        ("user", user),
        ("message", message),
    ];
    match client
        .post("https://api.pushover.net/1/messages.json")
        .form(&params)
        .send()
        .await
    {
        Ok(r) => {
            info!("Pushover status: {}", r.status());
            r.status().is_success()
        }
        Err(e) => {
            warn!("Failed to send Pushover: {}", e);
            false
        }
    }
}

#[tokio::main]
async fn main() -> std::io::Result<()> {
    env_logger::init();
    dotenv().ok();

    let pushover_token = env::var("PUSHOVER_TOKEN").expect("PUSHOVER_TOKEN must be set in .env");
    let pushover_user = env::var("PUSHOVER_USER").expect("PUSHOVER_USER must be set in .env");

    let timeout_secs: u64 = env::var("HEARTBEAT_TIMEOUT_SECS")
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

    TIMEOUT_SECS.store(timeout_secs, std::sync::atomic::Ordering::Relaxed);

    let client = Client::new();
    spawn(async move {
        loop {
            time::sleep(Duration::from_secs(check_interval)).await;

            let decision = {
                let state = STATE.lock().unwrap();
                let now_secs = state.now_secs();
                let last_hb = state.secs_since_epoch(state.last_heartbeat);
                let last_alert = state
                    .last_alert
                    .map(|t| state.secs_since_epoch(t));
                evaluate_check(now_secs, last_hb, last_alert, timeout_secs, debounce_secs)
            };

            match decision {
                CheckDecision::Healthy {
                    secs_since_heartbeat,
                } => {
                    info!(
                        "Heartbeat fresh ({}s ago, timeout {}s)",
                        secs_since_heartbeat, timeout_secs
                    );
                }
                CheckDecision::StillDown {
                    secs_since_heartbeat,
                    secs_since_alert,
                    secs_until_next_alert,
                } => {
                    info!(
                        "Still down: no heartbeat for {}s; last alert {}s ago; next alert in {}s (debounce {}s)",
                        secs_since_heartbeat,
                        secs_since_alert,
                        secs_until_next_alert,
                        debounce_secs
                    );
                }
                CheckDecision::AlertOutage {
                    secs_since_heartbeat,
                } => {
                    warn!(
                        "No heartbeat for {}s (> {}s). Sending Pushover outage alert.",
                        secs_since_heartbeat, timeout_secs
                    );
                    let message = format!(
                        "❌ Poop Monitor is offline! (no heartbeat for {}s)",
                        secs_since_heartbeat
                    );
                    let ok =
                        send_pushover(&client, &pushover_token, &pushover_user, &message).await;
                    if ok {
                        let mut state = STATE.lock().unwrap();
                        // Only last_alert / in_outage change — never touch last_heartbeat.
                        state.last_alert = Some(Instant::now());
                        state.in_outage = true;
                    }
                }
            }
        }
    });

    HttpServer::new(|| App::new().service(heartbeat))
        .bind(("0.0.0.0", 3000))?
        .run()
        .await
}
