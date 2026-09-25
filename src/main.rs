mod monitor;

use actix_web::{get, web, App, HttpServer, Responder};
use dotenv::dotenv;
use log::{info, warn};
use monitor::{evaluate_check, evaluate_heartbeat, CheckDecision, HeartbeatDecision};
use once_cell::sync::Lazy;
use reqwest::Client;
use std::{
    env,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc, Mutex,
    },
    time::{Duration, Instant},
};
use tokio::{spawn, time};

/// Monitor lifecycle phase for outage / recovery alerting.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Phase {
    /// Heartbeats are flowing (or startup before any outage alert).
    Healthy,
    /// An outage alert was sent; waiting for heartbeat recovery.
    Outage,
}

/// Alert action produced by a state transition.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Alert {
    None,
    Outage,
    Recovery,
}

/// Heartbeat received: recover only if we previously alerted an outage.
fn on_heartbeat(phase: Phase) -> (Phase, Alert) {
    match phase {
        Phase::Healthy => (Phase::Healthy, Alert::None),
        Phase::Outage => (Phase::Healthy, Alert::Recovery),
    }
}

/// Periodic staleness check: emit outage while heartbeats are missing.
fn on_staleness_check(phase: Phase, is_stale: bool) -> (Phase, Alert) {
    if is_stale {
        (Phase::Outage, Alert::Outage)
    } else {
        (phase, Alert::None)
    }
}

/// Phase transition after an outage alert send attempt.
/// Only moves phase to Outage when sending the alert succeeds.
fn on_outage_alert_result(phase: Phase, send_succeeded: bool) -> Phase {
    if send_succeeded {
        let (next, _) = on_staleness_check(phase, true);
        next
    } else {
        phase
    }
}

struct MonitorState {
    last_seen: Instant,
    last_alert: Option<Instant>,
    phase: Phase,
    epoch: Instant,
}

impl MonitorState {
    fn new() -> Self {
        let epoch = Instant::now();
        Self {
            last_seen: epoch,
            last_alert: None,
            phase: Phase::Healthy,
            epoch,
        }
    }

    fn secs_since_epoch(&self, instant: Instant) -> u64 {
        instant
            .checked_duration_since(self.epoch)
            .map(|d| d.as_secs())
            .unwrap_or(0)
    }

    fn now_secs(&self) -> u64 {
        self.secs_since_epoch(Instant::now())
    }

    /// Check if the monitor is currently stale and needs an outage alert.
    #[cfg(test)]
    fn check_staleness(&self, timeout_secs: u64) -> (bool, Alert) {
        let elapsed = self.last_seen.elapsed().as_secs();
        let is_stale = elapsed > timeout_secs;
        let (_, alert) = on_staleness_check(self.phase, is_stale);
        (is_stale, alert)
    }

    /// Record the outcome of an outage alert send.
    /// Only updates phase to Outage and sets last_alert if sending succeeded.
    /// Note: last_seen is NOT reset on staleness/alert.
    fn record_outage_result(&mut self, send_succeeded: bool) {
        self.phase = on_outage_alert_result(self.phase, send_succeeded);
        if send_succeeded {
            self.last_alert = Some(Instant::now());
        }
    }

    /// Record incoming heartbeat: updates last_seen, clears last_alert, and transitions phase.
    fn record_heartbeat(&mut self) -> Alert {
        self.last_seen = Instant::now();
        self.last_alert = None;
        let (next, alert) = on_heartbeat(self.phase);
        self.phase = next;
        alert
    }
}

// Shared monitor state (last heartbeat + last alert + outage/recovery phase)
static MONITOR: Lazy<Arc<Mutex<MonitorState>>> =
    Lazy::new(|| Arc::new(Mutex::new(MonitorState::new())));

static TIMEOUT_SECS: AtomicU64 = AtomicU64::new(90);

#[derive(Clone)]
struct NotifyConfig {
    token: String,
    user: String,
    outage_message: String,
    recovery_message: String,
    pushover_url: String,
}

