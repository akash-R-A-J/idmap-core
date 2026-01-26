mod client;

use anyhow::Result;
use dkg_tcp::env::init_env;
use tracing_subscriber::fmt;

/// Entry point (only calls run_client)
#[tokio::main]
async fn main() -> Result<()> {
    fmt()
        .with_max_level(tracing::Level::INFO)
        .with_target(true) 
        .with_thread_ids(true)
        .init();

    // load the env variables
    init_env(env!("CARGO_MANIFEST_DIR"));

    // start the process
    client::run_client().await
}
