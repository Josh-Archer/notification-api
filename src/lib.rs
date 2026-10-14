//! Heartbeat watcher with multi-channel notification backends.
//!
//! The binary in `main.rs` is a thin entrypoint over this module. `/healthz`,
//! `/heartbeat/poop`, configuration, delivery, and the outage/recovery watcher
//! all live here so the shipped executable and the tests use one implementation.
//!
//! Outage and recovery keep the binary's state machine: last heartbeat and last
//! alert are independent timestamps, an outage is recorded only after every
//! enabled backend accepts the alert, and recovery fires only after that.

mod monitor;

use actix_web::{get, web, App, HttpResponse, HttpServer};
use log::{info, warn};
use monitor::{
    evaluate_check, evaluate_heartbeat, format_outage_message, CheckDecision, HeartbeatDecision,
};
use once_cell::sync::Lazy;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use std::{
    env,
    sync::{Arc, Mutex},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
use tokio::{spawn, time};

/// Process start time (unix seconds) for health reporting.
static STARTED_AT_UNIX: Lazy<u64> = Lazy::new(|| {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
});

const DEFAULT_OUTAGE_MESSAGE: &str = "❌ Poop Monitor is offline!";
const DEFAULT_RECOVERY_MESSAGE: &str = "✅ Poop Monitor is back online!";
const DEFAULT_PUSHOVER_URL: &str = "https://api.pushover.net/1/messages.json";
const DEFAULT_NTFY_SERVER: &str = "https://ntfy.sh";
const DEFAULT_PORT: u16 = 3000;

/// Pushover credentials and the messages endpoint (overridable for tests).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PushoverConfig {
    pub token: String,
    pub user: String,
    /// Full messages URL. Defaults to the public Pushover API.
    pub url: String,
}

/// ntfy topic / server settings.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NtfyConfig {
    /// Base server URL, e.g. `https://ntfy.sh` (no trailing slash).
    pub server: String,
    pub topic: String,
    /// Optional bearer token for private topics.
    pub token: Option<String>,
}

/// Enabled notification backends (any combination; at least one required at startup).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NotifyConfig {
    pub pushover: Option<PushoverConfig>,
    pub ntfy: Option<NtfyConfig>,
    pub outage_message: String,
    pub recovery_message: String,
}

fn env_non_empty(key: &str) -> Option<String> {
    env::var(key).ok().filter(|value| !value.is_empty())
}

impl NotifyConfig {
    /// Load from environment variables.
    ///
    /// Empty values are treated as unset so a container can ship blank
    /// `PUSHOVER_*` defaults and still start on ntfy alone.
    ///
    /// - Pushover: `PUSHOVER_TOKEN` + `PUSHOVER_USER` (both required to enable).
    ///   Optional `PUSHOVER_URL` (default `https://api.pushover.net/1/messages.json`).
    /// - ntfy: `NTFY_TOPIC` (required to enable). Optional `NTFY_SERVER`
    ///   (default `https://ntfy.sh`) and `NTFY_TOKEN`.
    /// - Copy: `OUTAGE_MESSAGE`, `RECOVERY_MESSAGE`.
    pub fn from_env() -> Result<Self, String> {
        let pushover = match (
            env_non_empty("PUSHOVER_TOKEN"),
            env_non_empty("PUSHOVER_USER"),
        ) {
            (Some(token), Some(user)) => Some(PushoverConfig {
                token,
                user,
                url: env_non_empty("PUSHOVER_URL")
                    .unwrap_or_else(|| DEFAULT_PUSHOVER_URL.to_string()),
            }),
            (None, None) => None,
            _ => {
                return Err(
                    "Both PUSHOVER_TOKEN and PUSHOVER_USER must be set to enable Pushover".into(),
                );
            }
        };

        let ntfy = match env_non_empty("NTFY_TOPIC") {
            Some(topic) => {
                let server = env_non_empty("NTFY_SERVER")
                    .unwrap_or_else(|| DEFAULT_NTFY_SERVER.to_string())
                    .trim_end_matches('/')
                    .to_string();
                let token = env_non_empty("NTFY_TOKEN");
                Some(NtfyConfig {
                    server,
                    topic,
                    token,
                })
            }
            None => None,
        };

        if pushover.is_none() && ntfy.is_none() {
            return Err(
                "At least one notification backend is required: set PUSHOVER_TOKEN+PUSHOVER_USER and/or NTFY_TOPIC"
                    .into(),
            );
        }

        Ok(Self {
            pushover,
            ntfy,
            outage_message: env_non_empty("OUTAGE_MESSAGE")
                .unwrap_or_else(|| DEFAULT_OUTAGE_MESSAGE.to_string()),
            recovery_message: env_non_empty("RECOVERY_MESSAGE")
                .unwrap_or_else(|| DEFAULT_RECOVERY_MESSAGE.to_string()),
        })
    }

    /// Names of enabled backends (for health / logging).
    pub fn backend_names(&self) -> Vec<&'static str> {
        let mut names = Vec::new();
        if self.pushover.is_some() {
            names.push("pushover");
        }
        if self.ntfy.is_some() {
            names.push("ntfy");
        }
        names
    }
}

/// Runtime tuning for the staleness checker.
#[derive(Clone, Debug)]
pub struct WatcherConfig {
    pub timeout_secs: u64,
    pub check_interval_secs: u64,
    pub debounce_secs: u64,
}