async fn send_pushover(client: &Client, cfg: &NotifyConfig, message: &str) -> bool {
    let pushover_params = [
        ("token", cfg.token.as_str()),
        ("user", cfg.user.as_str()),
        ("message", message),
    ];
    match client
        .post(&cfg.pushover_url)
        .form(&pushover_params)
        .send()
        .await
    {
        Ok(r) if r.status().is_success() => {
            info!("Pushover status: {}", r.status());
            true
        }
        Ok(r) => {
            warn!("Pushover request failed with status: {}", r.status());
            false
        }
        Err(e) => {
            warn!("Failed to send Pushover: {}", e);
            false
        }
    }
}

#[get("/heartbeat/poop")]
async fn heartbeat(client: web::Data<Client>, cfg: web::Data<NotifyConfig>) -> impl Responder {
    let (alert, recovery_message) = {
        let mut mon = MONITOR.lock().unwrap();
        let now = Instant::now();
        let now_secs = mon.secs_since_epoch(now);
        let prev_secs = mon.secs_since_epoch(mon.last_seen);
        let in_outage = mon.phase == Phase::Outage;
        let timeout = TIMEOUT_SECS.load(Ordering::Relaxed);
        let decision = evaluate_heartbeat(now_secs, prev_secs, in_outage, timeout);

        let alert = mon.record_heartbeat();

        match decision {
            HeartbeatDecision::Recorded => {
                info!(
                    "Heartbeat received at {:?}; phase={:?}",
                    mon.last_seen, mon.phase
                );
            }
            HeartbeatDecision::Recovered {
                outage_duration_secs,
            } => {
                info!(
                    "Heartbeat received at {:?} — recovered after {}s without heartbeat; phase={:?}",
                    mon.last_seen, outage_duration_secs, mon.phase
                );
            }
        }
        (alert, cfg.recovery_message.clone())
    };

    if alert == Alert::Recovery {
        info!("Heartbeat resumed after outage; sending recovery alert.");
        let _ = send_pushover(client.get_ref(), cfg.get_ref(), &recovery_message).await;
    }

    "OK"
}

#[tokio::main]
async fn main() -> std::io::Result<()> {
    // Initialize logging and load .env
    env_logger::init();
    dotenv().ok();

    let pushover_token = env::var("PUSHOVER_TOKEN").expect("PUSHOVER_TOKEN must be set in .env");
    let pushover_user = env::var("PUSHOVER_USER").expect("PUSHOVER_USER must be set in .env");
    let pushover_url = env::var("PUSHOVER_URL")
        .unwrap_or_else(|_| "https://api.pushover.net/1/messages.json".into());
    let outage_message =
        env::var("OUTAGE_MESSAGE").unwrap_or_else(|_| "❌ Poop Monitor is offline!".into());
    let recovery_message =
        env::var("RECOVERY_MESSAGE").unwrap_or_else(|_| "✅ Poop Monitor is back online!".into());

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

    TIMEOUT_SECS.store(timeout_secs, Ordering::Relaxed);

    let notify = NotifyConfig {
        token: pushover_token,
        user: pushover_user,
        outage_message,
        recovery_message,
        pushover_url,
    };
    let client = Client::new();

    // Spawn the staleness-check task
    let watcher_client = client.clone();
    let watcher_notify = notify.clone();
    spawn(async move {
        loop {
            time::sleep(Duration::from_secs(check_interval)).await;

            let (decision, outage_message) = {
                let mon = MONITOR.lock().unwrap();
                let now_secs = mon.now_secs();
                let last_hb = mon.secs_since_epoch(mon.last_seen);
                let last_alert = mon.last_alert.map(|t| mon.secs_since_epoch(t));
                let decision =
                    evaluate_check(now_secs, last_hb, last_alert, timeout_secs, debounce_secs);
                (decision, watcher_notify.outage_message.clone())
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
                    let sent =
                        send_pushover(&watcher_client, &watcher_notify, &outage_message).await;
                    let mut mon = MONITOR.lock().unwrap();
                    mon.record_outage_result(sent);
                    if !sent {
                        warn!("Failed to send outage alert; leaving monitor phase and last_seen unchanged");
                    }
                }
            }
        }
    });

    // Start HTTP server
    let http_client = web::Data::new(client);
    let http_notify = web::Data::new(notify);
    HttpServer::new(move || {
        App::new()
            .app_data(http_client.clone())
            .app_data(http_notify.clone())
            .service(heartbeat)
    })
    .bind(("0.0.0.0", 3000))?
    .run()
    .await
}

