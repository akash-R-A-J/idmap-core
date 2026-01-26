use anyhow::Result;
use curve25519_dalek::{
    constants::RISTRETTO_BASEPOINT_POINT,
    ristretto::{CompressedRistretto, RistrettoPoint},
    scalar::Scalar,
};
use futures::{SinkExt, StreamExt};
use rand_core::OsRng;
use serde::{Deserialize, Serialize};
use tokio::net::{TcpListener, TcpStream};
use tokio_util::codec::{Framed, LengthDelimitedCodec};
use tracing::info;

/// ===============================
/// Types
/// ===============================

#[derive(Debug, Clone)]
pub struct AuditorKeyShare {
    /// Private scalar share x_i
    pub share: Scalar,
    /// Aggregated public key PK = (x₁ + x₂)·G
    pub public_key: RistrettoPoint,
}

#[derive(Debug, Serialize, Deserialize)]
struct AuditorPkMsg {
    /// Compressed Ristretto public share
    pub pk_i: [u8; 32],
}

/// ===============================
/// Auditor DKG (2-of-2 ElGamal)
/// ===============================
///
/// One round:
/// - exchange compressed public shares
/// - aggregate
///
pub async fn run_auditor_dkg(
    id: u64,
    listen_addr: &str,
    connect_addr: &str,
) -> Result<AuditorKeyShare> {
    info!("[AUDITOR-DKG] Node {} starting auditor DKG", id);

    // 1. Sample private scalar
    let mut rng = OsRng;
    let x_i = Scalar::random(&mut rng);
    let pk_i_point = x_i * RISTRETTO_BASEPOINT_POINT;
    let pk_i_bytes = pk_i_point.compress().to_bytes();

    // 2. Establish asymmetric connection
    let socket = if id == 0 {
        let listener = TcpListener::bind(listen_addr).await?;
        info!("[AUDITOR-DKG] Node 0 listening on {}", listen_addr);
        let (s, peer) = listener.accept().await?;
        info!("[AUDITOR-DKG] Node 0 accepted {}", peer);
        s
    } else {
        info!("[AUDITOR-DKG] Node 1 connecting to {}", connect_addr);
        TcpStream::connect(connect_addr).await?
    };

    let mut framed = Framed::new(socket, LengthDelimitedCodec::new());

    // 3. Send our PK
    framed
        .send(serde_json::to_vec(&AuditorPkMsg {
            pk_i: pk_i_bytes,
        })?
        .into())
        .await?;

    // 4. Receive peer PK
    let bytes = framed
        .next()
        .await
        .ok_or_else(|| anyhow::anyhow!("peer disconnected"))??;

    let peer_msg: AuditorPkMsg = serde_json::from_slice(&bytes)?;
    let peer_pk_point = CompressedRistretto(peer_msg.pk_i)
        .decompress()
        .ok_or_else(|| anyhow::anyhow!("invalid peer Ristretto point"))?;

    // 5. Aggregate
    let public_key = pk_i_point + peer_pk_point;

    info!("[AUDITOR-DKG] Node {} derived auditor PK", id);

    Ok(AuditorKeyShare {
        share: x_i,
        public_key,
    })
}
