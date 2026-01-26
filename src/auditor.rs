use anyhow::Result;
use curve25519_dalek::{
    constants::RISTRETTO_BASEPOINT_POINT, ristretto::RistrettoPoint, scalar::Scalar,
};
use rand_core::OsRng;
use tokio::net::TcpStream;

use givre::ciphersuite::AdditionalEntropy;
use givre::generic_ec::{NonZero, SecretScalar, curves::Ed25519};
use givre::key_share::DirtyKeyShare;
use givre::keygen::key_share::Valid;

use crate::keygen::generate_private_share;

/// ===============================
/// Types
/// ===============================

#[derive(Clone)]
pub struct AuditorKeyShare {
    pub share: Scalar,              // x_i
    pub public_key: RistrettoPoint, // x · G
}

#[derive(Clone)]
pub struct AuditorCiphertext {
    pub c1: RistrettoPoint, // r · G
    pub c2: RistrettoPoint, // m · G + r · PK
}

/// ===============================
/// Auditor Key Generation (DKG)
/// ===============================
///
/// Run ONCE per mint.
pub async fn run_auditor_keygen(
    socket: TcpStream,
    id: u64,
    n: u16,
    session: &[u8],
) -> Result<AuditorKeyShare> {
    let valid_share: Valid<DirtyKeyShare<Ed25519>> =
        generate_private_share(socket, id, n, session).await?;

    // Extract scalar share correctly
    let x_i: &NonZero<SecretScalar<Ed25519>> = &valid_share.x;
    let encoded = <NonZero<SecretScalar<Ed25519>> as AdditionalEntropy<
        givre::ciphersuite::Ed25519,
    >>::to_bytes(x_i);

    let scalar_bytes: [u8; 32] = encoded
        .as_ref()
        .try_into()
        .expect("scalar must be 32 bytes");
    
    let share = Scalar::from_bytes_mod_order(scalar_bytes);
    
    // Reconstruct shared public key (2-of-2 => multiply)
    let public_key = RISTRETTO_BASEPOINT_POINT * share * Scalar::from(n as u64);

    Ok(AuditorKeyShare { share, public_key })
}

/// ===============================
/// ElGamal Encryption (utility)
/// ===============================
pub fn encrypt(public_key: &RistrettoPoint, amount: Scalar) -> AuditorCiphertext {
    let mut rng = OsRng;
    let r = Scalar::random(&mut rng);

    let c1 = r * RISTRETTO_BASEPOINT_POINT;
    let c2 = amount * RISTRETTO_BASEPOINT_POINT + r * public_key;

    AuditorCiphertext { c1, c2 }
}

/// ===============================
/// Partial Decryption (MPC)
/// ===============================
pub fn partial_decrypt(
    key_share: &AuditorKeyShare,
    ciphertext: &AuditorCiphertext,
) -> RistrettoPoint {
    key_share.share * ciphertext.c1
}

/// ===============================
/// Combine Partial Decryptions
/// ===============================
pub fn combine_partial_decryptions(
    ciphertext: &AuditorCiphertext,
    partials: Vec<RistrettoPoint>,
) -> RistrettoPoint {
    let sum: RistrettoPoint = partials.into_iter().sum();
    ciphertext.c2 - sum
}