#[cfg(test)]
mod tests {
    use super::{
        monitor::{evaluate_check, CheckDecision},
        on_heartbeat, on_outage_alert_result, on_staleness_check, send_pushover, Alert,
        MonitorState, NotifyConfig, Phase,
    };
    use actix_web::{web, App, HttpResponse, HttpServer};
    use reqwest::Client;
    use std::time::{Duration, Instant};

    #[test]
    fn heartbeat_while_healthy_does_not_recover() {
        let (phase, alert) = on_heartbeat(Phase::Healthy);
        assert_eq!(phase, Phase::Healthy);
        assert_eq!(alert, Alert::None);
    }

    #[test]
    fn heartbeat_after_outage_fires_recovery_once() {
        let (phase, alert) = on_heartbeat(Phase::Outage);
        assert_eq!(phase, Phase::Healthy);
        assert_eq!(alert, Alert::Recovery);

        // Subsequent heartbeats must not re-fire recovery
        let (phase2, alert2) = on_heartbeat(phase);
        assert_eq!(phase2, Phase::Healthy);
        assert_eq!(alert2, Alert::None);
    }

    #[test]
    fn staleness_from_healthy_fires_outage() {
        let (phase, alert) = on_staleness_check(Phase::Healthy, true);
        assert_eq!(phase, Phase::Outage);
        assert_eq!(alert, Alert::Outage);
    }

    #[test]
    fn staleness_while_already_outage_can_realert() {
        // Debounced re-alert path: still stale after prior outage
        let (phase, alert) = on_staleness_check(Phase::Outage, true);
        assert_eq!(phase, Phase::Outage);
        assert_eq!(alert, Alert::Outage);
    }

    #[test]
    fn not_stale_keeps_phase_without_alert() {
        let (p1, a1) = on_staleness_check(Phase::Healthy, false);
        assert_eq!((p1, a1), (Phase::Healthy, Alert::None));

        let (p2, a2) = on_staleness_check(Phase::Outage, false);
        assert_eq!((p2, a2), (Phase::Outage, Alert::None));
    }

    #[test]
    fn full_outage_then_recovery_cycle() {
        let mut phase = Phase::Healthy;

        // Normal heartbeats: no alerts
        let (p, a) = on_heartbeat(phase);
        phase = p;
        assert_eq!(a, Alert::None);

        // Outage detected
        let (p, a) = on_staleness_check(phase, true);
        phase = p;
        assert_eq!(phase, Phase::Outage);
        assert_eq!(a, Alert::Outage);

        // Heartbeat returns → recovery only once
        let (p, a) = on_heartbeat(phase);
        phase = p;
        assert_eq!(phase, Phase::Healthy);
        assert_eq!(a, Alert::Recovery);

        let (_, a) = on_heartbeat(phase);
        assert_eq!(a, Alert::None);
    }

    #[test]
    fn recovery_only_after_prior_outage_alert() {
        // Never went stale → no recovery on heartbeat
        assert_eq!(on_heartbeat(Phase::Healthy).1, Alert::None);

        // After outage transition only
        let (phase, outage) = on_staleness_check(Phase::Healthy, true);
        assert_eq!(outage, Alert::Outage);
        assert_eq!(on_heartbeat(phase).1, Alert::Recovery);
    }

    #[test]
    fn monitor_state_outage_alert_does_not_reset_last_seen() {
        let mut mon = MonitorState::new();
        let initial_last_seen = mon.last_seen;

        // Simulate outage alert being sent
        mon.record_outage_result(true);

        // last_seen must remain unchanged (not reset to now on alert)
        assert_eq!(mon.last_seen, initial_last_seen);
        assert_eq!(mon.phase, Phase::Outage);
        assert!(mon.last_alert.is_some());
    }

