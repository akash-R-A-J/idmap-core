use anyhow::Result;
use futures::StreamExt;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::RwLock;
use tokio::time::{Duration, timeout};
use tokio::{net::TcpListener, task};
use tracing::{debug, error, info, warn};

use dkg_tcp::{env_loader::init_env, keygen, sign};
use givre::generic_ec::curves::Ed25519;
use givre::key_share::DirtyKeyShare;
use givre::keygen::key_share::Valid;
use redis::aio::MultiplexedConnection;
use redis::{AsyncCommands, Client};
use solana_sdk::signature::Signature;
use std::env;

type ShareStore = Arc<RwLock<HashMap<(u64, String), Valid<DirtyKeyShare<Ed25519>>>>>;

/// Structured environment configuration for the DKG + Signing servers.
#[derive(Debug, Clone)]
struct EnvConfig {
    n: u16,
    node_id: u64,
    redis_url: String,
    dkg_addr: String,
    sign_addr: String,
    default_session: String,
}

impl EnvConfig {
    /// Load all env variables and apply safe defaults.
    fn load() -> Result<Self> {
        Ok(Self {
            n: env::var("N")
                .unwrap_or_else(|_| "2".into())
                .parse::<u16>()
                .expect("N must be numeric"),

            node_id: env::var("NODE_ID")
                .unwrap_or_else(|_| "0".into())
                .parse::<u64>()
                .expect("NODE_ID must be numeric"),

            redis_url: env::var("REDIS_URL").unwrap_or_else(|_| "redis://127.0.0.1:6379".into()),

            dkg_addr: env::var("DKG_SERVER_ADDR").unwrap_or_else(|_| "0.0.0.0:7001".into()),

            sign_addr: env::var("SIGN_SERVER_ADDR").unwrap_or_else(|_| "0.0.0.0:7002".into()),

            default_session: env::var("DEFAULT_SESSION_ID")
                .unwrap_or_else(|_| "session-001".into()),
        })
    }
}

/// Starts both DKG and Signing servers concurrently.
pub async fn run_server() -> Result<()> {
    // Load .env file (works in async contexts too)
    init_env(env!("CARGO_MANIFEST_DIR"));
    let env_config = EnvConfig::load()?;

    info!(
        "Starting server [node_id={}] on DKG={} SIGN={} with Redis={}",
        env_config.node_id, env_config.dkg_addr, env_config.sign_addr, env_config.redis_url
    );

    // Redis clients
    let redis_client_dkg = Arc::new(Client::open(env_config.redis_url.clone())?);
    let redis_client_sign = Arc::new(Client::open(env_config.redis_url.clone())?);

    // Shared in-memory store for DKG shares
    let share_store: ShareStore = Arc::new(RwLock::new(HashMap::new()));

    // Start DKG server
    let dkg_task = {
        let redis = redis_client_dkg.clone();
        let store = share_store.clone();
        let id = env_config.node_id;
        let n = env_config.n;
        let addr = env_config.dkg_addr.clone();
        let default_session = env_config.default_session.clone();

        task::spawn(async move {
            if let Err(e) = run_dkg_server(redis, store, id, n, &addr, &default_session).await {
                error!("[SERVER-DKG] Error: {:?}", e);
            }
        })
    };

    // Start SIGN server
    let sign_task = {
        let redis = redis_client_sign.clone();
        let store = share_store.clone();
        let id = env_config.node_id;
        let addr = env_config.sign_addr.clone();
        let default_session = env_config.default_session.clone();

        task::spawn(async move {
            if let Err(e) = run_sign_server(redis, store, id, &addr, &default_session).await {
                error!("[SERVER-SIGN] Error: {:?}", e);
            }
        })
    };

    info!("[SERVER] Running DKG + SIGN servers concurrently...");
    let _ = tokio::join!(dkg_task, sign_task);
    Ok(())
}

/// Handles DKG key generation requests.
async fn run_dkg_server(
    redis_client: Arc<Client>,
    share_store: ShareStore,
    id: u64,
    n: u16,
    addr: &str,
    default_session: &str,
) -> Result<()> {
    let mut pubsub = redis_client.get_async_pubsub().await?;
    pubsub.subscribe("dkg-start").await?;
    info!("[DKG] Listening on Redis channel `dkg-start`");

    let mut pub_conn: MultiplexedConnection =
        redis_client.get_multiplexed_async_connection().await?;
    let listener = TcpListener::bind(addr).await?;
    info!("[DKG] TCP listener active on {}", addr);

    while let Some(msg) = pubsub.on_message().next().await {
        let payload: String = msg.get_payload()?;
        debug!("[DKG] Redis msg: {}", payload);

        let parsed: serde_json::Value = serde_json::from_str(&payload)?;
        if parsed["action"] != "startdkg" {
            debug!("[DKG] Ignored unrelated message");
            continue;
        }

        let session = parsed["session"].as_str().unwrap_or(default_session);
        info!("[DKG] Starting keygen session {}", session);

        // Timeout for TCP accept (prevents hanging if no peer connects)
        let (socket, peer) = match timeout(Duration::from_secs(10), listener.accept()).await {
            Ok(Ok(s)) => s,
            Ok(Err(e)) => {
                error!("[DKG] TCP accept error: {:?}", e);
                continue;
            }
            Err(_) => {
                warn!(
                    "[DKG] Timeout waiting for peer connection in session {}",
                    session
                );
                continue;
            }
        };
        info!("[DKG] Connected to peer {:?}", peer);

        // Timeout for DKG computation (prevents indefinite wait)
        let shares = match timeout(
            Duration::from_secs(30),
            keygen::generate_private_share(socket, id, n, session.as_bytes()),
        )
        .await
        {
            Ok(Ok(s)) => s,
            Ok(Err(e)) => {
                error!("[DKG] Key generation failed: {:?}", e);
                continue;
            }
            Err(_) => {
                error!("[DKG] DKG phase timed out for session {}", session);
                continue;
            }
        };

        let pubkey = bs58::encode(shares.shared_public_key().to_bytes(true)).into_string();

        {
            let mut store = share_store.write().await;
            store.insert((id, session.to_string()), shares.clone());
        }

        info!("[DKG] Stored share for session {}", session);

        let response = serde_json::json!({
            "id": parsed["id"],
            "result_type": "dkg-result",
            "data": pubkey,
            "server_id": id,
        });

        if let Err(e) = pub_conn
            .publish::<_, _, ()>("dkg-result", response.to_string())
            .await
        {
            error!("[DKG] Failed to publish DKG result: {:?}", e);
        } else {
            info!("[DKG] DKG result published successfully!");
        }
    }

    Ok(())
}