impl WatcherConfig {
    pub fn from_env() -> Self {
        let timeout_secs: u64 = env::var("HEARTBEAT_TIMEOUT_SECS")
            .unwrap_or_else(|_| "90".into())
            .parse()
            .expect("HEARTBEAT_TIMEOUT_SECS must be a number");
        let check_interval_secs: u64 = env::var("CHECK_INTERVAL_SECS")
            .unwrap_or_else(|_| "10".into())
            .parse()
            .expect("CHECK_INTERVAL_SECS must be a number");
        let debounce_secs: u64 = env::var("DEBOUNCE_SECS")
            .unwrap_or_else(|_| "300".into())
            .parse()
            .expect("DEBOUNCE_SECS must be a number");
        Self {
            timeout_secs,
            check_interval_secs,
            debounce_secs,
        }
    }
}

fn port_from_env() -> u16 {
    match env_non_empty("PORT") {
        Some(value) => value.parse().expect("PORT must be a number"),
        None => DEFAULT_PORT,
    }
}

fn heartbeat_secret_from_env() -> Result<String, String> {
    env_non_empty("HEARTBEAT_SECRET")
        .ok_or_else(|| "HEARTBEAT_SECRET must be set to protect /heartbeat/poop".into())
}

/// POST one Pushover message. `true` only when the HTTP status is success.
async fn send_pushover(client: &Client, pushover: &PushoverConfig, message: &str) -> bool {
    let params = [
        ("token", pushover.token.as_str()),
        ("user", pushover.user.as_str()),
        ("message", message),
    ];
    match client.post(&pushover.url).form(&params).send().await {
        Ok(response) => {
            let status = response.status();
            let _ = response.bytes().await;
            if status.is_success() {
                info!("Pushover status: {}", status);
                true
            } else {
                warn!("Pushover request failed with status: {}", status);
                false
            }
        }
        Err(err) => {
            warn!("Failed to send Pushover: {}", err);
            false
        }
    }
}

/// POST one ntfy message. `true` only when the HTTP status is success.
async fn send_ntfy(client: &Client, ntfy: &NtfyConfig, message: &str) -> bool {
    let url = format!("{}/{}", ntfy.server, ntfy.topic);
    let mut request = client
        .post(&url)
        .header("Title", "Notification API")
        .body(message.to_string());
    if let Some(token) = &ntfy.token {
        request = request.bearer_auth(token);
    }
    match request.send().await {
        Ok(response) => {
            let status = response.status();
            let _ = response.bytes().await;
            if status.is_success() {
                info!("ntfy status: {}", status);
                true
            } else {
                warn!("ntfy request failed with status: {}", status);
                false
            }
        }
        Err(err) => {
            warn!("Failed to send ntfy: {}", err);
            false
        }
    }
}

/// Send `message` on every enabled backend.
///
/// Every backend is attempted. The send counts as successful only when each
/// enabled backend returns HTTP success, which is what gates the outage phase.
pub async fn send_alert(client: &Client, config: &NotifyConfig, message: &str) -> bool {
    let mut attempted = 0;
    let mut failures = 0;

    if let Some(pushover) = &config.pushover {
        attempted += 1;
        if !send_pushover(client, pushover, message).await {
            failures += 1;
        }
    }
    if let Some(ntfy) = &config.ntfy {
        attempted += 1;
        if !send_ntfy(client, ntfy, message).await {
            failures += 1;
        }
    }

    attempted > 0 && failures == 0
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct HealthResponse {
    pub status: String,
    pub uptime_secs: u64,
    pub backends: Vec<String>,
    pub last_heartbeat_secs_ago: u64,
}

fn uptime_secs() -> u64 {
    let started = *STARTED_AT_UNIX;
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs().saturating_sub(started))
        .unwrap_or(0)
}

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
    /// `last_seen` is not reset: downtime stays measured from the last heartbeat.
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

/// Process health for probes (liveness).
#[get("/healthz")]
pub async fn healthz(
    notify: web::Data<NotifyConfig>,
    monitor: web::Data<Mutex<MonitorState>>,
) -> HttpResponse {
    let last_heartbeat_secs_ago = monitor
        .lock()
        .map(|last| last.last_seen.elapsed().as_secs())
        .unwrap_or(0);

    HttpResponse::Ok().json(HealthResponse {
        status: "ok".to_string(),
        uptime_secs: uptime_secs(),
        backends: notify
            .backend_names()
            .into_iter()
            .map(str::to_string)
            .collect(),
        last_heartbeat_secs_ago,
    })
}

#[get("/heartbeat/poop")]
async fn heartbeat(
    request: actix_web::HttpRequest,
    client: web::Data<Client>,
    cfg: web::Data<NotifyConfig>,
    watcher: web::Data<WatcherConfig>,
    monitor: web::Data<Mutex<MonitorState>>,
    secret: web::Data<String>,
) -> HttpResponse {
    let expected = format!("Bearer {}", secret.get_ref());
    let authorized = request
        .headers()
        .get(actix_web::http::header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value == expected.as_str());
    if !authorized {
        return HttpResponse::Unauthorized().finish();
    }

    let (alert, recovery_message) = {
        let mut mon = monitor.lock().unwrap();
        let now = Instant::now();
        let now_secs = mon.secs_since_epoch(now);
        let prev_secs = mon.secs_since_epoch(mon.last_seen);
        let in_outage = mon.phase == Phase::Outage;
        let decision = evaluate_heartbeat(now_secs, prev_secs, in_outage, watcher.timeout_secs);
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
        if !send_alert(client.get_ref(), cfg.get_ref(), &recovery_message).await {
            warn!("Failed to send recovery alert");
        }
    }

    HttpResponse::Ok().body("OK")
}

/// Register the routes the shipped binary serves.
pub fn configure_app(cfg: &mut web::ServiceConfig) {
    cfg.service(healthz).service(heartbeat);
}

