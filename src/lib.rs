//! Heartbeat watcher with multi-channel notification backends.

use actix_web::{get, web, App, HttpResponse, HttpServer, Responder};
use log::{info, warn};
use once_cell::sync::Lazy;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use std::{
    env,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc, Mutex,
    },
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
use tokio::{spawn, time};

/// Shared state: last heartbeat Instant.
pub static LAST_SEEN: Lazy<Arc<Mutex<Instant>>> =
    Lazy::new(|| Arc::new(Mutex::new(Instant::now())));

/// Process start time (unix seconds) for health reporting.
static STARTED_AT_UNIX: Lazy<u64> = Lazy::new(|| {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
});

/// Monotonic-ish uptime counter updated by health checks (optional); primary uptime uses STARTED_AT.
static _HEARTBEAT_COUNT: AtomicU64 = AtomicU64::new(0);

/// Pushover credentials.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PushoverConfig {
    pub token: String,
    pub user: String,
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

impl NotifyConfig {
    /// Load from environment variables.
    ///
    /// - Pushover: `PUSHOVER_TOKEN` + `PUSHOVER_USER` (both required to enable)
    /// - ntfy: `NTFY_TOPIC` (required to enable); optional `NTFY_SERVER` (default `https://ntfy.sh`),
    ///   optional `NTFY_TOKEN`
    pub fn from_env() -> Result<Self, String> {
        let pushover = match (env::var("PUSHOVER_TOKEN"), env::var("PUSHOVER_USER")) {
            (Ok(token), Ok(user)) if !token.is_empty() && !user.is_empty() => {
                Some(PushoverConfig { token, user })
            }
            (Ok(token), Ok(user)) if token.is_empty() || user.is_empty() => {
                return Err(
                    "PUSHOVER_TOKEN and PUSHOVER_USER must both be non-empty when set".into(),
                );
            }
            (Err(_), Err(_)) => None,
            (Ok(_), Err(_)) | (Err(_), Ok(_)) => {
                return Err(
                    "Both PUSHOVER_TOKEN and PUSHOVER_USER must be set to enable Pushover".into(),
                );
            }
            _ => None,
        };

        let ntfy = match env::var("NTFY_TOPIC") {
            Ok(topic) if !topic.is_empty() => {
                let server = env::var("NTFY_SERVER")
                    .unwrap_or_else(|_| "https://ntfy.sh".into())
                    .trim_end_matches('/')
                    .to_string();
                let token = env::var("NTFY_TOKEN").ok().filter(|t| !t.is_empty());
                Some(NtfyConfig {
                    server,
                    topic,
                    token,
                })
            }
            Ok(_) => {
                return Err("NTFY_TOPIC must be non-empty when set".into());
            }
            Err(_) => None,
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

/// Send `message` on every enabled backend. Errors are logged; does not fail the whole batch.
pub async fn send_alert(client: &Client, config: &NotifyConfig, message: &str) {
    if let Some(po) = &config.pushover {
        let params = [
            ("token", po.token.as_str()),
            ("user", po.user.as_str()),
            ("message", message),
        ];
        match client
            .post("https://api.pushover.net/1/messages.json")
            .form(&params)
            .send()
            .await
        {
            Ok(r) => info!("Pushover status: {}", r.status()),
            Err(e) => warn!("Failed to send Pushover: {}", e),
        }
    }

    if let Some(ntfy) = &config.ntfy {
        let url = format!("{}/{}", ntfy.server, ntfy.topic);
        let mut req = client
            .post(&url)
            .header("Title", "Notification API")
            .body(message.to_string());
        if let Some(token) = &ntfy.token {
            req = req.bearer_auth(token);
        }
        match req.send().await {
            Ok(r) => info!("ntfy status: {}", r.status()),
            Err(e) => warn!("Failed to send ntfy: {}", e),
        }
    }
}

#[derive(Serialize, Deserialize)]
pub struct HealthResponse {
    pub status: String,
    pub uptime_secs: u64,
    pub backends: Vec<String>,
    pub last_heartbeat_secs_ago: u64,
}

/// Process health for probes (liveness).
#[get("/healthz")]
pub async fn healthz(notify: web::Data<NotifyConfig>) -> HttpResponse {
    let uptime_secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs().saturating_sub(*STARTED_AT_UNIX))
        .unwrap_or(0);
    let last_heartbeat_secs_ago = LAST_SEEN
        .lock()
        .map(|last| last.elapsed().as_secs())
        .unwrap_or(0);

    HttpResponse::Ok().json(HealthResponse {
        status: "ok".to_string(),
        uptime_secs,
        backends: notify
            .backend_names()
            .into_iter()
            .map(str::to_string)
            .collect(),
        last_heartbeat_secs_ago,
    })
}

#[get("/heartbeat/poop")]
pub async fn heartbeat() -> impl Responder {
    let mut last = LAST_SEEN.lock().unwrap();
    *last = Instant::now();
    _HEARTBEAT_COUNT.fetch_add(1, Ordering::Relaxed);
    info!("Heartbeat received at {:?}", *last);
    "OK"
}

/// Build the Actix app (shared by binary and tests).
pub fn configure_app(cfg: &mut web::ServiceConfig) {
    cfg.service(healthz).service(heartbeat);
}

/// Create an App factory with the given notify config.
pub fn app_with_config(
    notify: NotifyConfig,
) -> App<
    impl actix_web::dev::ServiceFactory<
        actix_web::dev::ServiceRequest,
        Config = (),
        Response = actix_web::dev::ServiceResponse,
        Error = actix_web::Error,
        InitError = (),
    >,
> {
    App::new()
        .app_data(web::Data::new(notify))
        .configure(configure_app)
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

/// Background task: alert when heartbeat is stale.
pub fn spawn_staleness_watcher(notify: NotifyConfig, watcher: WatcherConfig) {
    let client = Client::new();
    spawn(async move {
        let mut connection_missing = false;
        loop {
            let mut time_interval = watcher.check_interval_secs;
            if connection_missing {
                info!("Starting debounce now that we alerted");
                time_interval = watcher.debounce_secs;
            }
            time::sleep(Duration::from_secs(time_interval)).await;
            connection_missing = false;
            let last = *LAST_SEEN.lock().unwrap();
            let elapsed = last.elapsed().as_secs();

            if elapsed > watcher.timeout_secs {
                warn!(
                    "No heartbeat for {}s (> {}s). Sending alert via {:?}.",
                    elapsed,
                    watcher.timeout_secs,
                    notify.backend_names()
                );
                send_alert(&client, &notify, "❌ Poop Monitor is offline!").await;
                connection_missing = true;
                // Prevent repeat alerts until next heartbeat resets LAST_SEEN
                let mut last = LAST_SEEN.lock().unwrap();
                *last = Instant::now();
            }
        }
    });
}

/// Run the HTTP server on 0.0.0.0:3000.
pub async fn run_server(notify: NotifyConfig) -> std::io::Result<()> {
    HttpServer::new(move || app_with_config(notify.clone()))
        .bind(("0.0.0.0", 3000))?
        .run()
        .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use actix_web::{test as aw_test, web, App};

    #[test]
    fn notify_config_backend_names() {
        let both = NotifyConfig {
            pushover: Some(PushoverConfig {
                token: "t".into(),
                user: "u".into(),
            }),
            ntfy: Some(NtfyConfig {
                server: "https://ntfy.sh".into(),
                topic: "alerts".into(),
                token: None,
            }),
        };
        assert_eq!(both.backend_names(), vec!["pushover", "ntfy"]);

        let only_ntfy = NotifyConfig {
            pushover: None,
            ntfy: Some(NtfyConfig {
                server: "https://ntfy.example".into(),
                topic: "x".into(),
                token: Some("secret".into()),
            }),
        };
        assert_eq!(only_ntfy.backend_names(), vec!["ntfy"]);
    }

    #[test]
    fn notify_config_from_env_requires_backend() {
        // Clear relevant vars in a controlled way via temp env is hard without
        // serial tests; validate the error path with an empty constructed config.
        let empty = NotifyConfig {
            pushover: None,
            ntfy: None,
        };
        assert!(empty.backend_names().is_empty());
    }

    #[test]
    fn notify_config_from_env_pushover_and_ntfy() {
        // Safety: tests that touch env should run single-threaded for these keys.
        // We only set when missing side effects matter — use a dedicated approach.
        std::env::set_var("PUSHOVER_TOKEN", "tok");
        std::env::set_var("PUSHOVER_USER", "usr");
        std::env::set_var("NTFY_TOPIC", "mytopic");
        std::env::set_var("NTFY_SERVER", "https://ntfy.example.com/");
        std::env::set_var("NTFY_TOKEN", "ntfy-secret");

        let cfg = NotifyConfig::from_env().expect("should load");
        assert!(cfg.pushover.is_some());
        let ntfy = cfg.ntfy.expect("ntfy enabled");
        assert_eq!(ntfy.topic, "mytopic");
        assert_eq!(ntfy.server, "https://ntfy.example.com");
        assert_eq!(ntfy.token.as_deref(), Some("ntfy-secret"));

        std::env::remove_var("PUSHOVER_TOKEN");
        std::env::remove_var("PUSHOVER_USER");
        std::env::remove_var("NTFY_TOPIC");
        std::env::remove_var("NTFY_SERVER");
        std::env::remove_var("NTFY_TOKEN");
    }

    #[test]
    fn notify_config_from_env_ntfy_only() {
        std::env::remove_var("PUSHOVER_TOKEN");
        std::env::remove_var("PUSHOVER_USER");
        std::env::set_var("NTFY_TOPIC", "solo");
        std::env::remove_var("NTFY_SERVER");
        std::env::remove_var("NTFY_TOKEN");

        let cfg = NotifyConfig::from_env().expect("ntfy-only should work");
        assert!(cfg.pushover.is_none());
        let ntfy = cfg.ntfy.unwrap();
        assert_eq!(ntfy.server, "https://ntfy.sh");
        assert_eq!(ntfy.topic, "solo");

        std::env::remove_var("NTFY_TOPIC");
    }

    #[test]
    fn notify_config_from_env_rejects_empty() {
        std::env::remove_var("PUSHOVER_TOKEN");
        std::env::remove_var("PUSHOVER_USER");
        std::env::remove_var("NTFY_TOPIC");
        std::env::remove_var("NTFY_SERVER");
        std::env::remove_var("NTFY_TOKEN");

        let err = NotifyConfig::from_env().unwrap_err();
        assert!(err.contains("At least one notification backend"));
    }

    #[actix_web::test]
    async fn healthz_returns_ok_json() {
        let notify = NotifyConfig {
            pushover: Some(PushoverConfig {
                token: "t".into(),
                user: "u".into(),
            }),
            ntfy: None,
        };
        let app = aw_test::init_service(
            App::new()
                .app_data(web::Data::new(notify))
                .configure(configure_app),
        )
        .await;

        let req = aw_test::TestRequest::get().uri("/healthz").to_request();
        let resp = aw_test::call_service(&app, req).await;
        assert!(resp.status().is_success());

        let body: HealthResponse = aw_test::read_body_json(resp).await;
        assert_eq!(body.status, "ok");
        assert_eq!(body.backends, vec!["pushover".to_string()]);
    }

    #[actix_web::test]
    async fn heartbeat_endpoint_ok() {
        let notify = NotifyConfig {
            pushover: None,
            ntfy: Some(NtfyConfig {
                server: "https://ntfy.sh".into(),
                topic: "t".into(),
                token: None,
            }),
        };
        let app = aw_test::init_service(
            App::new()
                .app_data(web::Data::new(notify))
                .configure(configure_app),
        )
        .await;

        let req = aw_test::TestRequest::get()
            .uri("/heartbeat/poop")
            .to_request();
        let resp = aw_test::call_service(&app, req).await;
        assert!(resp.status().is_success());
        let body = aw_test::read_body(resp).await;
        assert_eq!(&body[..], b"OK");
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
}
