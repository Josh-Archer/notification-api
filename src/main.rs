use actix_web::{
    get,
    http::header::{self, HeaderMap},
    web, App, HttpRequest, HttpResponse, HttpServer, Responder,
};
use dotenv::dotenv;
use futures::FutureExt;
use log::{info, warn};
use once_cell::sync::Lazy;
use reqwest::{Client, Error as ReqwestError, Response};
use std::{
    env,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};
use tokio::{spawn, time};

/// Shared secret for authenticating heartbeat requests.
/// When `Some`, requests without a matching token are rejected (fail closed).
/// When `None`, authentication is disabled (rely on network policy / mTLS).
#[derive(Clone, Debug)]
struct AuthConfig {
    token: Option<String>,
}

// Shared state to hold the last-seen Instant
static LAST_SEEN: Lazy<Arc<Mutex<Instant>>> = Lazy::new(|| {
    // Initialize to now so we don't immediately trigger alert on startup
    Arc::new(Mutex::new(Instant::now()))
});

/// Extract a presented shared secret from either:
/// - `Authorization: Bearer <token>`
/// - `X-Heartbeat-Token: <token>`
fn extract_presented_token(headers: &HeaderMap) -> Option<String> {
    if let Some(value) = headers.get("X-Heartbeat-Token") {
        if let Ok(s) = value.to_str() {
            let trimmed = s.trim();
            if !trimmed.is_empty() {
                return Some(trimmed.to_string());
            }
        }
    }

    if let Some(value) = headers.get(header::AUTHORIZATION) {
        if let Ok(s) = value.to_str() {
            let trimmed = s.trim();
            if let Some(token) = trimmed
                .strip_prefix("Bearer ")
                .or_else(|| trimmed.strip_prefix("bearer "))
            {
                let token = token.trim();
                if !token.is_empty() {
                    return Some(token.to_string());
                }
            }
        }
    }

    None
}

/// Constant-time-ish equality for shared secrets (avoids obvious early-exit leaks).
fn tokens_equal(a: &str, b: &str) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.bytes()
        .zip(b.bytes())
        .fold(0u8, |acc, (x, y)| acc | (x ^ y))
        == 0
}

fn authorize(headers: &HeaderMap, auth: &AuthConfig) -> Result<(), HttpResponse> {
    match &auth.token {
        // Auth not configured: open endpoint (network policy / mTLS must protect it).
        None => Ok(()),
        // Auth configured: fail closed unless a matching shared secret is presented.
        Some(expected) => match extract_presented_token(headers) {
            Some(presented) if tokens_equal(&presented, expected) => Ok(()),
            _ => Err(HttpResponse::Unauthorized()
                .insert_header((header::WWW_AUTHENTICATE, "Bearer"))
                .body("Unauthorized")),
        },
    }
}

#[get("/heartbeat/poop")]
async fn heartbeat(req: HttpRequest, auth: web::Data<AuthConfig>) -> impl Responder {
    if let Err(resp) = authorize(req.headers(), auth.get_ref()) {
        return resp;
    }

    let mut last = LAST_SEEN.lock().unwrap();
    *last = Instant::now();
    info!("Heartbeat received at {:?}", *last);
    HttpResponse::Ok().body("OK")
}

fn app_config(cfg: &mut web::ServiceConfig, auth: AuthConfig) {
    cfg.app_data(web::Data::new(auth)).service(heartbeat);
}

