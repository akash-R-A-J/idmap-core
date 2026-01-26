use anyhow::Result;
use base64::{Engine as _, engine::general_purpose};
use futures::StreamExt;
use std::sync::Arc;
use tokio::{net::TcpStream, task};
use tracing::{debug, error, info, warn};

use ::redis::Client;
use solana_sdk::signature::Signature;

use givre::generic_ec::curves::Ed25519;
use givre::key_share::DirtyKeyShare;
use givre::keygen::key_share::Valid;

use dkg_tcp::auditor::decrypt::AuditorCiphertext;
use dkg_tcp::auditor::run_auditor_dkg;
use dkg_tcp::auditor::{partial_decrypt};
use dkg_tcp::env::ClientEnv;
use dkg_tcp::{keygen, sign};
use dkg_tcp::{redis, store, tcp};

type ShareStore = store::ShareStore<Valid<DirtyKeyShare<Ed25519>>>;

/// Starts DKG + SIGN clients concurrently.
pub async fn run_client() -> Result<()> {
    let env = ClientEnv::load()?;
    let id = env.base.node_id;
    let n = env.base.n;

    info!(
        "[CLIENT] Starting client node_id={} Redis={}",
        id, env.base.redis_url
    );

    let redis_client = Arc::new(Client::open(env.base.redis_url.clone())?);
    let share_store: ShareStore = Arc::new(Default::default());

    let dkg_task = {
        let redis = redis_client.clone();
        let store = share_store.clone();
        let addr = env.dkg_server_addr.clone();
        let session = env.base.default_session.clone();

        task::spawn(async move {
            if let Err(e) = run_dkg_client(redis, store, id, n, &addr, &session).await {
                error!("[CLIENT-DKG] Error: {:?}", e);
            }
        })
    };

    let sign_task = {
        let redis = redis_client.clone();
        let store = share_store.clone();
        let addr = env.sign_server_addr.clone();

        task::spawn(async move {
            if let Err(e) = run_sign_client(redis, store, id, &addr).await {
                error!("[CLIENT-SIGN] Error: {:?}", e);
            }
        })
    };

    let auditor_store: store::AuditorStore = Arc::new(Default::default());

    let auditor_task = {
        let redis = redis_client.clone();
        let auditor_store = auditor_store.clone();
        let auditor_connect_addr = env.auditor_dkg_server_addr.clone();

        task::spawn(async move {
            if let Err(e) =
                run_auditor_dkg_client(redis, auditor_store, id, auditor_connect_addr).await
            {
                error!("[CLIENT-AUDITOR] Error: {:?}", e);
            }
        })
    };

    let auditor_decrypt_task = {
        let redis = redis_client.clone();
        let auditor_store = auditor_store.clone();

        task::spawn(async move {
            if let Err(e) = run_auditor_decrypt_client(redis, auditor_store, id).await {
                error!("[CLIENT-AUDITOR-DECRYPT] Error: {:?}", e);
            }
        })
    };

    info!("[CLIENT] DKG + SIGN clients running concurrently...");
    let _ = tokio::join!(dkg_task, sign_task, auditor_task, auditor_decrypt_task);
    Ok(())
}

/// Handles DKG phase client logic.
async fn run_dkg_client(
    redis_client: Arc<Client>,
    share_store: ShareStore,
    id: u64,
    n: u16,
    dkg_server_addr: &str,
    default_session: &str,
) -> Result<()> {
    info!("[CLIENT-DKG] Subscribed to `dkg-start`");
    let (mut pubsub, mut pub_conn) = redis::subscribe(&redis_client, "dkg-start").await?;

    while let Some(msg) = pubsub.on_message().next().await {
        let parsed: serde_json::Value = redis::parse(&msg)?;
        debug!("[CLIENT-DKG] Redis msg: {}", parsed);

        if parsed["action"] != "startdkg" {
            continue;
        }

        let session = parsed["session"].as_str().unwrap_or(default_session);
        info!("[CLIENT-DKG] Starting DKG session {}", session);

        let socket = TcpStream::connect(dkg_server_addr).await?;
        let share = keygen::generate_private_share(socket, id, n, session.as_bytes()).await?;

        store::put(&share_store, id, session, share.clone()).await;
        info!("[CLIENT-DKG] Stored share for session {}", session);

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
        info!("[CLIENT-DKG] DKG result published!");
    }

    Ok(())
}

