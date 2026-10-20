//! Heartbeat watcher with multi-channel notification backends.
//!
//! The binary in `main.rs` is a thin entrypoint over this module. `/healthz`,
//! `/heartbeat/{name}[/fail]`, configuration, delivery, and the outage/recovery watcher
//! all live here so the shipped executable and the tests use one implementation.
//!
//! Outage and recovery keep the binary's state machine: last heartbeat and last
//! alert are independent timestamps, an outage is recorded only after every
//! enabled backend accepts the alert, and recovery fires only after that.

mod monitor;
mod monitors;

use actix_web::{get, post, route, web, App, HttpResponse, HttpServer};
use log::{info, warn};
use monitor::{
    evaluate_check, evaluate_heartbeat, format_outage_message, CheckDecision, HeartbeatDecision,
};
pub use monitors::{normalize_name, MonitorConfig, DEFAULT_MONITOR};
#[cfg(test)]
use monitors::{DEFAULT_OUTAGE_MESSAGE, DEFAULT_RECOVERY_MESSAGE};
use once_cell::sync::Lazy;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, HashMap},
    env,
    sync::{Arc, Mutex},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
use subtle::ConstantTimeEq;
use tokio::{spawn, time};

/// Process start time (unix seconds) for health reporting.
static STARTED_AT_UNIX: Lazy<u64> = Lazy::new(|| {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
});

const DEFAULT_PUSHOVER_URL: &str = "https://api.pushover.net/1/messages.json";
const DEFAULT_PUSHOVER_RETRY: u64 = 60;
const DEFAULT_PUSHOVER_EXPIRE: u64 = 3600;
const MAX_PUSHOVER_EXPIRE: u64 = 10800;
const DEFAULT_NTFY_SERVER: &str = "https://ntfy.sh";
const DEFAULT_PORT: u16 = 3000;

