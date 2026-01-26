use anyhow::Result;
use base64::{Engine as _, engine::general_purpose};
use futures::StreamExt;
use std::sync::Arc;
use tokio::time::{Duration, timeout};
use tokio::{net::TcpListener, task};
use tracing::{debug, error, info, warn};

use ::redis::Client;
use solana_sdk::signature::Signature;

use givre::generic_ec::curves::Ed25519;
use givre::key_share::DirtyKeyShare;
use givre::keygen::key_share::Valid;

use dkg_tcp::env::ServerEnv;
use dkg_tcp::env::init_env;

// use dkg_tcp::env::env_loader::init_env;
use dkg_tcp::{keygen, sign};
use dkg_tcp::{redis, store, tcp};

type ShareStore = store::ShareStore<Valid<DirtyKeyShare<Ed25519>>>;

/// Starts both DKG and Signing servers concurrently.
pub async fn run_server() -> Result<()> {
    // Load .env files (local + root)
    init_env(env!("CARGO_MANIFEST_DIR"));

    let env = ServerEnv::load()?;
    let id = env.base.node_id;
    let n = env.base.n;

    let dkg_addr = env.dkg_addr.clone();
    let sign_addr = env.sign_addr.clone();
    let default_session = env.base.default_session.clone();

    info!(
        "Starting server [node_id={}] on DKG={} SIGN={} Redis={}",
        id, env.dkg_addr, env.sign_addr, env.base.redis_url
    );

    let redis_client = Arc::new(Client::open(env.base.redis_url.clone())?);
    let share_store: ShareStore = Arc::new(Default::default());

    // ---- DKG server task ----
    let dkg_task = {
        let redis = redis_client.clone();
        let store = share_store.clone();
        let default_session = default_session.clone();
        let dkg_addr = dkg_addr.clone();

        task::spawn(async move {
            if let Err(e) = run_dkg_server(redis, store, id, n, &dkg_addr, &default_session).await {
                error!("[SERVER-DKG] Error: {:?}", e);
            }
        })
    };

    // ---- SIGN server task ----
    let sign_task = {
        let redis = redis_client.clone();
        let store = share_store.clone();
        let default_session = default_session.clone();
        let sign_addr = sign_addr.clone();

        task::spawn(async move {
            if let Err(e) = run_sign_server(redis, store, id, &sign_addr, &default_session).await {
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
    info!("[DKG] Subscribing to Redis channel `dkg-start`");
    let (mut pubsub, mut pub_conn) = redis::subscribe(&redis_client, "dkg-start").await?;

    let listener = TcpListener::bind(addr).await?;
    info!("[DKG] TCP listener active on {}", addr);

    while let Some(msg) = pubsub.on_message().next().await {
        let parsed: serde_json::Value = redis::parse(&msg)?;
        debug!("[DKG] Redis msg: {}", parsed);

        if parsed["action"] != "startdkg" {
            debug!("[DKG] Ignored unrelated message");
            continue;
        }

        let session = parsed["session"].as_str().unwrap_or(default_session);
        info!("[DKG] Starting keygen session {}", session);

        // Accept peer connection with timeout
        let (socket, peer) = tcp::accept(&listener, 10).await?;
        info!("[DKG] Connected to peer {:?}", peer);

        // Run DKG with timeout
        let share = match timeout(
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
                error!("[DKG] DKG timed out for session {}", session);
                continue;
            }
        };

        store::put(&share_store, id, session, share.clone()).await;
        info!("[DKG] Stored share for session {}", session);

        redis::publish(
            &mut pub_conn,
            "dkg-result",
            serde_json::json!({
                "id": parsed["id"],
                "result_type": "dkg-result",
                "data": bs58::encode(share.shared_public_key().to_bytes(true)).into_string(),
                "server_id": id,
            }),
        )
        .await?;
        info!("[DKG] DKG result published successfully!");
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
    info!("[SIGN] Subscribing to Redis channel `sign-start`");
    let (mut pubsub, mut pub_conn) = redis::subscribe(&redis_client, "sign-start").await?;

    let listener = TcpListener::bind(addr).await?;
    info!("[SIGN] TCP listener active on {}", addr);

    while let Some(msg) = pubsub.on_message().next().await {
        let parsed: serde_json::Value = redis::parse(&msg)?;
        debug!("[SIGN] Redis msg: {}", parsed);

        if parsed["action"] != "sign" {
            debug!("[SIGN] Ignored unrelated message");
            continue;
        }

        let session = parsed["session"].as_str().unwrap_or(default_session);
        info!("[SIGN] Starting signing for session {}", session);

        let Some(share) = store::get(&share_store, id, session).await else {
            warn!("[SIGN] No share found for node {} session {}", id, session);
            continue;
        };

        let message_bytes =
            general_purpose::STANDARD.decode(parsed["message"].as_str().unwrap_or_default())?;

        let (socket, peer) = tcp::accept(&listener, 10).await?;
        info!("[SIGN] Connected to peer {:?}", peer);

        match timeout(
            Duration::from_secs(15),
            sign::run_signing_phase(id, share, socket, message_bytes),
        )
        .await
        {
            Ok(Ok((r, z))) => {
                let sig = Signature::try_from([r, z].concat())
                    .map_err(|_| anyhow::anyhow!("invalid Solana signature"))?;

                redis::publish(
                    &mut pub_conn,
                    "sign-result",
                    serde_json::json!({
                        "id": parsed["id"],
                        "result_type": "sign-result",
                        "data": sig.to_string(),
                        "server_id": id,
                    }),
                )
                .await?;
                info!("[SIGN] Signature published successfully!");
            }
            Ok(Err(e)) => error!("[SIGN] Signing failed: {:?}", e),
            Err(_) => error!("[SIGN] Signing timed out for session {}", session),
        }
    }

    Ok(())
}