/// Application factory shared by [`run`] and the in-crate tests.
pub(crate) fn create_app(
    notify: NotifyConfig,
    watcher: WatcherConfig,
    monitor: Arc<Mutex<MonitorState>>,
    client: Client,
    heartbeat_secret: String,
) -> App<
    impl actix_web::dev::ServiceFactory<
        actix_web::dev::ServiceRequest,
        Config = (),
        Response = actix_web::dev::ServiceResponse,
        Error = actix_web::Error,
        InitError = (),
    >,
> {
    let _ = uptime_secs();
    App::new()
        .app_data(web::Data::new(notify))
        .app_data(web::Data::new(watcher))
        .app_data(web::Data::from(monitor))
        .app_data(web::Data::new(client))
        .app_data(web::Data::new(heartbeat_secret))
        .configure(configure_app)
}

/// One staleness pass. Sleeping and the decision both live with the caller so
/// tests can exercise delivery without waiting on the check interval.
async fn poll_staleness(
    notify: &NotifyConfig,
    watcher: &WatcherConfig,
    monitor: &Mutex<MonitorState>,
    client: &Client,
) {
    let (decision, outage_message) = {
        let mon = monitor.lock().unwrap();
        let now_secs = mon.now_secs();
        let last_hb = mon.secs_since_epoch(mon.last_seen);
        let last_alert = mon.last_alert.map(|instant| mon.secs_since_epoch(instant));
        let decision = evaluate_check(
            now_secs,
            last_hb,
            last_alert,
            watcher.timeout_secs,
            watcher.debounce_secs,
        );
        (decision, notify.outage_message.clone())
    };

    match decision {
        CheckDecision::Healthy {
            secs_since_heartbeat,
        } => {
            info!(
                "Heartbeat fresh ({}s ago, timeout {}s)",
                secs_since_heartbeat, watcher.timeout_secs
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
                watcher.debounce_secs
            );
        }
        CheckDecision::AlertOutage {
            secs_since_heartbeat,
        } => {
            warn!(
                "No heartbeat for {}s (> {}s). Sending outage alert via {:?}.",
                secs_since_heartbeat,
                watcher.timeout_secs,
                notify.backend_names()
            );
            let alert_msg = format_outage_message(&outage_message, secs_since_heartbeat);
            let sent = send_alert(client, notify, &alert_msg).await;
            let mut mon = monitor.lock().unwrap();
            mon.record_outage_result(sent);
            if !sent {
                warn!("Failed to send outage alert; leaving monitor phase and last_seen unchanged");
            }
        }
    }
}

fn spawn_staleness_watcher(
    notify: NotifyConfig,
    watcher: WatcherConfig,
    monitor: Arc<Mutex<MonitorState>>,
    client: Client,
) {
    spawn(async move {
        loop {
            time::sleep(Duration::from_secs(watcher.check_interval_secs)).await;
            poll_staleness(&notify, &watcher, &monitor, &client).await;
        }
    });
}