/// Pushover credentials and the messages endpoint (overridable for tests).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PushoverConfig {
    pub token: String,
    pub user: String,
    /// Full messages URL. Defaults to the public Pushover API.
    pub url: String,
    pub title: Option<String>,
    pub priority: Option<i32>,
    pub retry: u64,
    pub expire: u64,
    pub sound: Option<String>,
    pub device: Option<String>,
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
    ///   Optional `PUSHOVER_URL` (default `https://api.pushover.net/1/messages.json`),
    ///   `PUSHOVER_TITLE`, `PUSHOVER_PRIORITY`, `PUSHOVER_RETRY`,
    ///   `PUSHOVER_EXPIRE`, `PUSHOVER_SOUND`, `PUSHOVER_DEVICE`.
    /// - ntfy: `NTFY_TOPIC` (required to enable). Optional `NTFY_SERVER`
    ///   (default `https://ntfy.sh`) and `NTFY_TOKEN`.
    pub fn from_env() -> Result<Self, String> {
        let pushover = match (
            env_non_empty("PUSHOVER_TOKEN"),
            env_non_empty("PUSHOVER_USER"),
        ) {
            (Some(token), Some(user)) => {
                let priority = match env_non_empty("PUSHOVER_PRIORITY") {
                    Some(val) => match val.parse::<i32>() {
                        Ok(p) if (-2..=2).contains(&p) => Some(p),
                        Ok(_) => return Err("PUSHOVER_PRIORITY must be between -2 and 2".into()),
                        Err(_) => return Err("PUSHOVER_PRIORITY must be a number".into()),
                    },
                    None => None,
                };
                let retry = env_non_empty("PUSHOVER_RETRY")
                    .map(|value| {
                        value
                            .parse::<u64>()
                            .map_err(|_| "PUSHOVER_RETRY must be a number".to_string())
                    })
                    .transpose()?
                    .unwrap_or(DEFAULT_PUSHOVER_RETRY)
                    .clamp(30, MAX_PUSHOVER_EXPIRE);
                let expire = env_non_empty("PUSHOVER_EXPIRE")
                    .map(|value| {
                        value
                            .parse::<u64>()
                            .map_err(|_| "PUSHOVER_EXPIRE must be a number".to_string())
                    })
                    .transpose()?
                    .unwrap_or(DEFAULT_PUSHOVER_EXPIRE)
                    .clamp(1, MAX_PUSHOVER_EXPIRE)
                    .max(retry);
                Some(PushoverConfig {
                    token,
                    user,
                    url: env_non_empty("PUSHOVER_URL")
                        .unwrap_or_else(|| DEFAULT_PUSHOVER_URL.to_string()),
                    title: env_non_empty("PUSHOVER_TITLE"),
                    priority,
                    retry,
                    expire,
                    sound: env_non_empty("PUSHOVER_SOUND"),
                    device: env_non_empty("PUSHOVER_DEVICE"),
                })
            }
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

        Ok(Self { pushover, ntfy })
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

/// Runtime tuning for the staleness checker and the named monitors it watches.
#[derive(Clone, Debug)]
pub struct WatcherConfig {
    pub check_interval_secs: u64,
    /// Canonical monitor name → settings. Always contains `poop`.
    pub monitors: BTreeMap<String, MonitorConfig>,
}

impl WatcherConfig {
    /// Load from the environment, panicking on malformed values like the
    /// rest of startup configuration.
    pub fn from_env() -> Self {
        Self::try_from_env().unwrap_or_else(|err| panic!("{err}"))
    }

    /// Fallible variant of [`WatcherConfig::from_env`].
    pub fn try_from_env() -> Result<Self, String> {
        let check_interval_secs: u64 = env::var("CHECK_INTERVAL_SECS")
            .unwrap_or_else(|_| "10".into())
            .parse()
            .map_err(|_| "CHECK_INTERVAL_SECS must be a number".to_string())?;
        let vars: BTreeMap<String, String> = env::vars().collect();
        Ok(Self {
            check_interval_secs,
            monitors: monitors::load_monitors(&vars)?,
        })
    }
}

fn port_from_env() -> u16 {
    match env_non_empty("PORT") {
        Some(value) => value.parse().expect("PORT must be a number"),
        None => DEFAULT_PORT,
    }
}

fn heartbeat_secret_from_env() -> Result<String, String> {
    env::var("HEARTBEAT_SECRET")
        .ok()
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty())
        .ok_or_else(|| "HEARTBEAT_SECRET must be set to protect the heartbeat routes".into())
}

/// POST one Pushover message. `true` only when the HTTP status is success.
async fn send_pushover(client: &Client, pushover: &PushoverConfig, message: &str) -> bool {
    let priority_str;
    let retry_str;
    let expire_str;
    let mut params = vec![
        ("token", pushover.token.as_str()),
        ("user", pushover.user.as_str()),
        ("message", message),
    ];
    if let Some(title) = &pushover.title {
        params.push(("title", title.as_str()));
    }
    if let Some(priority) = pushover.priority {
        priority_str = priority.to_string();
        params.push(("priority", priority_str.as_str()));
        if priority == 2 {
            retry_str = pushover.retry.to_string();
            expire_str = pushover.expire.to_string();
            params.push(("retry", retry_str.as_str()));
            params.push(("expire", expire_str.as_str()));
        }
    }
    if let Some(sound) = &pushover.sound {
        params.push(("sound", sound.as_str()));
    }
    if let Some(device) = &pushover.device {
        params.push(("device", device.as_str()));
    }
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
    /// Age of the default (`poop`) monitor's last heartbeat; kept for compatibility.
    pub last_heartbeat_secs_ago: u64,
    /// Number of configured monitors. Names are deliberately not exposed: the
    /// endpoint is unauthenticated.
    #[serde(default)]
    pub monitors_total: usize,
    /// Monitors that are `down`: an outage was alerted or the timeout has passed.
    #[serde(default)]
    pub monitors_down: usize,
}

/// Shared per-monitor state, keyed by canonical monitor name.
pub(crate) type Monitors = Mutex<HashMap<String, MonitorState>>;

/// Fresh state for every configured monitor.
pub(crate) fn new_monitors(watcher: &WatcherConfig) -> Arc<Monitors> {
    Arc::new(Mutex::new(
        watcher
            .monitors
            .keys()
            .map(|name| (name.clone(), MonitorState::new()))
            .collect(),
    ))
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
    watcher: web::Data<WatcherConfig>,
    monitors: web::Data<Monitors>,
) -> HttpResponse {
    let mut last_heartbeat_secs_ago = 0;
    let mut monitors_down = 0;
    if let Ok(states) = monitors.lock() {
        for (name, config) in &watcher.monitors {
            let Some(state) = states.get(name) else {
                continue;
            };
            let age = state.last_seen.elapsed().as_secs();
            if state.phase == Phase::Outage || age > config.timeout_secs {
                monitors_down += 1;
            }
            if name == DEFAULT_MONITOR {
                last_heartbeat_secs_ago = age;
            }
        }
    }

    HttpResponse::Ok().json(HealthResponse {
        status: "ok".to_string(),
        uptime_secs: uptime_secs(),
        backends: notify
            .backend_names()
            .into_iter()
            .map(str::to_string)
            .collect(),
        last_heartbeat_secs_ago,
        monitors_total: watcher.monitors.len(),
        monitors_down,
    })
}

/// Longest `/fail` body used as the alert message, in characters.
const MAX_FAIL_MESSAGE_CHARS: usize = 500;

/// Check the bearer secret, then resolve the path name to its configuration.
///
/// Authentication comes first so an unauthenticated caller cannot tell
/// configured names from unknown ones: a wrong or missing secret is always 401.
/// Unknown names are 404 only for callers holding the default secret (no
/// auto-provisioning).
fn authorize<'a>(
    request: &actix_web::HttpRequest,
    watcher: &'a WatcherConfig,
    default_secret: &str,
    raw_name: &str,
) -> Result<(String, &'a MonitorConfig), HttpResponse> {
    let found = normalize_name(raw_name)
        .and_then(|name| watcher.monitors.get(&name).map(|config| (name, config)));
    let secret = found
        .as_ref()
        .and_then(|(_, config)| config.secret.as_deref())
        .unwrap_or(default_secret);
    let authorized = request
        .headers()
        .get(actix_web::http::header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "))
        .is_some_and(|token| bool::from(token.as_bytes().ct_eq(secret.as_bytes())));
    if !authorized {
        return Err(HttpResponse::Unauthorized().finish());
    }
    found.ok_or_else(|| HttpResponse::NotFound().finish())
}

#[route("/heartbeat/{name}", method = "GET", method = "POST")]
async fn heartbeat(
    request: actix_web::HttpRequest,
    path: web::Path<String>,
    client: web::Data<Client>,
    cfg: web::Data<NotifyConfig>,
    watcher: web::Data<WatcherConfig>,
    monitors: web::Data<Monitors>,
    secret: web::Data<String>,
) -> HttpResponse {
    let (name, config) = match authorize(&request, &watcher, &secret, &path) {
        Ok(found) => found,
        Err(response) => return response,
    };

    let alert = {
        let mut states = monitors.lock().unwrap();
        let Some(mon) = states.get_mut(&name) else {
            return HttpResponse::NotFound().finish();
        };
        let now = Instant::now();
        let now_secs = mon.secs_since_epoch(now);
        let prev_secs = mon.secs_since_epoch(mon.last_seen);
        let in_outage = mon.phase == Phase::Outage;
        let decision = evaluate_heartbeat(now_secs, prev_secs, in_outage, config.timeout_secs);
        let alert = mon.record_heartbeat();

        match decision {
            HeartbeatDecision::Recorded => {
                info!(
                    "[{name}] Heartbeat received at {:?}; phase={:?}",
                    mon.last_seen, mon.phase
                );
            }
            HeartbeatDecision::Recovered {
                outage_duration_secs,
            } => {
                info!(
                    "[{name}] Heartbeat received at {:?} — recovered after {}s without heartbeat; phase={:?}",
                    mon.last_seen, outage_duration_secs, mon.phase
                );
            }
        }
        alert
    };

    if alert == Alert::Recovery {
        info!("[{name}] Heartbeat resumed after outage; sending recovery alert.");
        if !send_alert(client.get_ref(), cfg.get_ref(), &config.recovery_message).await {
            warn!("[{name}] Failed to send recovery alert");
        }
    }

    HttpResponse::Ok().body("OK")
}

/// Explicit failure signal: fire the outage alert now, optionally with the
/// request body as the message. `last_seen` is untouched, so the next real
/// heartbeat recovers the monitor. POST only, so prefetchers cannot fire it.
/// Repeat calls within the monitor's `debounce_secs` of the last outage alert
/// are suppressed (202).
#[post("/heartbeat/{name}/fail")]
async fn heartbeat_fail(
    request: actix_web::HttpRequest,
    path: web::Path<String>,
    body: web::Bytes,
    client: web::Data<Client>,
    cfg: web::Data<NotifyConfig>,
    watcher: web::Data<WatcherConfig>,
    monitors: web::Data<Monitors>,
) -> HttpResponse {
    let Some(secret) = request.app_data::<web::Data<String>>().cloned() else {
        return HttpResponse::InternalServerError().finish();
    };
    let (name, config) = match authorize(&request, &watcher, &secret, &path) {
        Ok(found) => found,
        Err(response) => return response,
    };

    let supplied = String::from_utf8_lossy(&body);
    let supplied = supplied.trim();
    let message: String = if supplied.is_empty() {
        config.outage_message.clone()
    } else {
        supplied.chars().take(MAX_FAIL_MESSAGE_CHARS).collect()
    };

    // Claim the debounce window under the lock so concurrent calls cannot all
    // pass the check; a failed send restores the previous value.
    let previous_alert = {
        let mut states = monitors.lock().unwrap();
        let Some(mon) = states.get_mut(&name) else {
            return HttpResponse::NotFound().finish();
        };
        let previous = mon.last_alert;
        if previous.is_some_and(|at| at.elapsed().as_secs() < config.debounce_secs) {
            info!(
                "[{name}] Failure signal suppressed: inside the {}s debounce window",
                config.debounce_secs
            );
            return HttpResponse::Accepted().body("suppressed");
        }
        mon.last_alert = Some(Instant::now());
        previous
    };

    warn!("[{name}] Failure signal received; sending outage alert.");
    let sent = send_alert(client.get_ref(), cfg.get_ref(), &message).await;
    if let Some(mon) = monitors.lock().unwrap().get_mut(&name) {
        if sent {
            mon.record_outage_result(true);
        } else {
            mon.last_alert = previous_alert;
        }
    }
    if sent {
        HttpResponse::Ok().body("OK")
    } else {
        warn!("[{name}] Failed to send failure alert");
        HttpResponse::BadGateway().body("alert delivery failed")
    }
}

/// Register the routes the shipped binary serves.
pub fn configure_app(cfg: &mut web::ServiceConfig) {
    cfg.service(healthz)
        .service(heartbeat_fail)
        .service(heartbeat);
}

/// Application factory shared by [`run`] and the in-crate tests.
pub(crate) fn create_app(
    notify: NotifyConfig,
    watcher: WatcherConfig,
    monitors: Arc<Monitors>,
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
        .app_data(web::Data::from(monitors))
        .app_data(web::Data::new(client))
        .app_data(web::Data::new(heartbeat_secret))
        .configure(configure_app)
}

/// One staleness pass over every monitor. Sleeping and the decision both live
/// with the caller so tests can exercise delivery without waiting on the check
/// interval.
async fn poll_staleness(
    notify: &NotifyConfig,
    watcher: &WatcherConfig,
    monitors: &Monitors,
    client: &Client,
) {
    for (name, config) in &watcher.monitors {
        poll_monitor(name, config, notify, monitors, client).await;
    }
}

async fn poll_monitor(
    name: &str,
    config: &MonitorConfig,
    notify: &NotifyConfig,
    monitors: &Monitors,
    client: &Client,
) {
    let decision = {
        let states = monitors.lock().unwrap();
        let Some(mon) = states.get(name) else {
            return;
        };
        let now_secs = mon.now_secs();
        let last_hb = mon.secs_since_epoch(mon.last_seen);
        let last_alert = mon.last_alert.map(|instant| mon.secs_since_epoch(instant));
        evaluate_check(
            now_secs,
            last_hb,
            last_alert,
            config.timeout_secs,
            config.debounce_secs,
        )
    };

    match decision {
        CheckDecision::Healthy {
            secs_since_heartbeat,
        } => {
            info!(
                "[{name}] Heartbeat fresh ({}s ago, timeout {}s)",
                secs_since_heartbeat, config.timeout_secs
            );
        }
        CheckDecision::StillDown {
            secs_since_heartbeat,
            secs_since_alert,
            secs_until_next_alert,
        } => {
            info!(
                "[{name}] Still down: no heartbeat for {}s; last alert {}s ago; next alert in {}s (debounce {}s)",
                secs_since_heartbeat, secs_since_alert, secs_until_next_alert, config.debounce_secs
            );
        }
        CheckDecision::AlertOutage {
            secs_since_heartbeat,
        } => {
            warn!(
                "[{name}] No heartbeat for {}s (> {}s). Sending outage alert via {:?}.",
                secs_since_heartbeat,
                config.timeout_secs,
                notify.backend_names()
            );
            let alert_msg = format_outage_message(&config.outage_message, secs_since_heartbeat);
            let sent = send_alert(client, notify, &alert_msg).await;
            if let Some(mon) = monitors.lock().unwrap().get_mut(name) {
                mon.record_outage_result(sent);
            }
            if !sent {
                warn!(
                    "[{name}] Failed to send outage alert; leaving monitor phase and last_seen unchanged"
                );
            }
        }
    }
}

fn spawn_staleness_watcher(
    notify: NotifyConfig,
    watcher: WatcherConfig,
    monitors: Arc<Monitors>,
    client: Client,
) {
    spawn(async move {
        loop {
            time::sleep(Duration::from_secs(watcher.check_interval_secs)).await;
            poll_staleness(&notify, &watcher, &monitors, &client).await;
        }
    });
}

/// Serve the shared app on `0.0.0.0:$PORT` (default 3000) and watch heartbeats.
///
/// This is the process the Dockerfile runs.
pub async fn run(notify: NotifyConfig, watcher: WatcherConfig) -> std::io::Result<()> {
    let heartbeat_secret = heartbeat_secret_from_env().expect("invalid heartbeat configuration");
    let monitors = new_monitors(&watcher);
    let client = Client::new();
    info!(
        "Notification backends: {}",
        notify.backend_names().join(", ")
    );
    info!(
        "Monitors: {}",
        watcher
            .monitors
            .keys()
            .cloned()
            .collect::<Vec<_>>()
            .join(", ")
    );
    spawn_staleness_watcher(
        notify.clone(),
        watcher.clone(),
        Arc::clone(&monitors),
        client.clone(),
    );

    let port = port_from_env();
    info!("Listening on 0.0.0.0:{}", port);
    HttpServer::new(move || {
        create_app(
            notify.clone(),
            watcher.clone(),
            Arc::clone(&monitors),
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
            "PUSHOVER_TITLE",
            "PUSHOVER_PRIORITY",
            "PUSHOVER_RETRY",
            "PUSHOVER_EXPIRE",
            "PUSHOVER_SOUND",
            "PUSHOVER_DEVICE",
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
        let extra: Vec<String> = std::env::vars()
            .map(|(key, _)| key)
            .filter(|key| key.starts_with("MONITOR_") || key == "MONITORS_JSON")
            .collect();
        for key in extra {
            std::env::remove_var(key);
        }
    }

    fn sample_notify(pushover: Option<PushoverConfig>, ntfy: Option<NtfyConfig>) -> NotifyConfig {
        NotifyConfig { pushover, ntfy }
    }

    fn sample_pushover(url: &str) -> PushoverConfig {
        PushoverConfig {
            token: "REPLACE_ME".into(),
            user: "REPLACE_ME".into(),
            url: url.into(),
            title: None,
            priority: None,
            retry: DEFAULT_PUSHOVER_RETRY,
            expire: DEFAULT_PUSHOVER_EXPIRE,
            sound: None,
            device: None,
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
            check_interval_secs: 3600,
            monitors: BTreeMap::from([("poop".to_string(), sample_monitor(90, None))]),
        }
    }

    fn sample_monitor(timeout_secs: u64, secret: Option<&str>) -> MonitorConfig {
        MonitorConfig {
            timeout_secs,
            debounce_secs: 300,
            outage_message: DEFAULT_OUTAGE_MESSAGE.to_string(),
            recovery_message: DEFAULT_RECOVERY_MESSAGE.to_string(),
            secret: secret.map(str::to_string),
        }
    }

    fn test_monitors() -> Arc<Monitors> {
        new_monitors(&sample_watcher())
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
        ($notify:expr, $monitor:expr, $secret:expr) => {
            aw_test::init_service(create_app(
                $notify,
                sample_watcher(),
                $monitor,
                test_client(),
                $secret.into(),
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
        std::env::set_var("HEARTBEAT_SECRET", "  trimmed-secret \t\n");
        assert_eq!(heartbeat_secret_from_env().unwrap(), "trimmed-secret");
        std::env::set_var("HEARTBEAT_SECRET", "");
        assert!(heartbeat_secret_from_env().is_err());
        std::env::set_var("HEARTBEAT_SECRET", " \t\n ");
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
        let watcher = WatcherConfig::from_env();
        assert_eq!(watcher.monitors["poop"].outage_message, "down");
        assert_eq!(watcher.monitors["poop"].recovery_message, "up");
        let pushover = cfg.pushover.expect("pushover enabled");
        assert_eq!(pushover.token, "REPLACE_ME");
        assert_eq!(pushover.user, "REPLACE_ME");
        assert_eq!(pushover.url, "http://127.0.0.1:9/messages.json");
        assert_eq!(pushover.title, None);
        assert_eq!(pushover.priority, None);
        assert_eq!(pushover.retry, DEFAULT_PUSHOVER_RETRY);
        assert_eq!(pushover.expire, DEFAULT_PUSHOVER_EXPIRE);
        assert_eq!(pushover.sound, None);
        assert_eq!(pushover.device, None);
        let ntfy = cfg.ntfy.expect("ntfy enabled");
        assert_eq!(ntfy.topic, "mytopic");
        assert_eq!(ntfy.server, "https://ntfy.example.com");
        assert_eq!(ntfy.token.as_deref(), Some("REPLACE_ME"));
    }

    #[test]
    fn notify_config_from_env_pushover_optional_fields() {
        let _lock = env_lock();
        let _clean = EnvGuard::enter();
        std::env::set_var("PUSHOVER_TOKEN", "REPLACE_ME");
        std::env::set_var("PUSHOVER_USER", "REPLACE_ME");
        std::env::set_var("PUSHOVER_TITLE", "Custom Title");
        std::env::set_var("PUSHOVER_PRIORITY", "1");
        std::env::set_var("PUSHOVER_SOUND", "cosmic");
        std::env::set_var("PUSHOVER_DEVICE", "myphone");

        let cfg = NotifyConfig::from_env().expect("should load");
        let pushover = cfg.pushover.expect("pushover enabled");
        assert_eq!(pushover.title.as_deref(), Some("Custom Title"));
        assert_eq!(pushover.priority, Some(1));
        assert_eq!(pushover.sound.as_deref(), Some("cosmic"));
        assert_eq!(pushover.device.as_deref(), Some("myphone"));

        // Verify negative priority works as well
        std::env::set_var("PUSHOVER_PRIORITY", "-1");
        let cfg_neg = NotifyConfig::from_env().expect("should load negative priority");
        assert_eq!(cfg_neg.pushover.unwrap().priority, Some(-1));

        // Verify blank values are treated as unset
        std::env::set_var("PUSHOVER_TITLE", "");
        std::env::set_var("PUSHOVER_PRIORITY", "");
        std::env::set_var("PUSHOVER_SOUND", "");
        std::env::set_var("PUSHOVER_DEVICE", "");
        let cfg_blank = NotifyConfig::from_env().expect("should load blank optional fields");
        let p_blank = cfg_blank.pushover.unwrap();
        assert_eq!(p_blank.title, None);
        assert_eq!(p_blank.priority, None);
        assert_eq!(p_blank.sound, None);
        assert_eq!(p_blank.device, None);
    }

    #[test]
    fn notify_config_from_env_pushover_rejects_invalid_priority() {
        let _lock = env_lock();
        let _clean = EnvGuard::enter();
        std::env::set_var("PUSHOVER_TOKEN", "REPLACE_ME");
        std::env::set_var("PUSHOVER_USER", "REPLACE_ME");
        std::env::set_var("PUSHOVER_PRIORITY", "not-a-number");

        let err = NotifyConfig::from_env().unwrap_err();
        assert!(err.contains("PUSHOVER_PRIORITY must be a number"));
    }

    #[test]
    fn notify_config_from_env_accepts_priority_range_and_rejects_outside() {
        let _lock = env_lock();
        let _clean = EnvGuard::enter();
        std::env::set_var("PUSHOVER_TOKEN", "REPLACE_ME");
        std::env::set_var("PUSHOVER_USER", "REPLACE_ME");

        for priority in -2..=2 {
            std::env::set_var("PUSHOVER_PRIORITY", priority.to_string());
            assert_eq!(
                NotifyConfig::from_env().unwrap().pushover.unwrap().priority,
                Some(priority)
            );
        }
        for priority in [-3, 3] {
            std::env::set_var("PUSHOVER_PRIORITY", priority.to_string());
            let err = NotifyConfig::from_env().unwrap_err();
            assert!(err.contains("PUSHOVER_PRIORITY must be between -2 and 2"));
        }
    }

    #[test]
    fn notify_config_from_env_clamps_emergency_retry_and_expire() {
        let _lock = env_lock();
        let _clean = EnvGuard::enter();
        std::env::set_var("PUSHOVER_TOKEN", "REPLACE_ME");
        std::env::set_var("PUSHOVER_USER", "REPLACE_ME");
        std::env::set_var("PUSHOVER_RETRY", "10");
        std::env::set_var("PUSHOVER_EXPIRE", "20000");

        let cfg = NotifyConfig::from_env().unwrap();
        let pushover = cfg.pushover.unwrap();
        assert_eq!(pushover.retry, 30);
        assert_eq!(pushover.expire, MAX_PUSHOVER_EXPIRE);

        std::env::set_var("PUSHOVER_RETRY", "45");
        std::env::set_var("PUSHOVER_EXPIRE", "1");
        let cfg = NotifyConfig::from_env().unwrap();
        let pushover = cfg.pushover.unwrap();
        assert_eq!(pushover.retry, 45);
        assert_eq!(pushover.expire, 45);

        std::env::set_var("PUSHOVER_RETRY", "20000");
        std::env::set_var("PUSHOVER_EXPIRE", "20000");
        let cfg = NotifyConfig::from_env().unwrap();
        let pushover = cfg.pushover.unwrap();
        assert_eq!(pushover.retry, MAX_PUSHOVER_EXPIRE);
        assert_eq!(pushover.expire, MAX_PUSHOVER_EXPIRE);
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
        let watcher = WatcherConfig::from_env();
        assert_eq!(
            watcher.monitors["poop"].outage_message,
            DEFAULT_OUTAGE_MESSAGE
        );
        assert_eq!(
            watcher.monitors["poop"].recovery_message,
            DEFAULT_RECOVERY_MESSAGE
        );
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
        assert_eq!(defaults.monitors.len(), 1);
        assert_eq!(defaults.monitors["poop"].timeout_secs, 90);
        assert_eq!(defaults.check_interval_secs, 10);
        assert_eq!(defaults.monitors["poop"].debounce_secs, 300);

        std::env::set_var("HEARTBEAT_TIMEOUT_SECS", "15");
        std::env::set_var("CHECK_INTERVAL_SECS", "2");
        std::env::set_var("DEBOUNCE_SECS", "40");
        let custom = WatcherConfig::from_env();
        assert_eq!(custom.monitors["poop"].timeout_secs, 15);
        assert_eq!(custom.check_interval_secs, 2);
        assert_eq!(custom.monitors["poop"].debounce_secs, 40);
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
    async fn send_pushover_posts_all_optional_fields_when_present() {
        let mock = spawn_mock().await;
        let mut cfg = sample_pushover(&format!("http://127.0.0.1:{}/pushover", mock.port));
        cfg.title = Some("Poop Alert".into());
        cfg.priority = Some(1);
        cfg.sound = Some("siren".into());
        cfg.device = Some("pixel-8".into());

        let client = test_client();
        let ok = send_pushover(&client, &cfg, "warning message").await;
        assert!(ok);

        let hits = hits_of(&mock);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].path, "/pushover");
        assert!(hits[0].body.contains("token=REPLACE_ME"));
        assert!(hits[0].body.contains("user=REPLACE_ME"));
        let decoded = decoded_body(&hits[0].body);
        assert!(decoded.contains("message=warning message"));
        assert!(decoded.contains("title=Poop Alert"));
        assert!(hits[0].body.contains("priority=1"));
        assert!(hits[0].body.contains("sound=siren"));
        assert!(hits[0].body.contains("device=pixel-8"));
    }

    #[actix_web::test]
    async fn send_pushover_omits_optional_fields_when_none() {
        let mock = spawn_mock().await;
        let cfg = sample_pushover(&format!("http://127.0.0.1:{}/pushover", mock.port));

        let client = test_client();
        let ok = send_pushover(&client, &cfg, "plain message").await;
        assert!(ok);

        let hits = hits_of(&mock);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].path, "/pushover");
        assert!(hits[0].body.contains("token=REPLACE_ME"));
        assert!(hits[0].body.contains("user=REPLACE_ME"));
        let decoded = decoded_body(&hits[0].body);
        assert!(decoded.contains("message=plain message"));
        assert!(!hits[0].body.contains("title="));
        assert!(!hits[0].body.contains("priority="));
        assert!(!hits[0].body.contains("sound="));
        assert!(!hits[0].body.contains("device="));
    }

    #[actix_web::test]
    async fn send_pushover_posts_subset_of_optional_fields() {
        let mock = spawn_mock().await;
        let mut cfg = sample_pushover(&format!("http://127.0.0.1:{}/pushover", mock.port));
        cfg.priority = Some(-1);
        cfg.sound = Some("quiet".into());

        let client = test_client();
        let ok = send_pushover(&client, &cfg, "quiet message").await;
        assert!(ok);

        let hits = hits_of(&mock);
        assert_eq!(hits.len(), 1);
        assert!(hits[0].body.contains("priority=-1"));
        assert!(hits[0].body.contains("sound=quiet"));
        assert!(!hits[0].body.contains("title="));
        assert!(!hits[0].body.contains("device="));
        assert!(!hits[0].body.contains("retry="));
        assert!(!hits[0].body.contains("expire="));
    }

    #[actix_web::test]
    async fn send_pushover_emergency_includes_default_retry_and_expire() {
        let mock = spawn_mock().await;
        let mut cfg = sample_pushover(&format!("http://127.0.0.1:{}/pushover", mock.port));
        cfg.priority = Some(2);

        assert!(send_pushover(&test_client(), &cfg, "urgent message").await);
        let hits = hits_of(&mock);
        assert_eq!(hits.len(), 1);
        assert!(hits[0].body.contains("priority=2"));
        assert!(hits[0].body.contains("retry=60"));
        assert!(hits[0].body.contains("expire=3600"));
    }

    #[actix_web::test]
    async fn send_pushover_emergency_includes_custom_retry_and_expire() {
        let mock = spawn_mock().await;
        let mut cfg = sample_pushover(&format!("http://127.0.0.1:{}/pushover", mock.port));
        cfg.priority = Some(2);
        cfg.retry = 30;
        cfg.expire = MAX_PUSHOVER_EXPIRE;

        assert!(send_pushover(&test_client(), &cfg, "urgent message").await);
        let hits = hits_of(&mock);
        assert_eq!(hits.len(), 1);
        assert!(hits[0].body.contains("retry=30"));
        assert!(hits[0].body.contains("expire=10800"));
    }

    #[actix_web::test]
    async fn send_alert_pushover_with_optional_fields() {
        let mock = spawn_mock().await;
        let mut pushover = sample_pushover(&format!("http://127.0.0.1:{}/pushover", mock.port));
        pushover.title = Some("Outage".into());
        pushover.priority = Some(1);
        pushover.sound = Some("falling".into());
        pushover.device = Some("nexus".into());
        let notify = sample_notify(Some(pushover), None);

        let ok = send_alert(&test_client(), &notify, "synthetic-alert").await;
        assert!(ok);
        let hits = hits_of(&mock);
        assert_eq!(hits.len(), 1);
        assert!(hits[0].body.contains("title=Outage"));
        assert!(hits[0].body.contains("priority=1"));
        assert!(hits[0].body.contains("sound=falling"));
        assert!(hits[0].body.contains("device=nexus"));
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
        let monitor = test_monitors();
        let age = Duration::from_secs(1000);
        {
            let mut states = monitor.lock().unwrap();
            let mon = states.get_mut("poop").unwrap();
            age_heartbeat(mon, age);
        }
        let seen_before = monitor.lock().unwrap()["poop"].last_seen;

        poll_staleness(&notify, &sample_watcher(), &monitor, &test_client()).await;

        let hits = hits_of(&mock);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].path, "/solo");
        let down_for = downtime_secs(&hits[0].body);
        assert!(
            (990..=1010).contains(&down_for),
            "measured downtime {down_for} must follow the last heartbeat, not the alert"
        );
        let states = monitor.lock().unwrap();
        let mon = states.get("poop").unwrap();
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
        let monitor = test_monitors();
        {
            let mut states = monitor.lock().unwrap();
            let mon = states.get_mut("poop").unwrap();
            age_heartbeat(mon, Duration::from_secs(1000));
        }

        let watcher = sample_watcher();
        let client = test_client();
        poll_staleness(&notify, &watcher, &monitor, &client).await;
        poll_staleness(&notify, &watcher, &monitor, &client).await;

        let hits = hits_of(&mock);
        assert_eq!(hits.len(), 4, "failed delivery retries on the next check");
        let states = monitor.lock().unwrap();
        let mon = states.get("poop").unwrap();
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
        let monitor = test_monitors();
        {
            let mut states = monitor.lock().unwrap();
            let mon = states.get_mut("poop").unwrap();
            age_heartbeat(mon, Duration::from_secs(1000));
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
            let mut states = monitor.lock().unwrap();
            let mon = states.get_mut("poop").unwrap();
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
            let monitor = test_monitors();
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
        let monitor = test_monitors();
        {
            let mut states = monitor.lock().unwrap();
            let mon = states.get_mut("poop").unwrap();
            age_heartbeat(mon, Duration::from_secs(50));
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
        assert_eq!(monitor.lock().unwrap()["poop"].phase, Phase::Healthy);
    }

    #[actix_web::test]
    async fn unauthorized_heartbeat_does_not_refresh_liveness_or_send_recovery() {
        let mock = spawn_mock().await;
        let server = format!("http://127.0.0.1:{}", mock.port);
        let notify = sample_notify(
            Some(sample_pushover(&format!("{server}/pushover"))),
            Some(sample_ntfy(&server, "alerts")),
        );
        let monitor = test_monitors();
        {
            let mut states = monitor.lock().unwrap();
            let mon = states.get_mut("poop").unwrap();
            age_heartbeat(mon, Duration::from_secs(200));
            mon.phase = Phase::Outage;
            mon.last_alert = Some(mon.epoch + Duration::from_secs(100));
        }
        let last_seen = monitor.lock().unwrap()["poop"].last_seen;
        let app = shipped_app!(notify, Arc::clone(&monitor));

        for authorization in [None, Some("Bearer xxxxxxxxxx"), Some("Bearer short")] {
            let mut request = aw_test::TestRequest::get().uri("/heartbeat/poop");
            if let Some(value) = authorization {
                request = request.insert_header((actix_web::http::header::AUTHORIZATION, value));
            }
            let response = aw_test::call_service(&app, request.to_request()).await;
            assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        }

        let states = monitor.lock().unwrap();
        let mon = states.get("poop").unwrap();
        assert_eq!(mon.last_seen, last_seen);
        assert_eq!(mon.phase, Phase::Outage);
        assert!(hits_of(&mock).is_empty());
    }

    #[actix_web::test]
    async fn heartbeat_accepts_correct_secret() {
        let notify = sample_notify(None, None);
        let monitor = test_monitors();
        let app = shipped_app!(notify, monitor, "trimmed-secret");

        let response = aw_test::call_service(
            &app,
            aw_test::TestRequest::get()
                .uri("/heartbeat/poop")
                .insert_header((
                    actix_web::http::header::AUTHORIZATION,
                    "Bearer trimmed-secret",
                ))
                .to_request(),
        )
        .await;

        assert_eq!(response.status(), StatusCode::OK);
    }

    #[actix_web::test]
    async fn heartbeat_after_outage_sends_recovery_to_configured_backends() {
        let mock = spawn_mock().await;
        let server = format!("http://127.0.0.1:{}", mock.port);
        let notify = sample_notify(
            Some(sample_pushover(&format!("{server}/pushover"))),
            Some(sample_ntfy(&server, "alerts")),
        );
        let monitor = test_monitors();
        {
            let mut states = monitor.lock().unwrap();
            let mon = states.get_mut("poop").unwrap();
            age_heartbeat(mon, Duration::from_secs(200));
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
            let states = monitor.lock().unwrap();
            let mon = states.get("poop").unwrap();
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

    // --- multiple named monitors ---

    fn two_monitor_watcher() -> WatcherConfig {
        let mut watcher = sample_watcher();
        let mut backup = sample_monitor(3600, Some("backup-secret"));
        backup.outage_message = "BACKUP_DOWN".into();
        backup.recovery_message = "BACKUP_UP".into();
        watcher.monitors.insert("backup".into(), backup);
        watcher
    }

    macro_rules! two_monitor_app {
        ($notify:expr, $monitors:expr) => {
            aw_test::init_service(create_app(
                $notify,
                two_monitor_watcher(),
                $monitors,
                test_client(),
                "REPLACE_ME".into(),
            ))
            .await
        };
    }

    fn bearer(secret: &str) -> (actix_web::http::header::HeaderName, String) {
        (
            actix_web::http::header::AUTHORIZATION,
            format!("Bearer {secret}"),
        )
    }

    #[actix_web::test]
    async fn unknown_monitor_is_401_unauthenticated_and_404_when_authenticated() {
        let monitors = new_monitors(&two_monitor_watcher());
        let app = two_monitor_app!(sample_notify(None, None), Arc::clone(&monitors));
        for uri in [
            "/heartbeat/nope",
            "/heartbeat/nope/fail",
            "/heartbeat/a%20b",
        ] {
            // Authenticated with the default secret: 404, no provisioning.
            let req = aw_test::TestRequest::post()
                .uri(uri)
                .insert_header(bearer("REPLACE_ME"))
                .to_request();
            let resp = aw_test::call_service(&app, req).await;
            assert_eq!(resp.status(), StatusCode::NOT_FOUND, "{uri}");
            // Unauthenticated or wrong secret: 401, same as a configured name.
            for header in [None, Some("wrong"), Some("backup-secret")] {
                let mut req = aw_test::TestRequest::post().uri(uri);
                if let Some(secret) = header {
                    req = req.insert_header(bearer(secret));
                }
                let resp = aw_test::call_service(&app, req.to_request()).await;
                assert_eq!(resp.status(), StatusCode::UNAUTHORIZED, "{uri}");
            }
        }
        assert_eq!(monitors.lock().unwrap().len(), 2);
    }

    #[actix_web::test]
    async fn per_monitor_secret_is_enforced_and_default_applies_otherwise() {
        let monitors = new_monitors(&two_monitor_watcher());
        let app = two_monitor_app!(sample_notify(None, None), monitors);
        let status = |uri: &'static str, secret: Option<&'static str>| {
            let mut req = aw_test::TestRequest::get().uri(uri);
            if let Some(secret) = secret {
                req = req.insert_header(bearer(secret));
            }
            req.to_request()
        };
        let backup_ok =
            aw_test::call_service(&app, status("/heartbeat/backup", Some("backup-secret"))).await;
        assert_eq!(backup_ok.status(), StatusCode::OK);
        // The global secret does not open a monitor with its own secret ...
        let backup_global =
            aw_test::call_service(&app, status("/heartbeat/backup", Some("REPLACE_ME"))).await;
        assert_eq!(backup_global.status(), StatusCode::UNAUTHORIZED);
        let fail_global = aw_test::call_service(
            &app,
            aw_test::TestRequest::post()
                .uri("/heartbeat/backup/fail")
                .insert_header(bearer("REPLACE_ME"))
                .to_request(),
        )
        .await;
        assert_eq!(fail_global.status(), StatusCode::UNAUTHORIZED);
        // ... and the monitor secret does not open poop.
        let poop_backup =
            aw_test::call_service(&app, status("/heartbeat/poop", Some("backup-secret"))).await;
        assert_eq!(poop_backup.status(), StatusCode::UNAUTHORIZED);
        let poop_ok =
            aw_test::call_service(&app, status("/heartbeat/poop", Some("REPLACE_ME"))).await;
        assert_eq!(poop_ok.status(), StatusCode::OK);
        let none = aw_test::call_service(&app, status("/heartbeat/backup", None)).await;
        assert_eq!(none.status(), StatusCode::UNAUTHORIZED);
    }

    #[actix_web::test]
    async fn monitors_are_independent_and_post_is_accepted() {
        let monitors = new_monitors(&two_monitor_watcher());
        {
            let mut states = monitors.lock().unwrap();
            age_heartbeat(states.get_mut("poop").unwrap(), Duration::from_secs(50));
            age_heartbeat(states.get_mut("backup").unwrap(), Duration::from_secs(50));
        }
        let app = two_monitor_app!(sample_notify(None, None), Arc::clone(&monitors));

        let resp = aw_test::call_service(
            &app,
            aw_test::TestRequest::post()
                .uri("/heartbeat/backup")
                .insert_header(bearer("backup-secret"))
                .to_request(),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::OK);

        let health = aw_test::call_service(
            &app,
            aw_test::TestRequest::get().uri("/healthz").to_request(),
        )
        .await;
        let body: HealthResponse = aw_test::read_body_json(health).await;
        assert_eq!(body.monitors_total, 2);
        assert_eq!(body.monitors_down, 0);
        assert!(body.last_heartbeat_secs_ago >= 49, "poop's age is reported");
    }

    #[actix_web::test]
    async fn healthz_counts_stale_and_failed_monitors_down() {
        let monitors = new_monitors(&two_monitor_watcher());
        {
            let mut states = monitors.lock().unwrap();
            // poop timeout is 90s, backup 3600s.
            age_heartbeat(states.get_mut("poop").unwrap(), Duration::from_secs(200));
            age_heartbeat(states.get_mut("backup").unwrap(), Duration::from_secs(200));
        }
        let app = two_monitor_app!(sample_notify(None, None), Arc::clone(&monitors));
        let read = |app| async move {
            let resp = aw_test::call_service(
                app,
                aw_test::TestRequest::get().uri("/healthz").to_request(),
            )
            .await;
            aw_test::read_body_json::<HealthResponse, _>(resp).await
        };
        let body = read(&app).await;
        assert_eq!((body.monitors_total, body.monitors_down), (2, 1));

        monitors.lock().unwrap().get_mut("backup").unwrap().phase = Phase::Outage;
        let body = read(&app).await;
        assert_eq!(body.monitors_down, 2);
    }

    #[actix_web::test]
    async fn fail_signal_alerts_immediately_with_body_or_configured_message() {
        let mock = spawn_mock().await;
        let notify = sample_notify(
            None,
            Some(sample_ntfy(
                &format!("http://127.0.0.1:{}", mock.port),
                "alerts",
            )),
        );
        let monitors = new_monitors(&two_monitor_watcher());
        let app = two_monitor_app!(notify, Arc::clone(&monitors));

        // No body: the monitor's configured outage message, no downtime suffix.
        let resp = aw_test::call_service(
            &app,
            aw_test::TestRequest::post()
                .uri("/heartbeat/backup/fail")
                .insert_header(bearer("backup-secret"))
                .to_request(),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::OK);
        // Let the debounce window lapse (backup: 300s), then send a body.
        {
            let mut states = monitors.lock().unwrap();
            let mon = states.get_mut("backup").unwrap();
            mon.last_alert = Some(Instant::now() - Duration::from_secs(301));
        }
        let resp = aw_test::call_service(
            &app,
            aw_test::TestRequest::post()
                .uri("/heartbeat/backup/fail")
                .insert_header(bearer("backup-secret"))
                .set_payload("  pg_dump exited 2  ")
                .to_request(),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::OK);

        let hits = hits_of(&mock);
        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0].body, "BACKUP_DOWN");
        assert_eq!(hits[1].body, "pg_dump exited 2");
        {
            let states = monitors.lock().unwrap();
            assert_eq!(states["backup"].phase, Phase::Outage);
            assert!(states["backup"].last_alert.is_some());
            assert_eq!(
                states["poop"].phase,
                Phase::Healthy,
                "other monitors untouched"
            );
        }

        // The next success ping recovers with that monitor's recovery message.
        let resp = aw_test::call_service(
            &app,
            aw_test::TestRequest::get()
                .uri("/heartbeat/backup")
                .insert_header(bearer("backup-secret"))
                .to_request(),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::OK);
        let hits = hits_of(&mock);
        assert_eq!(hits.len(), 3);
        assert_eq!(hits[2].body, "BACKUP_UP");
        assert_eq!(monitors.lock().unwrap()["backup"].phase, Phase::Healthy);
    }

    #[actix_web::test]
    async fn fail_signal_is_debounced_per_monitor_and_cleared_by_recovery() {
        let mock = spawn_mock().await;
        let notify = sample_notify(
            None,
            Some(sample_ntfy(
                &format!("http://127.0.0.1:{}", mock.port),
                "alerts",
            )),
        );
        let monitors = new_monitors(&two_monitor_watcher());
        let app = two_monitor_app!(notify, Arc::clone(&monitors));
        let fail = |uri: &'static str, secret: &'static str| {
            aw_test::TestRequest::post()
                .uri(uri)
                .insert_header(bearer(secret))
                .set_payload("boom")
                .to_request()
        };

        let first =
            aw_test::call_service(&app, fail("/heartbeat/backup/fail", "backup-secret")).await;
        assert_eq!(first.status(), StatusCode::OK);
        for _ in 0..3 {
            let again =
                aw_test::call_service(&app, fail("/heartbeat/backup/fail", "backup-secret")).await;
            assert_eq!(again.status(), StatusCode::ACCEPTED);
            assert_eq!(aw_test::read_body(again).await, "suppressed");
        }
        assert_eq!(hits_of(&mock).len(), 1, "repeats are not sent");

        // Another monitor has its own window.
        let other = aw_test::call_service(&app, fail("/heartbeat/poop/fail", "REPLACE_ME")).await;
        assert_eq!(other.status(), StatusCode::OK);
        assert_eq!(hits_of(&mock).len(), 2);

        // Recovery clears the window: backup may alert again (plus the recovery message).
        let up = aw_test::call_service(
            &app,
            aw_test::TestRequest::post()
                .uri("/heartbeat/backup")
                .insert_header(bearer("backup-secret"))
                .to_request(),
        )
        .await;
        assert_eq!(up.status(), StatusCode::OK);
        let again =
            aw_test::call_service(&app, fail("/heartbeat/backup/fail", "backup-secret")).await;
        assert_eq!(again.status(), StatusCode::OK);
        assert_eq!(hits_of(&mock).len(), 4);
    }

    #[actix_web::test]
    async fn fail_signal_rejects_get() {
        let mock = spawn_mock().await;
        let notify = sample_notify(
            None,
            Some(sample_ntfy(
                &format!("http://127.0.0.1:{}", mock.port),
                "alerts",
            )),
        );
        let monitors = new_monitors(&two_monitor_watcher());
        let app = two_monitor_app!(notify, Arc::clone(&monitors));
        let resp = aw_test::call_service(
            &app,
            aw_test::TestRequest::get()
                .uri("/heartbeat/backup/fail")
                .insert_header(bearer("backup-secret"))
                .to_request(),
        )
        .await;
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);
        assert!(hits_of(&mock).is_empty());
        assert!(monitors.lock().unwrap()["backup"].last_alert.is_none());
    }

    #[actix_web::test]
    async fn healthz_does_not_expose_monitor_names() {
        let monitors = new_monitors(&two_monitor_watcher());
        let app = two_monitor_app!(sample_notify(None, None), monitors);
        let resp = aw_test::call_service(
            &app,
            aw_test::TestRequest::get().uri("/healthz").to_request(),
        )
        .await;
        let raw = String::from_utf8(aw_test::read_body(resp).await.to_vec()).unwrap();
        assert!(!raw.contains("backup"), "{raw}");
        assert!(!raw.contains("poop"), "{raw}");
    }

    #[actix_web::test]
    async fn fail_signal_requires_secret_and_reports_delivery_failure() {
        let mock = spawn_mock().await;
        let server = format!("http://127.0.0.1:{}", mock.port);
        let monitors = new_monitors(&two_monitor_watcher());
        let app = two_monitor_app!(
            sample_notify(None, Some(sample_ntfy(&server, "fail"))),
            Arc::clone(&monitors)
        );

        let unauth = aw_test::call_service(
            &app,
            aw_test::TestRequest::post()
                .uri("/heartbeat/poop/fail")
                .to_request(),
        )
        .await;
        assert_eq!(unauth.status(), StatusCode::UNAUTHORIZED);
        assert!(hits_of(&mock).is_empty());

        // Backend rejects ("fail" in the topic path): 502, phase unchanged.
        let failed = aw_test::call_service(
            &app,
            aw_test::TestRequest::post()
                .uri("/heartbeat/poop/fail")
                .insert_header(bearer("REPLACE_ME"))
                .to_request(),
        )
        .await;
        assert_eq!(failed.status(), StatusCode::BAD_GATEWAY);
        let states = monitors.lock().unwrap();
        assert_eq!(states["poop"].phase, Phase::Healthy);
        assert!(states["poop"].last_alert.is_none());
    }

    #[actix_web::test]
    async fn poll_uses_each_monitors_timeout_and_message() {
        let mock = spawn_mock().await;
        let notify = sample_notify(
            None,
            Some(sample_ntfy(
                &format!("http://127.0.0.1:{}", mock.port),
                "alerts",
            )),
        );
        let watcher = two_monitor_watcher();
        let monitors = new_monitors(&watcher);
        {
            let mut states = monitors.lock().unwrap();
            // 200s old: past poop's 90s timeout, within backup's 3600s.
            age_heartbeat(states.get_mut("poop").unwrap(), Duration::from_secs(200));
            age_heartbeat(states.get_mut("backup").unwrap(), Duration::from_secs(200));
        }
        poll_staleness(&notify, &watcher, &monitors, &test_client()).await;
        let hits = hits_of(&mock);
        assert_eq!(hits.len(), 1);
        assert!(decoded_body(&hits[0].body).contains("Poop Monitor is offline"));
        {
            let states = monitors.lock().unwrap();
            assert_eq!(states["poop"].phase, Phase::Outage);
            assert_eq!(states["backup"].phase, Phase::Healthy);
        }

        {
            let mut states = monitors.lock().unwrap();
            age_heartbeat(states.get_mut("backup").unwrap(), Duration::from_secs(4000));
        }
        poll_staleness(&notify, &watcher, &monitors, &test_client()).await;
        let hits = hits_of(&mock);
        assert_eq!(hits.len(), 2, "poop is debounced; backup alerts");
        assert!(hits[1].body.starts_with("BACKUP_DOWN"));
    }
}