/// Handles signing requests.
async fn run_sign_server(
    redis_client: Arc<Client>,
    share_store: ShareStore,
    id: u64,
    addr: &str,
    default_session: &str,
) -> Result<()> {
    let mut pubsub = redis_client.get_async_pubsub().await?;
    pubsub.subscribe("sign-start").await?;
    info!("[SIGN] Listening on Redis channel `sign-start`");

    let mut pub_conn: MultiplexedConnection =
        redis_client.get_multiplexed_async_connection().await?;
    let listener = TcpListener::bind(addr).await?;
    info!("[SIGN] TCP listener active on {}", addr);

    while let Some(msg) = pubsub.on_message().next().await {
        let payload: String = match msg.get_payload() {
            Ok(p) => p,
            Err(e) => {
                error!("[SIGN] Failed to parse payload: {:?}", e);
                continue;
            }
        };

        debug!("[SIGN] Redis msg: {}", payload);
        let parsed: serde_json::Value = match serde_json::from_str(&payload) {
            Ok(p) => p,
            Err(e) => {
                warn!("[SIGN] Invalid JSON payload: {:?}", e);
                continue;
            }
        };

        if parsed["action"] != "sign" {
            debug!("[SIGN] Ignored unrelated message");
            continue;
        }

        let session = parsed["session"].as_str().unwrap_or(default_session);
        info!("[SIGN] Starting signing for session {}", session);

        // Timeout for client connection
        let (socket, peer) = match timeout(Duration::from_secs(10), listener.accept()).await {
            Ok(Ok(s)) => s,
            Ok(Err(e)) => {
                error!("[SIGN] TCP accept error: {:?}", e);
                continue;
            }
            Err(_) => {
                warn!(
                    "[SIGN] Timeout waiting for client to connect for session {}",
                    session
                );
                continue;
            }
        };
        info!("[SIGN] Connected to peer {:?}", peer);

        let maybe_share = {
            let store = share_store.read().await;
            store.get(&(id, session.to_string())).cloned()
        };

        let valid_share = match maybe_share {
            Some(s) => s,
            None => {
                warn!("[SIGN] No share found for node {} session {}", id, session);
                let error_ack = serde_json::json!({
                    "id": parsed["id"],
                    "result_type": "sign-error",
                    "error": format!("No share found for node {} session {}", id, session),
                    "server_id": id,
                });
                let _ = pub_conn
                    .publish::<_, _, ()>("sign-result", error_ack.to_string())
                    .await;
                continue;
            }
        };

        let message_base64 = parsed["message"].as_str().unwrap_or_default();
        let message_bytes = match base64::decode(message_base64) {
            Ok(b) => b,
            Err(e) => {
                error!("[SIGN] Failed to decode message: {:?}", e);
                continue;
            }
        };

        // Timeout for signing phase itself
        match timeout(
            Duration::from_secs(15),
            sign::run_signing_phase(id, valid_share, socket, message_bytes),
        )
        .await
        {
            Ok(Ok((r, z))) => {
                let sig = [r, z].concat();
                let sol_sig =
                    Signature::try_from(sig.clone()).expect("Invalid Solana signature conversion");

                let response = serde_json::json!({
                    "id": parsed["id"],
                    "result_type": "sign-result",
                    "data": sol_sig.to_string(),
                    "server_id": id,
                });

                pub_conn
                    .publish::<_, _, ()>("sign-result", response.to_string())
                    .await?;
                info!("[SIGN] Signature published successfully!");
            }
            Ok(Err(e)) => {
                error!("[SIGN] Signing failed: {:?}", e);
                let fail_ack = serde_json::json!({
                    "id": parsed["id"],
                    "result_type": "sign-error",
                    "error": format!("Signing failed: {}", e),
                    "server_id": id,
                });
                let _ = pub_conn
                    .publish::<_, _, ()>("sign-result", fail_ack.to_string())
                    .await;
            }
            Err(_) => {
                error!("[SIGN] Signing phase timed out for session {}", session);
                let timeout_ack = serde_json::json!({
                    "id": parsed["id"],
                    "result_type": "sign-error",
                    "error": "Signing phase timed out",
                    "server_id": id,
                });
                let _ = pub_conn
                    .publish::<_, _, ()>("sign-result", timeout_ack.to_string())
                    .await;
            }
        }
    }

    Ok(())
}