    #[test]
    fn monitor_state_true_downtime_preserved_across_debounce() {
        let timeout_secs = 90;
        let debounce_secs = 300;

        // Heartbeat was at epoch (t=0)
        let last_hb_secs = 0;

        // At t=100 (past timeout, no previous alert): AlertOutage
        let decision = evaluate_check(100, last_hb_secs, None, timeout_secs, debounce_secs);
        assert_eq!(
            decision,
            CheckDecision::AlertOutage {
                secs_since_heartbeat: 100
            }
        );

        // Alert sent at t=100. Debouncing at t=200: StillDown, true downtime is 200s
        let decision = evaluate_check(200, last_hb_secs, Some(100), timeout_secs, debounce_secs);
        assert_eq!(
            decision,
            CheckDecision::StillDown {
                secs_since_heartbeat: 200,
                secs_since_alert: 100,
                secs_until_next_alert: 200,
            }
        );

        // After debounce at t=400: AlertOutage with true downtime 400s (not 300s!)
        let decision = evaluate_check(400, last_hb_secs, Some(100), timeout_secs, debounce_secs);
        assert_eq!(
            decision,
            CheckDecision::AlertOutage {
                secs_since_heartbeat: 400
            }
        );
    }

    #[test]
    fn outage_alert_result_only_transitions_on_success() {
        assert_eq!(
            on_outage_alert_result(Phase::Healthy, false),
            Phase::Healthy
        );
        assert_eq!(on_outage_alert_result(Phase::Healthy, true), Phase::Outage);
        assert_eq!(on_outage_alert_result(Phase::Outage, false), Phase::Outage);
        assert_eq!(on_outage_alert_result(Phase::Outage, true), Phase::Outage);
    }

    #[test]
    fn failed_outage_send_does_not_clear_last_seen_or_advance_phase() {
        let mut mon = MonitorState::new();
        // Stale by 100 seconds (timeout 90)
        mon.last_seen = Instant::now() - Duration::from_secs(100);
        let (is_stale, alert) = mon.check_staleness(90);
        assert!(is_stale);
        assert_eq!(alert, Alert::Outage);

        // Failed send: phase must stay Healthy, last_alert must be None, and last_seen must NOT be cleared
        mon.record_outage_result(false);
        assert_eq!(mon.phase, Phase::Healthy);
        assert_eq!(mon.last_alert, None);
        assert!(mon.last_seen.elapsed().as_secs() >= 100);

        // Next check immediately still sees staleness (not suppressed)
        let (is_stale_next, alert_next) = mon.check_staleness(90);
        assert!(is_stale_next);
        assert_eq!(alert_next, Alert::Outage);

        // Subsequent heartbeat must NOT trigger recovery alert
        let hb_alert = mon.record_heartbeat();
        assert_eq!(hb_alert, Alert::None);
        assert_eq!(mon.phase, Phase::Healthy);
    }

    #[test]
    fn successful_outage_send_records_outage_and_preserves_last_seen() {
        let mut mon = MonitorState::new();
        mon.last_seen = Instant::now() - Duration::from_secs(100);
        let (is_stale, alert) = mon.check_staleness(90);
        assert!(is_stale);
        assert_eq!(alert, Alert::Outage);

        // Succeeded send: phase becomes Outage, last_alert is recorded, and last_seen is NOT reset
        mon.record_outage_result(true);
        assert_eq!(mon.phase, Phase::Outage);
        assert!(mon.last_alert.is_some());
        assert!(mon.last_seen.elapsed().as_secs() >= 100);

        // Subsequent heartbeat fires recovery
        let hb_alert = mon.record_heartbeat();
        assert_eq!(hb_alert, Alert::Recovery);
        assert_eq!(mon.phase, Phase::Healthy);
        assert_eq!(mon.last_alert, None);

        // Next heartbeat after recovery does not fire recovery again
        let hb_alert2 = mon.record_heartbeat();
        assert_eq!(hb_alert2, Alert::None);
    }