#[tokio::main]
async fn main() -> std::io::Result<()> {
    // Initialize logging and load .env
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

    let auth_token = env::var("HEARTBEAT_AUTH_TOKEN")
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());
    if auth_token.is_none() {
        warn!(
            "HEARTBEAT_AUTH_TOKEN is not set; /heartbeat/poop accepts unauthenticated requests. \
             Restrict network access (firewall, NetworkPolicy, mTLS) or set HEARTBEAT_AUTH_TOKEN."
        );
    } else {
        info!("Heartbeat authentication enabled (shared-secret header required)");
    }
    let auth = AuthConfig { token: auth_token };

    // Spawn the staleness-check task
    let mut connection_missing: bool = false;
    let client = Client::new();
    spawn(async move {
        loop {
            let mut time_interval = check_interval;
            if connection_missing {
                info!("Starting debounce now that we alerted");
                time_interval = debounce_secs;
            }
            time::sleep(Duration::from_secs(time_interval)).await;
            connection_missing = false;
            let last = *LAST_SEEN.lock().unwrap();
            let elapsed = last.elapsed().as_secs();

            if elapsed > timeout_secs {
                warn!(
                    "No heartbeat for {}s (> {}s). Sending Pushover alert.",
                    elapsed, timeout_secs
                );
                let pushover_params = [
                    ("token", pushover_token.as_str()),
                    ("user", pushover_user.as_str()),
                    ("message", "❌ Poop Monitor is offline!"),
                ];
                let _ = client
                    .post("https://api.pushover.net/1/messages.json")
                    .form(&pushover_params)
                    .send()
                    .inspect(|res: &Result<Response, ReqwestError>| match res {
                        Ok(r) => {
                            info!("Pushover status: {}", r.status());
                            connection_missing = true;
                        }
                        Err(e) => warn!("Failed to send Pushover: {}", e),
                    })
                    .await;
                // Prevent repeat alerts until next heartbeat resets LAST_SEEN
                let mut last = LAST_SEEN.lock().unwrap();
                *last = Instant::now();
            }
        }
    });

    // Start HTTP server
    HttpServer::new(move || {
        let auth = auth.clone();
        App::new().configure(move |cfg| app_config(cfg, auth.clone()))
    })
    .bind(("0.0.0.0", 3000))?
    .run()
    .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use actix_web::{http::StatusCode, test, App};

    async fn call_heartbeat(auth: AuthConfig, headers: Vec<(&str, &str)>) -> (StatusCode, String) {
        let app = test::init_service(
            App::new().configure(move |cfg| app_config(cfg, auth.clone())),
        )
        .await;

        let mut req = test::TestRequest::get().uri("/heartbeat/poop");
        for (name, value) in headers {
            req = req.insert_header((name, value));
        }
        let resp = test::call_service(&app, req.to_request()).await;
        let status = resp.status();
        let body = test::read_body(resp).await;
        let body = String::from_utf8(body.to_vec()).unwrap();
        (status, body)
    }

    #[actix_web::test]
    async fn heartbeat_ok_when_auth_disabled() {
        let (status, body) = call_heartbeat(AuthConfig { token: None }, vec![]).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body, "OK");
    }

    #[actix_web::test]
    async fn heartbeat_unauthorized_when_auth_configured_and_missing_header() {
        let auth = AuthConfig {
            token: Some("super-secret".into()),
        };
        let (status, body) = call_heartbeat(auth, vec![]).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
        assert_eq!(body, "Unauthorized");
    }

    #[actix_web::test]
    async fn heartbeat_unauthorized_when_wrong_token() {
        let auth = AuthConfig {
            token: Some("super-secret".into()),
        };
        let (status, _) = call_heartbeat(
            auth,
            vec![("X-Heartbeat-Token", "wrong-token")],
        )
        .await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
    }

    #[actix_web::test]
    async fn heartbeat_ok_with_x_heartbeat_token() {
        let auth = AuthConfig {
            token: Some("super-secret".into()),
        };
        let (status, body) = call_heartbeat(
            auth,
            vec![("X-Heartbeat-Token", "super-secret")],
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body, "OK");
    }

    #[actix_web::test]
    async fn heartbeat_ok_with_authorization_bearer() {
        let auth = AuthConfig {
            token: Some("super-secret".into()),
        };
        let (status, body) = call_heartbeat(
            auth,
            vec![("Authorization", "Bearer super-secret")],
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body, "OK");
    }

    #[actix_web::test]
    async fn extract_token_prefers_x_heartbeat_token() {
        let mut headers = HeaderMap::new();
        headers.insert(
            header::HeaderName::from_static("x-heartbeat-token"),
            header::HeaderValue::from_static("from-x"),
        );
        headers.insert(
            header::AUTHORIZATION,
            header::HeaderValue::from_static("Bearer from-auth"),
        );
        assert_eq!(
            extract_presented_token(&headers).as_deref(),
            Some("from-x")
        );
    }

    #[actix_web::test]
    async fn tokens_equal_rejects_length_mismatch() {
        assert!(!tokens_equal("abc", "ab"));
        assert!(tokens_equal("abc", "abc"));
        assert!(!tokens_equal("abc", "abd"));
    }
}
