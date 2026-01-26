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

#[derive(Debug, Clone)]
pub struct AuditorKeyShare {
    pub share: Scalar,              // x_i
    pub public_key: RistrettoPoint, // (x₁ + x₂)·G
}

#[derive(Debug, Serialize, Deserialize)]
struct AuditorPkMsg {
    pub pk_i: [u8; 32],
}

/// 2-of-2 ElGamal auditor DKG (Token-2022 compatible)
pub async fn run_auditor_dkg(
    id: u64,
    listen_addr: &str,
    connect_addr: &str,
) -> Result<AuditorKeyShare> {
    info!("[AUDITOR-DKG] Node {} starting", id);

    let mut rng = OsRng;
    let x_i = Scalar::random(&mut rng);
    let pk_i = (x_i * RISTRETTO_BASEPOINT_POINT).compress().to_bytes();

    let socket = if id == 0 {
        let listener = TcpListener::bind(listen_addr).await?;
        let (s, _) = listener.accept().await?;
        s
    } else {
        TcpStream::connect(connect_addr).await?
    };

    let mut framed = Framed::new(socket, LengthDelimitedCodec::new());

    framed
        .send(serde_json::to_vec(&AuditorPkMsg { pk_i })?.into())
        .await?;

    let bytes = framed
        .next()
        .await
        .ok_or_else(|| anyhow::anyhow!("peer disconnected"))??;

    let peer: AuditorPkMsg = serde_json::from_slice(&bytes)?;
    let peer_pk = CompressedRistretto(peer.pk_i)
        .decompress()
        .ok_or_else(|| anyhow::anyhow!("invalid peer pk"))?;

    let public_key =
        (CompressedRistretto(pk_i).decompress().unwrap()) + peer_pk;

    Ok(AuditorKeyShare { share: x_i, public_key })
}
