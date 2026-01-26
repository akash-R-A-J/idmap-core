use anyhow::Result;
use curve25519_dalek::ristretto::{CompressedRistretto, RistrettoPoint};

use super::decrypt::{AuditorCiphertext, PartialDecryption};
use super::auditor_dkg::AuditorKeyShare;

/// d_i = x_i · c1
pub fn partial_decrypt(
    key_share: &AuditorKeyShare,
    ciphertext: &AuditorCiphertext,
) -> Result<PartialDecryption> {
    let c1 = CompressedRistretto(ciphertext.c1)
        .decompress()
        .ok_or_else(|| anyhow::anyhow!("invalid c1"))?;

    let di = key_share.share * c1;

    Ok(PartialDecryption {
        di: di.compress().to_bytes(),
    })
}

/// m·G = c2 − Σ d_i
pub fn combine_partial_decryptions(
    ciphertext: &AuditorCiphertext,
    partials: &[PartialDecryption],
) -> Result<RistrettoPoint> {
    let c2 = CompressedRistretto(ciphertext.c2)
        .decompress()
        .ok_or_else(|| anyhow::anyhow!("invalid c2"))?;

    let mut sum = RistrettoPoint::default();
    for p in partials {
        let di = CompressedRistretto(p.di)
            .decompress()
            .ok_or_else(|| anyhow::anyhow!("invalid di"))?;
        sum += di;
    }

    Ok(c2 - sum)
}
