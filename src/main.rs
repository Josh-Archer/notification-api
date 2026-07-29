use dotenv::dotenv;
use log::info;
use notification_api::{run_server, spawn_staleness_watcher, NotifyConfig, WatcherConfig};

#[tokio::main]
async fn main() -> std::io::Result<()> {
    env_logger::init();
    dotenv().ok();

    let notify = NotifyConfig::from_env().unwrap_or_else(|e| {
        eprintln!("configuration error: {e}");
        std::process::exit(1);
    });
    let watcher = WatcherConfig::from_env();

    info!(
        "Starting notification-api with backends: {:?}",
        notify.backend_names()
    );
    info!(
        "Watcher: timeout={}s check={}s debounce={}s",
        watcher.timeout_secs, watcher.check_interval_secs, watcher.debounce_secs
    );

    spawn_staleness_watcher(notify.clone(), watcher);
    run_server(notify).await
}