/// Handles SIGN phase client logic.
async fn run_sign_client(
    redis_client: Arc<Client>,
    share_store: ShareStore,
    id: u64,
    sign_server_addr: &str,
) -> Result<()> {
    info!("[CLIENT-SIGN] Subscribed to `sign-start`");
    let (mut pubsub, mut pub_conn) = redis::subscribe(&redis_client, "sign-start").await?;

    while let Some(msg) = pubsub.on_message().next().await {
        let parsed: serde_json::Value = redis::parse(&msg)?;
        debug!("[CLIENT-SIGN] Redis msg: {}", parsed);

        if parsed["action"] != "sign" {
            continue;
        }

        let session = parsed["session"].as_str().unwrap_or("session-001");
        info!("[CLIENT-SIGN] Signing for session {}", session);

        let Some(share) = store::get(&share_store, id, session).await else {
            warn!(
                "[CLIENT-SIGN] No share found for node {} session {}",
                id, session
            );
            continue;
        };

        let message_bytes =
            general_purpose::STANDARD.decode(parsed["message"].as_str().unwrap_or_default())?;

        let socket = tcp::connect(sign_server_addr).await?;
        match sign::run_signing_phase(id, share, socket, message_bytes).await {
            Ok((r, z)) => {
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
                info!("[CLIENT-SIGN] Signature published!");
            }
            Err(e) => error!("[CLIENT-SIGN] Signing failed: {:?}", e),
        }
    }

    Ok(())
}

async fn run_auditor_dkg_client(
    redis_client: Arc<Client>,
    auditor_store: store::AuditorStore,
    id: u64,
    auditor_connect_addr: String,
) -> Result<()> {
    info!("[CLIENT-AUDITOR] Subscribed to `auditor-dkg-start`");
    let (mut pubsub, _) = redis::subscribe(&redis_client, "auditor-dkg-start").await?;

    while let Some(msg) = pubsub.on_message().next().await {
        let parsed: serde_json::Value = redis::parse(&msg)?;
        debug!("[CLIENT-AUDITOR] Redis msg: {}", parsed);

        if parsed["action"] != "start-auditor-dkg" {
            continue;
        }

        let mint = parsed["mint"].as_str().unwrap();

        info!("[CLIENT-AUDITOR] Running auditor DKG for mint {}", mint);

        let keyshare = run_auditor_dkg(
            id,
            "0.0.0.0:0",           // unused
            &auditor_connect_addr, // connect
        )
        .await?;

        auditor_store
    .write()
    .await
    .insert(mint.to_string(), keyshare.clone());

        info!("[CLIENT-AUDITOR] Stored auditor share for mint {}", mint);
    }

    Ok(())
}

async fn run_auditor_decrypt_client(
    redis_client: Arc<Client>,
    auditor_store: store::AuditorStore,
    id: u64,
) -> Result<()> {
    info!("[CLIENT-AUDITOR-DECRYPT] Subscribed to auditor-decrypt-start");

    let (mut pubsub, mut pub_conn) =
        redis::subscribe(&redis_client, "auditor-decrypt-start").await?;

    while let Some(msg) = pubsub.on_message().next().await {
        let parsed: serde_json::Value = redis::parse(&msg)?;

        let mint = parsed["mint"].as_str().unwrap();
        let ciphertext: AuditorCiphertext =
            serde_json::from_value(parsed["ciphertext"].clone())?;

        let share = {
            let store = auditor_store.read().await;
            store.get(mint).cloned()
        };

        let Some(share) = share else {
            warn!("[AUDITOR-DECRYPT] No share for mint {}", mint);
            continue;
        };

        let partial = partial_decrypt(&share, &ciphertext)?;

        redis::publish(
            &mut pub_conn,
            "auditor-decrypt-partial",
            serde_json::json!({
                "id": parsed["id"],
                "node_id": id,
                "partial": partial,
            }),
        )
        .await?;
    }

    Ok(())
}
