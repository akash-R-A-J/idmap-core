use serde::{Deserialize, Serialize};

/// Ciphertext exactly as produced by Token-2022 ElGamal
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AuditorCiphertext {
    pub c1: [u8; 32], // compressed Ristretto
    pub c2: [u8; 32],
}

/// Partial decryption share d_i = x_i · c1
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PartialDecryption {
    pub di: [u8; 32], // compressed Ristretto
}
