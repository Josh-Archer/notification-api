use dotenv::dotenv;
use notification_api::{NotifyConfig, WatcherConfig};

/// Shipped entrypoint. Configuration, `/healthz`, heartbeat handling, and the
/// outage/recovery watcher are implemented once in the library.
#[tokio::main]
async fn main() -> std::io::Result<()> {
    env_logger::init();
    dotenv().ok();

    let notify = NotifyConfig::from_env().expect("invalid notification configuration");
    let watcher = WatcherConfig::from_env();
    notification_api::run(notify, watcher).await
}
