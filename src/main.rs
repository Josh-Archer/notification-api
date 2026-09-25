use actix_web::{get, web, App, HttpServer, Responder};
use dotenv::dotenv;
use log::{info, warn};
use once_cell::sync::Lazy;
use reqwest::Client;
use std::{
    env,
    sync::{Arc, Mutex},
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
        Phase::Outage
    } else {
        phase
    }
}

struct MonitorState {
    last_seen: Instant,
    phase: Phase,
}

impl MonitorState {
    fn new() -> Self {
        Self {
            last_seen: Instant::now(),
            phase: Phase::Healthy,
        }
    }

    /// Check if the monitor is currently stale and needs an outage alert.
    fn check_staleness(&self, timeout_secs: u64) -> (bool, Alert) {
        let elapsed = self.last_seen.elapsed().as_secs();
        let is_stale = elapsed > timeout_secs;
        let (_, alert) = on_staleness_check(self.phase, is_stale);
        (is_stale, alert)
    }

    /// Record the outcome of an outage alert send.
    /// Only updates phase to Outage and resets last_seen if sending succeeded.
    fn record_outage_result(&mut self, send_succeeded: bool) {
        self.phase = on_outage_alert_result(self.phase, send_succeeded);
        if send_succeeded {
            // Prevent repeat alerts until next heartbeat or debounce window
            self.last_seen = Instant::now();
        }
    }

    /// Record incoming heartbeat: updates last_seen and transitions phase back to Healthy.
    fn record_heartbeat(&mut self) -> Alert {
        self.last_seen = Instant::now();
        let (next, alert) = on_heartbeat(self.phase);
        self.phase = next;
        alert
    }
}

// Shared monitor state (last heartbeat + outage/recovery phase)
static MONITOR: Lazy<Arc<Mutex<MonitorState>>> =
    Lazy::new(|| Arc::new(Mutex::new(MonitorState::new())));

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
        let alert = mon.record_heartbeat();
        info!(
            "Heartbeat received at {:?}; phase={:?}",
            mon.last_seen, mon.phase
        );
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

    let notify = NotifyConfig {
        token: pushover_token,
        user: pushover_user,
        outage_message,
        recovery_message,
        pushover_url,
    };
    let client = Client::new();

    // Spawn the staleness-check task
    let mut connection_missing: bool = false;
    let watcher_client = client.clone();
    let watcher_notify = notify.clone();
    spawn(async move {
        loop {
            let mut time_interval = check_interval;
            if connection_missing {
                info!("Starting debounce now that we alerted");
                time_interval = debounce_secs;
            }
            time::sleep(Duration::from_secs(time_interval)).await;
            connection_missing = false;

            let (alert, outage_message) = {
                let mon = MONITOR.lock().unwrap();
                let elapsed = mon.last_seen.elapsed().as_secs();
                let (is_stale, alert) = mon.check_staleness(timeout_secs);
                if is_stale {
                    warn!(
                        "No heartbeat for {}s (> {}s). phase={:?}, alert={:?}",
                        elapsed, timeout_secs, mon.phase, alert
                    );
                }
                (alert, watcher_notify.outage_message.clone())
            };

            if alert == Alert::Outage {
                if send_pushover(&watcher_client, &watcher_notify, &outage_message).await {
                    let mut mon = MONITOR.lock().unwrap();
                    mon.record_outage_result(true);
                    connection_missing = true;
                } else {
                    warn!("Failed to send outage alert; leaving monitor phase and last_seen unchanged");
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
        on_heartbeat, on_outage_alert_result, on_staleness_check, send_pushover, Alert,
        MonitorState, NotifyConfig, Phase,
    };
    use actix_web::{web, App, HttpResponse};
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

        // Failed send: phase must stay Healthy and last_seen must NOT be cleared
        mon.record_outage_result(false);
        assert_eq!(mon.phase, Phase::Healthy);
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
    fn successful_outage_send_records_outage_and_resets_last_seen() {
        let mut mon = MonitorState::new();
        mon.last_seen = Instant::now() - Duration::from_secs(100);
        let (is_stale, alert) = mon.check_staleness(90);
        assert!(is_stale);
        assert_eq!(alert, Alert::Outage);

        // Succeeded send: phase becomes Outage and last_seen is reset to now
        mon.record_outage_result(true);
        assert_eq!(mon.phase, Phase::Outage);
        assert!(mon.last_seen.elapsed().as_secs() < 2);

        // Subsequent heartbeat fires recovery
        let hb_alert = mon.record_heartbeat();
        assert_eq!(hb_alert, Alert::Recovery);
        assert_eq!(mon.phase, Phase::Healthy);

        // Next heartbeat after recovery does not fire recovery again
        let hb_alert2 = mon.record_heartbeat();
        assert_eq!(hb_alert2, Alert::None);
    }

    #[actix_web::test]
    async fn send_pushover_status_handling() {
        use actix_web::HttpServer;

        let server = HttpServer::new(|| {
            App::new()
                .route(
                    "/ok",
                    web::post().to(|| async { HttpResponse::Ok().body("{\"status\":1}") }),
                )
                .route(
                    "/err",
                    web::post().to(|| async {
                        HttpResponse::InternalServerError().body("{\"status\":0}")
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

        // 500 Internal Server Error -> false
        cfg.pushover_url = format!("http://127.0.0.1:{}/err", port);
        assert!(!send_pushover(&client, &cfg, "msg").await);

        // Connection refused / invalid port -> false
        cfg.pushover_url = "http://127.0.0.1:1/invalid".into();
        assert!(!send_pushover(&client, &cfg, "msg").await);
    }
}
