use anyhow::Result;
use base64::{Engine as _, engine::general_purpose};
use futures::StreamExt;
use std::sync::Arc;
use tokio::time::{Duration, timeout};
use tokio::{net::TcpListener, task};
use tracing::{debug, error, info, warn};

use ::redis::Client;
use solana_sdk::signature::Signature;

use anyhow::anyhow;
use givre::generic_ec::curves::Ed25519;
use givre::key_share::DirtyKeyShare;
use givre::keygen::key_share::Valid;
use serde_json::{Value, json};

use dkg_tcp::auditor::decrypt::PartialDecryption;

use dkg_tcp::auditor::decrypt::AuditorCiphertext;
use dkg_tcp::auditor::{combine_partial_decryptions, partial_decrypt};
use dkg_tcp::env::ServerEnv;
use dkg_tcp::env::init_env;
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

    // ---- AUDITOR server task ----
    let auditor_store: store::AuditorStore = Arc::new(Default::default());

    let auditor_task = {
        let redis = redis_client.clone();
        let auditor_store = auditor_store.clone();
        let auditor_bind_addr = env.auditor_dkg_addr.clone();

        task::spawn(async move {
            if let Err(e) =
                run_auditor_dkg_server(redis, auditor_store, id, auditor_bind_addr).await
            {
                error!("[SERVER-AUDITOR] Error: {:?}", e);
            }
        })
    };

    let auditor_decrypt_task = {
        let redis = redis_client.clone();
        let auditor_store = auditor_store.clone();

        task::spawn(async move {
            if let Err(e) = run_auditor_decrypt_server(redis, auditor_store, id).await {
                error!("[SERVER-AUDITOR-DECRYPT] Error: {:?}", e);
            }
        })
    };

    info!("[SERVER] Running DKG + SIGN servers concurrently...");
    let _ = tokio::join!(dkg_task, sign_task, auditor_task, auditor_decrypt_task);
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

use dkg_tcp::auditor::run_auditor_dkg;

async fn run_auditor_dkg_server(
    redis_client: Arc<Client>,
    auditor_store: store::AuditorStore,
    id: u64,
    auditor_bind_addr: String,
) -> Result<()> {
    info!("[AUDITOR] Subscribing to `auditor-dkg-start`");
    let (mut pubsub, mut pub_conn) = redis::subscribe(&redis_client, "auditor-dkg-start").await?;

    while let Some(msg) = pubsub.on_message().next().await {
        let parsed: serde_json::Value = redis::parse(&msg)?;
        debug!("[AUDITOR] Redis msg: {}", parsed);

        if parsed["action"] != "start-auditor-dkg" {
            continue;
        }

        let mint = parsed["mint"].as_str().unwrap();

        info!("[AUDITOR] Starting auditor DKG for mint {}", mint);

        let keyshare = run_auditor_dkg(
            id,
            &auditor_bind_addr, // bind
            "",                 // unused for node 0
        )
        .await?;

        auditor_store
            .write()
            .await
            .insert(mint.to_string(), keyshare.clone());

        redis::publish(
            &mut pub_conn,
            "auditor-dkg-result",
            serde_json::json!({
                "mint": mint,
                "auditor_pk": bs58::encode(
                    keyshare.public_key.compress().as_bytes()
                ).into_string()
            }),
        )
        .await?;

        info!("[AUDITOR] Auditor key generated for mint {}", mint);
    }

    Ok(())
}

async fn run_auditor_decrypt_server(
    redis_client: Arc<Client>,
    auditor_store: store::AuditorStore,
    _id: u64,
) -> Result<()> {
    info!("[AUDITOR-DECRYPT] Subscribed");

    // listen for decrypt requests
    let (mut start_sub, _) = redis::subscribe(&redis_client, "auditor-decrypt-start").await?;

    // listen for peer partials
    let (mut partial_sub, mut pub_conn) =
        redis::subscribe(&redis_client, "auditor-decrypt-partial").await?;

    while let Some(msg) = start_sub.on_message().next().await {
        let parsed: Value = redis::parse(&msg)?;

        let mint = parsed["mint"]
            .as_str()
            .ok_or_else(|| anyhow!("missing mint"))?;

        let ciphertext: AuditorCiphertext = serde_json::from_value(parsed["ciphertext"].clone())?;

        // fetch auditor key share
        let share = auditor_store
            .read()
            .await
            .get(mint)
            .cloned()
            .ok_or_else(|| anyhow!("missing auditor share"))?;

        // server partial
        let my_partial = partial_decrypt(&share, &ciphertext)?;

        redis::publish(
            &mut pub_conn,
            "auditor-decrypt-partial",
            json!({
                "mint": mint,
                "node_id": 0,
                "partial": my_partial,
            }),
        )
        .await?;

        let mut partials = vec![my_partial];

        // wait for client partial (2-of-2)
        while partials.len() < 2 {
            let peer_msg = partial_sub
                .on_message()
                .next()
                .await
                .ok_or_else(|| anyhow!("peer disconnected"))?;

            let peer: Value = redis::parse(&peer_msg)?;

            if peer["mint"] != mint {
                continue;
            }

            let p: PartialDecryption = serde_json::from_value(peer["partial"].clone())?;

            partials.push(p);
        }

        // combine
        let m_g = combine_partial_decryptions(&ciphertext, &partials)?;

        redis::publish(
            &mut pub_conn,
            "auditor-decrypt-result",
            json!({
                "mint": mint,
                "value_commitment": bs58::encode(
                    m_g.compress().as_bytes()
                ).into_string()
            }),
        )
        .await?;

        info!("[AUDITOR-DECRYPT] Decryption completed for mint {}", mint);
    }

    Ok(())
}
