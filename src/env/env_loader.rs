use dotenvy::{dotenv, from_filename};
use std::path::Path;
use std::sync::Once;
use tracing::{info, warn};

static INIT: Once = Once::new();

/// Initialize environment variables for any binary crate.
///
/// 1. Loads the binary crate's local `.env` file.
/// 2. Loads the shared root `.env` file.
/// 3. Falls back to system environment if no `.env` found.
pub fn init_env(crate_dir: &str) {
    INIT.call_once(|| {
        // Try to load the local crate's .env (client/.env or server/.env)
        let local_env = Path::new(crate_dir).join(".env");
        if from_filename(&local_env).is_ok() {
            info!("Loaded local .env file: {}", local_env.display());
        } else {
            warn!("No local .env found at {}", local_env.display());
        }

        // Then load the root-level (shared) .env
        let root_env = Path::new(crate_dir).join("..").join(".env");
        if from_filename(&root_env).is_ok() {
            info!("Loaded shared root .env file: {}", root_env.display());
        } else {
            warn!("No shared .env found at {}", root_env.display());
        }

        // Finally, load default .env if called from root
        if dotenv().is_ok() {
            info!("Loaded default .env file from current directory");
        }

        info!("Environment initialized for crate: {}", crate_dir);
    });
}