/// Serve the shared app on `0.0.0.0:$PORT` (default 3000) and watch heartbeats.
///
/// This is the process the Dockerfile runs.
pub async fn run(notify: NotifyConfig, watcher: WatcherConfig) -> std::io::Result<()> {
    let heartbeat_secret = heartbeat_secret_from_env().expect("invalid heartbeat configuration");
    let monitor = Arc::new(Mutex::new(MonitorState::new()));
    let client = Client::new();
    info!(
        "Notification backends: {}",
        notify.backend_names().join(", ")
    );
    spawn_staleness_watcher(
        notify.clone(),
        watcher.clone(),
        Arc::clone(&monitor),
        client.clone(),
    );

    let port = port_from_env();
    info!("Listening on 0.0.0.0:{}", port);
    HttpServer::new(move || {
        create_app(
            notify.clone(),
            watcher.clone(),
            Arc::clone(&monitor),
            client.clone(),
            heartbeat_secret.clone(),
        )
    })
    .bind(("0.0.0.0", port))?
    .run()
    .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use actix_web::{
        http::StatusCode, test as aw_test, web, App, HttpRequest, HttpResponse, HttpServer,
    };
    use std::time::Duration;

    static ENV_MUTEX: Mutex<()> = Mutex::new(());

    struct EnvGuard;

    impl EnvGuard {
        fn enter() -> Self {
            clear_notify_env();
            EnvGuard
        }
    }

    impl Drop for EnvGuard {
        fn drop(&mut self) {
            clear_notify_env();
        }
    }

    fn env_lock() -> std::sync::MutexGuard<'static, ()> {
        ENV_MUTEX.lock().unwrap_or_else(|err| err.into_inner())
    }

    fn clear_notify_env() {
        for key in [
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
            "HEARTBEAT_SECRET",
        ] {
            std::env::remove_var(key);
        }
    }

    fn sample_notify(pushover: Option<PushoverConfig>, ntfy: Option<NtfyConfig>) -> NotifyConfig {
        NotifyConfig {
            pushover,
            ntfy,
            outage_message: DEFAULT_OUTAGE_MESSAGE.to_string(),
            recovery_message: DEFAULT_RECOVERY_MESSAGE.to_string(),
        }
    }

    fn sample_pushover(url: &str) -> PushoverConfig {
        PushoverConfig {
            token: "REPLACE_ME".into(),
            user: "REPLACE_ME".into(),
            url: url.into(),
        }
    }

    fn sample_ntfy(server: &str, topic: &str) -> NtfyConfig {
        NtfyConfig {
            server: server.into(),
            topic: topic.into(),
            token: Some("REPLACE_ME".into()),
        }
    }

    fn sample_watcher() -> WatcherConfig {
        WatcherConfig {
            timeout_secs: 90,
            check_interval_secs: 3600,
            debounce_secs: 300,
        }
    }

    fn test_client() -> Client {
        Client::builder()
            .timeout(Duration::from_secs(5))
            .build()
            .expect("client")
    }

    #[derive(Clone, Debug)]
    struct Hit {
        path: String,
        body: String,
        auth: Option<String>,
        title: Option<String>,
    }

    struct MockServer {
        port: u16,
        hits: Arc<Mutex<Vec<Hit>>>,
    }

    async fn record_hit(
        req: HttpRequest,
        hits: web::Data<Mutex<Vec<Hit>>>,
        body: web::Bytes,
    ) -> HttpResponse {
        let path = req.path().to_string();
        let auth = req
            .headers()
            .get(actix_web::http::header::AUTHORIZATION)
            .and_then(|value| value.to_str().ok())
            .map(str::to_string);
        let title = req
            .headers()
            .get("Title")
            .and_then(|value| value.to_str().ok())
            .map(str::to_string);
        hits.lock().unwrap().push(Hit {
            path: path.clone(),
            body: String::from_utf8_lossy(&body).into_owned(),
            auth,
            title,
        });
        if path.contains("fail") {
            HttpResponse::BadRequest().body("nope")
        } else {
            HttpResponse::Ok().body("ok")
        }
    }

    async fn spawn_mock() -> MockServer {
        let hits = Arc::new(Mutex::new(Vec::new()));
        let hits_for_app = Arc::clone(&hits);
        let server = HttpServer::new(move || {
            let hits = Arc::clone(&hits_for_app);
            App::new()
                .app_data(web::Data::from(hits))
                .default_service(web::to(record_hit))
        })
        .bind(("127.0.0.1", 0))
        .expect("bind mock server");
        let port = server.addrs()[0].port();
        tokio::spawn(server.run());

        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            if std::net::TcpStream::connect(("127.0.0.1", port)).is_ok() {
                return MockServer { port, hits };
            }
            time::sleep(Duration::from_millis(20)).await;
        }
        panic!("mock server did not listen on {port}");
    }

    fn hits_of(mock: &MockServer) -> Vec<Hit> {
        mock.hits.lock().unwrap().clone()
    }

    fn decoded_body(body: &str) -> String {
        body.replace('+', " ")
            .replace("%20", " ")
            .replace("%21", "!")
            .replace("%28", "(")
            .replace("%29", ")")
    }

    fn downtime_secs(body: &str) -> u64 {
        let decoded = decoded_body(body);
        let key = "down for ";
        let idx = decoded
            .find(key)
            .unwrap_or_else(|| panic!("no downtime in body {body}"));
        let digits: String = decoded[idx + key.len()..]
            .chars()
            .take_while(|c| c.is_ascii_digit())
            .collect();
        digits
            .parse()
            .unwrap_or_else(|_| panic!("downtime digits in {decoded}"))
    }

    /// Place the last heartbeat `age` in the past without moving `epoch` after it.
    fn age_heartbeat(mon: &mut MonitorState, age: Duration) {
        let now = Instant::now();
        mon.epoch = now.checked_sub(age).expect("age fits");
        mon.last_seen = mon.epoch;
        mon.last_alert = None;
        mon.phase = Phase::Healthy;
    }

    macro_rules! shipped_app {
        ($notify:expr, $monitor:expr) => {
            aw_test::init_service(create_app(
                $notify,
                sample_watcher(),
                $monitor,
                test_client(),
                "REPLACE_ME".into(),
            ))
            .await
        };
    }

    #[test]
    fn notify_config_backend_names() {
        let both = sample_notify(
            Some(sample_pushover(DEFAULT_PUSHOVER_URL)),
            Some(NtfyConfig {
                server: "https://ntfy.sh".into(),
                topic: "alerts".into(),
                token: None,
            }),
        );
        assert_eq!(both.backend_names(), vec!["pushover", "ntfy"]);

        let only_ntfy = sample_notify(None, Some(sample_ntfy("https://ntfy.example", "x")));
        assert_eq!(only_ntfy.backend_names(), vec!["ntfy"]);

        let empty = sample_notify(None, None);
        assert!(empty.backend_names().is_empty());
    }

    #[test]
    fn heartbeat_secret_must_be_configured() {
        let _lock = env_lock();
        let _clean = EnvGuard::enter();
        assert!(heartbeat_secret_from_env().is_err());
        std::env::set_var("HEARTBEAT_SECRET", "REPLACE_ME");
        assert_eq!(heartbeat_secret_from_env().unwrap(), "REPLACE_ME");
        std::env::set_var("HEARTBEAT_SECRET", "");
        assert!(heartbeat_secret_from_env().is_err());
    }

    #[test]
    fn notify_config_from_env_pushover_and_ntfy() {
        let _lock = env_lock();
        let _clean = EnvGuard::enter();
        std::env::set_var("PUSHOVER_TOKEN", "REPLACE_ME");
        std::env::set_var("PUSHOVER_USER", "REPLACE_ME");
        std::env::set_var("PUSHOVER_URL", "http://127.0.0.1:9/messages.json");
        std::env::set_var("NTFY_TOPIC", "mytopic");
        std::env::set_var("NTFY_SERVER", "https://ntfy.example.com/");
        std::env::set_var("NTFY_TOKEN", "REPLACE_ME");
        std::env::set_var("OUTAGE_MESSAGE", "down");
        std::env::set_var("RECOVERY_MESSAGE", "up");

        let cfg = NotifyConfig::from_env().expect("should load");
        assert_eq!(cfg.backend_names(), vec!["pushover", "ntfy"]);
        assert_eq!(cfg.outage_message, "down");
        assert_eq!(cfg.recovery_message, "up");
        let pushover = cfg.pushover.expect("pushover enabled");
        assert_eq!(pushover.token, "REPLACE_ME");
        assert_eq!(pushover.user, "REPLACE_ME");
        assert_eq!(pushover.url, "http://127.0.0.1:9/messages.json");
        let ntfy = cfg.ntfy.expect("ntfy enabled");
        assert_eq!(ntfy.topic, "mytopic");
        assert_eq!(ntfy.server, "https://ntfy.example.com");
        assert_eq!(ntfy.token.as_deref(), Some("REPLACE_ME"));
    }

    #[test]
    fn notify_config_from_env_ntfy_only_ignores_empty_pushover() {
        let _lock = env_lock();
        let _clean = EnvGuard::enter();
        // Dockerfile defaults and a blank .env set these to empty strings.
        std::env::set_var("PUSHOVER_TOKEN", "");
        std::env::set_var("PUSHOVER_USER", "");
        std::env::set_var("NTFY_TOPIC", "solo");
        std::env::set_var("NTFY_TOKEN", "");

        let cfg = NotifyConfig::from_env().expect("ntfy-only should work");
        assert!(cfg.pushover.is_none());
        let ntfy = cfg.ntfy.unwrap();
        assert_eq!(ntfy.server, DEFAULT_NTFY_SERVER);
        assert_eq!(ntfy.topic, "solo");
        assert_eq!(ntfy.token, None);
        assert_eq!(cfg.outage_message, DEFAULT_OUTAGE_MESSAGE);
        assert_eq!(cfg.recovery_message, DEFAULT_RECOVERY_MESSAGE);
    }

    #[test]
    fn notify_config_from_env_pushover_only_ignores_empty_ntfy() {
        let _lock = env_lock();
        let _clean = EnvGuard::enter();
        std::env::set_var("PUSHOVER_TOKEN", "REPLACE_ME");
        std::env::set_var("PUSHOVER_USER", "REPLACE_ME");
        std::env::set_var("NTFY_TOPIC", "");
        std::env::set_var("NTFY_SERVER", "");

        let cfg = NotifyConfig::from_env().expect("pushover-only should work");
        assert!(cfg.ntfy.is_none());
        assert_eq!(cfg.backend_names(), vec!["pushover"]);
        let pushover = cfg.pushover.unwrap();
        assert_eq!(pushover.url, DEFAULT_PUSHOVER_URL);
    }

    #[test]
    fn notify_config_from_env_rejects_empty() {
        let _lock = env_lock();
        let _clean = EnvGuard::enter();
        let err = NotifyConfig::from_env().unwrap_err();
        assert!(err.contains("At least one notification backend"));
    }

    #[test]
    fn notify_config_from_env_rejects_partial_pushover() {
        let _lock = env_lock();
        let _clean = EnvGuard::enter();
        std::env::set_var("PUSHOVER_TOKEN", "REPLACE_ME");
        std::env::set_var("NTFY_TOPIC", "solo");
        let err = NotifyConfig::from_env().unwrap_err();
        assert!(err.contains("Both PUSHOVER_TOKEN and PUSHOVER_USER"));
    }

    #[test]
    fn watcher_config_from_env_defaults_and_overrides() {
        let _lock = env_lock();
        let _clean = EnvGuard::enter();
        let defaults = WatcherConfig::from_env();
        assert_eq!(defaults.timeout_secs, 90);
        assert_eq!(defaults.check_interval_secs, 10);
        assert_eq!(defaults.debounce_secs, 300);

        std::env::set_var("HEARTBEAT_TIMEOUT_SECS", "15");
        std::env::set_var("CHECK_INTERVAL_SECS", "2");
        std::env::set_var("DEBOUNCE_SECS", "40");
        let custom = WatcherConfig::from_env();
        assert_eq!(custom.timeout_secs, 15);
        assert_eq!(custom.check_interval_secs, 2);
        assert_eq!(custom.debounce_secs, 40);
    }

    #[test]
    fn ntfy_url_construction() {
        let ntfy = NtfyConfig {
            server: "https://ntfy.sh".into(),
            topic: "home-alerts".into(),
            token: None,
        };
        let url = format!("{}/{}", ntfy.server, ntfy.topic);
        assert_eq!(url, "https://ntfy.sh/home-alerts");
    }

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
        let (phase, alert) = on_staleness_check(Phase::Outage, true);
        assert_eq!(phase, Phase::Outage);
        assert_eq!(alert, Alert::Outage);
    }

    #[test]
    fn not_stale_keeps_phase_without_alert() {
        let (phase, alert) = on_staleness_check(Phase::Healthy, false);
        assert_eq!((phase, alert), (Phase::Healthy, Alert::None));

        let (phase, alert) = on_staleness_check(Phase::Outage, false);
        assert_eq!((phase, alert), (Phase::Outage, Alert::None));
    }

    #[test]
    fn full_outage_then_recovery_cycle() {
        let mut phase = Phase::Healthy;

        let (next, alert) = on_heartbeat(phase);
        phase = next;
        assert_eq!(alert, Alert::None);

        let (next, alert) = on_staleness_check(phase, true);
        phase = next;
        assert_eq!(phase, Phase::Outage);
        assert_eq!(alert, Alert::Outage);

        let (next, alert) = on_heartbeat(phase);
        phase = next;
        assert_eq!(phase, Phase::Healthy);
        assert_eq!(alert, Alert::Recovery);

        let (_, alert) = on_heartbeat(phase);
        assert_eq!(alert, Alert::None);
    }

    #[test]
    fn recovery_only_after_prior_outage_alert() {
        assert_eq!(on_heartbeat(Phase::Healthy).1, Alert::None);

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
    fn monitor_state_outage_alert_does_not_reset_last_seen() {
        let mut mon = MonitorState::new();
        let initial_last_seen = mon.last_seen;

        mon.record_outage_result(true);

        assert_eq!(mon.last_seen, initial_last_seen);
        assert_eq!(mon.phase, Phase::Outage);
        assert!(mon.last_alert.is_some());
    }

    #[test]
    fn failed_outage_send_does_not_clear_last_seen_or_advance_phase() {
        let mut mon = MonitorState::new();
        mon.last_seen = Instant::now() - Duration::from_secs(100);
        let (is_stale, alert) = mon.check_staleness(90);
        assert!(is_stale);
        assert_eq!(alert, Alert::Outage);

        mon.record_outage_result(false);
        assert_eq!(mon.phase, Phase::Healthy);
        assert_eq!(mon.last_alert, None);
        assert!(mon.last_seen.elapsed().as_secs() >= 100);

        let (is_stale_next, alert_next) = mon.check_staleness(90);
        assert!(is_stale_next);
        assert_eq!(alert_next, Alert::Outage);

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

        mon.record_outage_result(true);
        assert_eq!(mon.phase, Phase::Outage);
        assert!(mon.last_alert.is_some());
        assert!(mon.last_seen.elapsed().as_secs() >= 100);

        let hb_alert = mon.record_heartbeat();
        assert_eq!(hb_alert, Alert::Recovery);
        assert_eq!(mon.phase, Phase::Healthy);
        assert_eq!(mon.last_alert, None);

        let hb_alert2 = mon.record_heartbeat();
        assert_eq!(hb_alert2, Alert::None);
    }

    #[test]
    fn failed_pushover_send_does_not_start_debounce_window() {
        let mut mon = MonitorState::new();
        let timeout_secs = 90;
        let debounce_secs = 300;
        let last_hb_secs = 0;

        let decision = evaluate_check(
            100,
            last_hb_secs,
            mon.last_alert.map(|instant| mon.secs_since_epoch(instant)),
            timeout_secs,
            debounce_secs,
        );
        assert_eq!(
            decision,
            CheckDecision::AlertOutage {
                secs_since_heartbeat: 100
            }
        );

        let send_success = false;
        if send_success {
            let (next, _) = on_staleness_check(mon.phase, true);
            mon.phase = next;
            mon.last_alert = Some(Instant::now());
        }

        assert!(mon.last_alert.is_none());
        assert_eq!(mon.phase, Phase::Healthy);

        let decision = evaluate_check(
            110,
            last_hb_secs,
            mon.last_alert.map(|instant| mon.secs_since_epoch(instant)),
            timeout_secs,
            debounce_secs,
        );
        assert_eq!(
            decision,
            CheckDecision::AlertOutage {
                secs_since_heartbeat: 110
            }
        );

        let (_, alert) = on_heartbeat(mon.phase);
        assert_eq!(alert, Alert::None);
    }

    #[actix_web::test]
    async fn send_pushover_checks_http_status_codes() {
        async fn status_app(status: StatusCode) -> HttpResponse {
            HttpResponse::build(status).body("{\"status\":0}")
        }

        let server = HttpServer::new(|| {
            App::new()
                .route(
                    "/ok",
                    web::post().to(|| async { HttpResponse::Ok().body("{\"status\":1}") }),
                )
                .route(
                    "/client_error",
                    web::post().to(|| async { status_app(StatusCode::BAD_REQUEST).await }),
                )
                .route(
                    "/unauthorized",
                    web::post().to(|| async { status_app(StatusCode::UNAUTHORIZED).await }),
                )
                .route(
                    "/server_error",
                    web::post()
                        .to(|| async { status_app(StatusCode::INTERNAL_SERVER_ERROR).await }),
                )
        })
        .bind(("127.0.0.1", 0))
        .expect("bind mock server");

        let port = server.addrs()[0].port();
        tokio::spawn(server.run());
        let client = test_client();

        let mut cfg = sample_pushover(&format!("http://127.0.0.1:{port}/ok"));
        assert!(send_pushover(&client, &cfg, "msg").await);

        cfg.url = format!("http://127.0.0.1:{port}/client_error");
        assert!(!send_pushover(&client, &cfg, "msg").await);

        cfg.url = format!("http://127.0.0.1:{port}/unauthorized");
        assert!(!send_pushover(&client, &cfg, "msg").await);

        cfg.url = format!("http://127.0.0.1:{port}/server_error");
        assert!(!send_pushover(&client, &cfg, "msg").await);

        cfg.url = "http://127.0.0.1:1/nonexistent".into();
        assert!(!send_pushover(&client, &cfg, "msg").await);
    }

    #[actix_web::test]
    async fn send_alert_reaches_pushover_only_synthetic_endpoint() {
        let mock = spawn_mock().await;
        let notify = sample_notify(
            Some(sample_pushover(&format!(
                "http://127.0.0.1:{}/pushover",
                mock.port
            ))),
            None,
        );
        let ok = send_alert(&test_client(), &notify, "synthetic-alert").await;
        assert!(ok);
        let hits = hits_of(&mock);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].path, "/pushover");
        assert!(hits[0].body.contains("token=REPLACE_ME"));
        assert!(hits[0].body.contains("user=REPLACE_ME"));
        assert!(hits[0].body.contains("message=synthetic-alert"));
        assert!(hits[0].auth.is_none());
    }

    #[actix_web::test]
    async fn send_alert_reaches_ntfy_only_synthetic_endpoint() {
        let mock = spawn_mock().await;
        let notify = sample_notify(
            None,
            Some(sample_ntfy(
                &format!("http://127.0.0.1:{}", mock.port),
                "solo",
            )),
        );
        let ok = send_alert(&test_client(), &notify, "synthetic-alert").await;
        assert!(ok);
        let hits = hits_of(&mock);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].path, "/solo");
        assert_eq!(hits[0].body, "synthetic-alert");
        assert_eq!(hits[0].title.as_deref(), Some("Notification API"));
        assert_eq!(hits[0].auth.as_deref(), Some("Bearer REPLACE_ME"));
    }

    #[actix_web::test]
    async fn send_alert_reaches_combined_synthetic_endpoints() {
        let mock = spawn_mock().await;
        let server = format!("http://127.0.0.1:{}", mock.port);
        let notify = sample_notify(
            Some(sample_pushover(&format!("{server}/pushover"))),
            Some(sample_ntfy(&server, "alerts")),
        );
        let ok = send_alert(&test_client(), &notify, "synthetic-alert").await;
        assert!(ok);
        let hits = hits_of(&mock);
        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0].path, "/pushover");
        assert!(hits[0].body.contains("message=synthetic-alert"));
        assert_eq!(hits[1].path, "/alerts");
        assert_eq!(hits[1].body, "synthetic-alert");
    }

    #[actix_web::test]
    async fn send_alert_fails_when_one_combined_backend_fails() {
        let mock = spawn_mock().await;
        let server = format!("http://127.0.0.1:{}", mock.port);
        let notify = sample_notify(
            Some(sample_pushover(&format!("{server}/pushover"))),
            Some(sample_ntfy(&server, "fail")),
        );
        let ok = send_alert(&test_client(), &notify, "synthetic-alert").await;
        assert!(!ok);
        let hits = hits_of(&mock);
        assert_eq!(hits.len(), 2, "both backends are still attempted");
        assert_eq!(hits[0].path, "/pushover");
        assert_eq!(hits[1].path, "/fail");
    }

    #[actix_web::test]
    async fn poll_ntfy_only_records_outage_without_resetting_last_seen() {
        let mock = spawn_mock().await;
        let notify = sample_notify(
            None,
            Some(sample_ntfy(
                &format!("http://127.0.0.1:{}", mock.port),
                "solo",
            )),
        );
        let monitor = Mutex::new(MonitorState::new());
        let age = Duration::from_secs(1000);
        {
            let mut mon = monitor.lock().unwrap();
            age_heartbeat(&mut mon, age);
        }
        let seen_before = monitor.lock().unwrap().last_seen;

        poll_staleness(&notify, &sample_watcher(), &monitor, &test_client()).await;

        let hits = hits_of(&mock);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].path, "/solo");
        let down_for = downtime_secs(&hits[0].body);
        assert!(
            (990..=1010).contains(&down_for),
            "measured downtime {down_for} must follow the last heartbeat, not the alert"
        );
        let mon = monitor.lock().unwrap();
        assert_eq!(mon.phase, Phase::Outage);
        assert_eq!(mon.last_seen, seen_before);
        assert!(mon.last_alert.is_some());
    }

    #[actix_web::test]
    async fn poll_combined_failure_does_not_start_debounce_or_outage() {
        let mock = spawn_mock().await;
        let server = format!("http://127.0.0.1:{}", mock.port);
        let notify = sample_notify(
            Some(sample_pushover(&format!("{server}/pushover"))),
            Some(sample_ntfy(&server, "fail")),
        );
        let monitor = Mutex::new(MonitorState::new());
        {
            let mut mon = monitor.lock().unwrap();
            age_heartbeat(&mut mon, Duration::from_secs(1000));
        }

        let watcher = sample_watcher();
        let client = test_client();
        poll_staleness(&notify, &watcher, &monitor, &client).await;
        poll_staleness(&notify, &watcher, &monitor, &client).await;

        let hits = hits_of(&mock);
        assert_eq!(hits.len(), 4, "failed delivery retries on the next check");
        let mon = monitor.lock().unwrap();
        assert_eq!(mon.phase, Phase::Healthy);
        assert!(mon.last_alert.is_none());
        assert!(mon.last_seen.elapsed().as_secs() >= 990);
    }

    #[actix_web::test]
    async fn poll_debounce_suppresses_realert_and_keeps_true_downtime() {
        let mock = spawn_mock().await;
        let notify = sample_notify(
            Some(sample_pushover(&format!(
                "http://127.0.0.1:{}/pushover",
                mock.port
            ))),
            None,
        );
        let monitor = Mutex::new(MonitorState::new());
        {
            let mut mon = monitor.lock().unwrap();
            age_heartbeat(&mut mon, Duration::from_secs(1000));
        }
        let watcher = sample_watcher();
        let client = test_client();
        poll_staleness(&notify, &watcher, &monitor, &client).await;
        poll_staleness(&notify, &watcher, &monitor, &client).await;
        assert_eq!(
            hits_of(&mock).len(),
            1,
            "second pass is inside the debounce window"
        );

        {
            let mut mon = monitor.lock().unwrap();
            // Previous alert is old enough that debounce allows another send.
            // last_seen stays at the original heartbeat.
            mon.last_alert = Some(mon.epoch + Duration::from_secs(100));
        }
        poll_staleness(&notify, &watcher, &monitor, &client).await;
        let hits = hits_of(&mock);
        assert_eq!(hits.len(), 2);
        let down_for = downtime_secs(&hits[1].body);
        assert!(
            (990..=1010).contains(&down_for),
            "re-alert downtime {down_for} must not collapse to the debounce interval"
        );
    }

    #[actix_web::test]
    async fn healthz_on_shared_factory_reports_each_backend_mode() {
        let cases = [
            (
                sample_notify(Some(sample_pushover(DEFAULT_PUSHOVER_URL)), None),
                vec!["pushover".to_string()],
            ),
            (
                sample_notify(None, Some(sample_ntfy("https://ntfy.example", "solo"))),
                vec!["ntfy".to_string()],
            ),
            (
                sample_notify(
                    Some(sample_pushover(DEFAULT_PUSHOVER_URL)),
                    Some(sample_ntfy("https://ntfy.example", "both")),
                ),
                vec!["pushover".to_string(), "ntfy".to_string()],
            ),
        ];

        for (notify, backends) in cases {
            let monitor = Arc::new(Mutex::new(MonitorState::new()));
            let app = shipped_app!(notify, Arc::clone(&monitor));
            let req = aw_test::TestRequest::get().uri("/healthz").to_request();
            let resp = aw_test::call_service(&app, req).await;
            assert!(resp.status().is_success());
            let body: HealthResponse = aw_test::read_body_json(resp).await;
            assert_eq!(body.status, "ok");
            assert_eq!(body.backends, backends);
            assert!(body.last_heartbeat_secs_ago < 5);
        }
    }

    #[actix_web::test]
    async fn heartbeat_and_healthz_share_last_seen_on_shipped_factory() {
        let monitor = Arc::new(Mutex::new(MonitorState::new()));
        {
            let mut mon = monitor.lock().unwrap();
            age_heartbeat(&mut mon, Duration::from_secs(50));
        }
        let app = shipped_app!(
            sample_notify(None, Some(sample_ntfy("http://127.0.0.1:9", "solo"))),
            Arc::clone(&monitor)
        );

        let stale = aw_test::call_service(
            &app,
            aw_test::TestRequest::get().uri("/healthz").to_request(),
        )
        .await;
        let stale_body: HealthResponse = aw_test::read_body_json(stale).await;
        assert!(stale_body.last_heartbeat_secs_ago >= 49);

        let beat = aw_test::call_service(
            &app,
            aw_test::TestRequest::get()
                .uri("/heartbeat/poop")
                .insert_header((actix_web::http::header::AUTHORIZATION, "Bearer REPLACE_ME"))
                .to_request(),
        )
        .await;
        assert!(beat.status().is_success());
        let body = aw_test::read_body(beat).await;
        assert_eq!(&body[..], b"OK");

        let fresh = aw_test::call_service(
            &app,
            aw_test::TestRequest::get().uri("/healthz").to_request(),
        )
        .await;
        let fresh_body: HealthResponse = aw_test::read_body_json(fresh).await;
        assert!(fresh_body.last_heartbeat_secs_ago < 5);
        assert_eq!(monitor.lock().unwrap().phase, Phase::Healthy);
    }

    #[actix_web::test]
    async fn unauthorized_heartbeat_does_not_refresh_liveness_or_send_recovery() {
        let mock = spawn_mock().await;
        let server = format!("http://127.0.0.1:{}", mock.port);
        let notify = sample_notify(
            Some(sample_pushover(&format!("{server}/pushover"))),
            Some(sample_ntfy(&server, "alerts")),
        );
        let monitor = Arc::new(Mutex::new(MonitorState::new()));
        {
            let mut mon = monitor.lock().unwrap();
            age_heartbeat(&mut mon, Duration::from_secs(200));
            mon.phase = Phase::Outage;
            mon.last_alert = Some(mon.epoch + Duration::from_secs(100));
        }
        let last_seen = monitor.lock().unwrap().last_seen;
        let app = shipped_app!(notify, Arc::clone(&monitor));

        for authorization in [None, Some("Bearer incorrect")] {
            let mut request = aw_test::TestRequest::get().uri("/heartbeat/poop");
            if let Some(value) = authorization {
                request = request.insert_header((actix_web::http::header::AUTHORIZATION, value));
            }
            let response = aw_test::call_service(&app, request.to_request()).await;
            assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        }

        let mon = monitor.lock().unwrap();
        assert_eq!(mon.last_seen, last_seen);
        assert_eq!(mon.phase, Phase::Outage);
        assert!(hits_of(&mock).is_empty());
    }

    #[actix_web::test]
    async fn heartbeat_after_outage_sends_recovery_to_configured_backends() {
        let mock = spawn_mock().await;
        let server = format!("http://127.0.0.1:{}", mock.port);
        let notify = sample_notify(
            Some(sample_pushover(&format!("{server}/pushover"))),
            Some(sample_ntfy(&server, "alerts")),
        );
        let monitor = Arc::new(Mutex::new(MonitorState::new()));
        {
            let mut mon = monitor.lock().unwrap();
            age_heartbeat(&mut mon, Duration::from_secs(200));
            mon.phase = Phase::Outage;
            mon.last_alert = Some(mon.epoch + Duration::from_secs(100));
        }
        let app = shipped_app!(notify, Arc::clone(&monitor));
        let resp = aw_test::call_service(
            &app,
            aw_test::TestRequest::get()
                .uri("/heartbeat/poop")
                .insert_header((actix_web::http::header::AUTHORIZATION, "Bearer REPLACE_ME"))
                .to_request(),
        )
        .await;
        assert!(resp.status().is_success());

        let hits = hits_of(&mock);
        assert_eq!(hits.len(), 2);
        for hit in &hits {
            let decoded = decoded_body(&hit.body);
            assert!(
                decoded.contains("back online"),
                "recovery body missing from {}",
                hit.body
            );
        }
        {
            let mon = monitor.lock().unwrap();
            assert_eq!(mon.phase, Phase::Healthy);
            assert!(mon.last_alert.is_none());
        }

        let again = aw_test::call_service(
            &app,
            aw_test::TestRequest::get()
                .uri("/heartbeat/poop")
                .insert_header((actix_web::http::header::AUTHORIZATION, "Bearer REPLACE_ME"))
                .to_request(),
        )
        .await;
        assert!(again.status().is_success());
        assert_eq!(
            hits_of(&mock).len(),
            2,
            "a second heartbeat must not send another recovery"
        );
    }
}
