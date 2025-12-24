use crate::transport::{TcpIncoming, TcpOutgoing};

use anyhow::Result;
use rand_core::OsRng;
use round_based::MpcParty;
use sha2::Sha256;
use std::convert::TryInto;
use tokio::net::TcpStream;
use tracing::{error, info};

use givre::ciphersuite::AdditionalEntropy;
use givre::generic_ec::{EncodedScalar, NonZero, SecretScalar, curves::Ed25519};
use givre::key_share::DirtyKeyShare;
use givre::keygen::{ExecutionId, ThresholdMsg, keygen};
use givre::keygen::{key_share::Valid, security_level::SecurityLevel128};

use solana_pubkey::Pubkey;
use solana_rpc_client::rpc_client::RpcClient;

type KeygenMsg = ThresholdMsg<Ed25519, SecurityLevel128, Sha256>;

/// Runs the DKG protocol for this participant and returns the generated private share.
pub async fn generate_private_share(
    socket: tokio::net::TcpStream,
    id: u64,
    n: u16,
    session: &[u8],
) -> Result<Valid<DirtyKeyShare<Ed25519>>> {
    let std_stream: std::net::TcpStream = socket.into_std()?;
    std_stream.set_nonblocking(true)?;
    let std_stream_dkg: std::net::TcpStream = std_stream.try_clone()?;

    // Convert std streams to tokio streams
    let reader_stream_dkg = TcpStream::from_std(std_stream_dkg.try_clone()?)?;
    let writer_stream_dkg = TcpStream::from_std(std_stream_dkg)?;

    let incoming = TcpIncoming::<KeygenMsg>::new(reader_stream_dkg, id);
    let outgoing = TcpOutgoing::<KeygenMsg>::new(writer_stream_dkg);

    // Initialize builder for 2-of-2 threshold (adjust as needed)
    let eid = ExecutionId::new(session);
    let builder = keygen::<Ed25519>(eid, id as u16, n).set_threshold(2);
    let mut rng = OsRng;

    // Start MPC party
    let party = MpcParty::connected((incoming, outgoing));

    let valid_share = match builder.start(&mut rng, party).await {
        Ok(share) => share,
        Err(e) => {
            error!("DKG failed for participant {}: {:?}", id, e);
            return Err(e.into());
        }
    };

    // Derive private share (for caller usage)
    let private_share: &NonZero<SecretScalar<Ed25519>> = &valid_share.x;
    let encoded: EncodedScalar<Ed25519> = <NonZero<SecretScalar<Ed25519>> as AdditionalEntropy<
        givre::ciphersuite::Ed25519,
    >>::to_bytes(private_share);
    let _private_bytes: [u8; 32] = encoded.as_ref().try_into().unwrap();

    Ok(valid_share)
}

/// Helper: Airdrops `lamports` to the given Solana address (Devnet)

pub fn airdrop_funds(address: &str, lamports: u64) -> Result<Pubkey> {
    let rpc_endpoints = [
        "https://api.devnet.solana.com",
        "https://rpc.ankr.com/solana_devnet",
        "https://devnet.rpcpool.com",
        "https://rpc-devnet.helius.xyz",
        "https://devnet-rpc.triton.one",
    ];

    let pubkey = Pubkey::from_str_const(address);
    let mut success = false;

    for url in rpc_endpoints.iter() {
        let rpc = RpcClient::new(url.to_string());

        for attempt in 1..=3 {
            match rpc.request_airdrop(&pubkey, lamports) {
                Ok(sig) => match rpc.confirm_transaction(&sig) {
                    Ok(confirmed) if confirmed => {
                        info!(
                            "Airdrop successful via {} (attempt {}) — {} lamports sent to {}",
                            url, attempt, lamports, address
                        );
                        success = true;
                        break;
                    }
                    Ok(_) | Err(_) => {
                        std::thread::sleep(std::time::Duration::from_secs(2));
                    }
                },
                Err(e) => {
                    error!("Airdrop attempt {} via {} failed: {:?}", attempt, url, e);
                    std::thread::sleep(std::time::Duration::from_secs(2));
                }
            }
        }

        if success {
            break;
        }
    }

    let rpc = RpcClient::new("https://api.devnet.solana.com".to_string());
    if let Err(e) = rpc.get_balance(&pubkey) {
        error!("Failed to fetch balance for {}: {:?}", address, e);
    }

    if !success {
        error!("Airdrop failed on all RPC endpoints for {}", address);
    }

    Ok(pubkey)
}
