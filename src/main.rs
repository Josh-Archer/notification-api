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

struct MonitorState {
    last_seen: Instant,
    phase: Phase,
}

// Shared monitor state (last heartbeat + outage/recovery phase)
static MONITOR: Lazy<Arc<Mutex<MonitorState>>> = Lazy::new(|| {
    Arc::new(Mutex::new(MonitorState {
        // Initialize to now so we don't immediately trigger alert on startup
        last_seen: Instant::now(),
        phase: Phase::Healthy,
    }))
});

#[derive(Clone)]
struct NotifyConfig {
    token: String,
    user: String,
    outage_message: String,
    recovery_message: String,
}

async fn send_pushover(client: &Client, cfg: &NotifyConfig, message: &str) -> bool {
    let pushover_params = [
        ("token", cfg.token.as_str()),
        ("user", cfg.user.as_str()),
        ("message", message),
    ];
    match client
        .post("https://api.pushover.net/1/messages.json")
        .form(&pushover_params)
        .send()
        .await
    {
        Ok(r) => {
            info!("Pushover status: {}", r.status());
            true
        }
        Err(e) => {
            warn!("Failed to send Pushover: {}", e);
            false
        }
    }
}

#[get("/heartbeat/poop")]
async fn heartbeat(
    client: web::Data<Client>,
    cfg: web::Data<NotifyConfig>,
) -> impl Responder {
    let (alert, recovery_message) = {
        let mut mon = MONITOR.lock().unwrap();
        mon.last_seen = Instant::now();
        let (next, alert) = on_heartbeat(mon.phase);
        mon.phase = next;
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
    let outage_message = env::var("OUTAGE_MESSAGE")
        .unwrap_or_else(|_| "❌ Poop Monitor is offline!".into());
    let recovery_message = env::var("RECOVERY_MESSAGE")
        .unwrap_or_else(|_| "✅ Poop Monitor is back online!".into());

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
                let mut mon = MONITOR.lock().unwrap();
                let elapsed = mon.last_seen.elapsed().as_secs();
                let is_stale = elapsed > timeout_secs;
                let (next, alert) = on_staleness_check(mon.phase, is_stale);
                mon.phase = next;
                if is_stale {
                    warn!(
                        "No heartbeat for {}s (> {}s). phase={:?}, alert={:?}",
                        elapsed, timeout_secs, mon.phase, alert
                    );
                    // Prevent repeat alerts until next heartbeat resets last_seen
                    mon.last_seen = Instant::now();
                }
                (alert, watcher_notify.outage_message.clone())
            };

            if alert == Alert::Outage {
                if send_pushover(&watcher_client, &watcher_notify, &outage_message).await {
                    connection_missing = true;
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
    use super::{on_heartbeat, on_staleness_check, Alert, Phase};

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
}
