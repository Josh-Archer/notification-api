use actix_web::{get, App, HttpServer, Responder};
use dotenv::dotenv;
use once_cell::sync::Lazy;
use reqwest::{Client, Response, Error as ReqwestError};
use std::{
    env,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};
use tokio::{spawn, time};
use log::{info, warn};
use futures::FutureExt;

// Shared state to hold the last-seen Instant
static LAST_SEEN: Lazy<Arc<Mutex<Instant>>> = Lazy::new(|| {
    // Initialize to now so we don't immediately trigger alert on startup
    Arc::new(Mutex::new(Instant::now()))
});

#[get("/heartbeat/poop")]
async fn heartbeat() -> impl Responder {
    let mut last = LAST_SEEN.lock().unwrap();
    *last = Instant::now();
    info!("Heartbeat received at {:?}", *last);
    "OK"
}

#[tokio::main]
async fn main() -> std::io::Result<()> {
    // Initialize logging and load .env
    env_logger::init();
    dotenv().ok();

    let pushover_token = env::var("PUSHOVER_TOKEN")
        .expect("PUSHOVER_TOKEN must be set in .env");
    let pushover_user = env::var("PUSHOVER_USER")
        .expect("PUSHOVER_USER must be set in .env");

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
                    .inspect(|res: &Result<Response, ReqwestError>| {
                        match res {
                            Ok(r)  => {
                                info!("Pushover status: {}", r.status());
                                connection_missing = true;
                            },
                            Err(e) => warn!("Failed to send Pushover: {}", e),
                        }
                    })
                    .await;
                // Prevent repeat alerts until next heartbeat resets LAST_SEEN
                let mut last = LAST_SEEN.lock().unwrap();
                *last = Instant::now();
            }
        }
    });

    // Start HTTP server
    HttpServer::new(|| App::new().service(heartbeat))
        .bind(("0.0.0.0", 3000))?
        .run()
        .await
}