    #[actix_web::test]
    async fn send_pushover_checks_http_status_codes() {
        let server = HttpServer::new(|| {
            App::new()
                .route(
                    "/ok",
                    web::post().to(|| async { HttpResponse::Ok().body("{\"status\":1}") }),
                )
                .route(
                    "/client_error",
                    web::post().to(|| async {
                        HttpResponse::BadRequest()
                            .body("{\"status\":0,\"errors\":[\"token is invalid\"]}")
                    }),
                )
                .route(
                    "/unauthorized",
                    web::post().to(|| async {
                        HttpResponse::Unauthorized()
                            .body("{\"status\":0,\"errors\":[\"user not found\"]}")
                    }),
                )
                .route(
                    "/server_error",
                    web::post().to(|| async {
                        HttpResponse::InternalServerError()
                            .body("{\"status\":0,\"errors\":[\"internal error\"]}")
                    }),
                )
        })
        .bind(("127.0.0.1", 0))
        .expect("bind mock server");

        let port = server.addrs()[0].port();
        let _server_handle = tokio::spawn(server.run());

        let client = Client::new();
        let mut cfg = NotifyConfig {
            token: "REDACTED_TEST_VALUE".into(),
            user: "REDACTED_TEST_VALUE".into(),
            outage_message: "outage".into(),
            recovery_message: "recovery".into(),
            pushover_url: format!("http://127.0.0.1:{}/ok", port),
        };

        // 200 OK -> true
        assert!(send_pushover(&client, &cfg, "msg").await);

        // 400 Bad Request (Pushover 4xx) -> false
        cfg.pushover_url = format!("http://127.0.0.1:{}/client_error", port);
        assert!(!send_pushover(&client, &cfg, "msg").await);

        // 401 Unauthorized (Pushover 4xx) -> false
        cfg.pushover_url = format!("http://127.0.0.1:{}/unauthorized", port);
        assert!(!send_pushover(&client, &cfg, "msg").await);

        // 500 Internal Server Error -> false
        cfg.pushover_url = format!("http://127.0.0.1:{}/server_error", port);
        assert!(!send_pushover(&client, &cfg, "msg").await);

        // Network connection error -> false
        cfg.pushover_url = "http://127.0.0.1:1/nonexistent".into();
        assert!(!send_pushover(&client, &cfg, "msg").await);
    }

    #[test]
    fn failed_pushover_send_does_not_start_debounce_window() {
        let mut mon = MonitorState::new();
        let timeout_secs = 90;
        let debounce_secs = 300;
        let last_hb_secs = 0;

        // t=100: heartbeat is past timeout (100 > 90), so check wants to alert outage
        let decision = evaluate_check(
            100,
            last_hb_secs,
            mon.last_alert.map(|t| mon.secs_since_epoch(t)),
            timeout_secs,
            debounce_secs,
        );
        assert_eq!(
            decision,
            CheckDecision::AlertOutage {
                secs_since_heartbeat: 100
            }
        );

        // Simulate send_pushover failure (e.g. Pushover returned 4xx)
        let send_success = false;
        if send_success {
            let (next, _) = on_staleness_check(mon.phase, true);
            mon.phase = next;
            mon.last_alert = Some(Instant::now());
        }

        // Neither last_alert nor phase should have been updated
        assert!(mon.last_alert.is_none());
        assert_eq!(mon.phase, Phase::Healthy);

        // Next check cycle (e.g. t=110): debounce window is NOT running, so it retries alerting
        let decision = evaluate_check(
            110,
            last_hb_secs,
            mon.last_alert.map(|t| mon.secs_since_epoch(t)),
            timeout_secs,
            debounce_secs,
        );
        assert_eq!(
            decision,
            CheckDecision::AlertOutage {
                secs_since_heartbeat: 110
            }
        );

        // A subsequent heartbeat does NOT emit a false recovery alert
        let (_, alert) = on_heartbeat(mon.phase);
        assert_eq!(alert, Alert::None);
    }
}
