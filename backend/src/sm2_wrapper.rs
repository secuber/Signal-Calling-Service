use rand::{CryptoRng, RngCore};
use sm2_integrated::{
    sm2_generate_key, sm2_derive_public_key, sm2_decompress_public_key, sm2_compute_ecdh
};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct PublicKey(pub [u8; 33]);

impl PublicKey {
    pub fn as_bytes(&self) -> &[u8; 33] {
        &self.0
    }
}

impl From<[u8; 33]> for PublicKey {
    fn from(bytes: [u8; 33]) -> Self {
        Self(bytes)
    }
}

impl From<&SecretKey> for PublicKey {
    fn from(secret: &SecretKey) -> Self {
        // Derive uncompressed public key (64 bytes)
        match sm2_derive_public_key(&secret.0) {
            Ok(raw) => compress_public_key(&raw),
            Err(e) => {
                // Should not happen for valid secret key. Panic is acceptable here as From trait cannot fail.
                panic!("Failed to derive public key: {}", e);
            }
        }
    }
}

#[derive(Clone)]
pub struct SecretKey(pub [u8; 32]);

impl SecretKey {
    pub fn random() -> Self {
        let mut rng = rand::thread_rng();
        Self::random_from_rng(&mut rng)
    }

    pub fn random_from_rng<R: RngCore + CryptoRng>(_rng: R) -> Self {
        // Generate new key pair
        // Note: sm2_generate_key likely ignores the rng passed in if it calls OpenSSL internally,
        // but existing API might expect it.
        // The sm2_integrated::sm2_generate_key returns (priv, pub_raw)
        match sm2_generate_key() {
            Ok((sk, _pk)) => SecretKey(sk), 
            Err(e) => panic!("Failed to generate SM2 key: {}", e),
        }
    }

    pub fn diffie_hellman(&self, public: &PublicKey) -> SharedSecret {
        // Decompress peer public key
        let raw_pub = match sm2_decompress_public_key(&public.0) {
            Ok(pt) => pt,
            Err(e) => {
                // If invalid public key, we cannot proceed.
                eprintln!("Error decompressing public key: {}", e);
                return SharedSecret([0u8; 32]);
            }
        };

        match sm2_compute_ecdh(&self.0, &raw_pub) {
            Ok(secret) => SharedSecret(secret),
            Err(e) => {
                eprintln!("Error computing shared secret: {}", e);
                SharedSecret([0u8; 32])
            }
        }
    }
}

pub struct SharedSecret(pub [u8; 32]);

impl SharedSecret {
    pub fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }
}

pub fn compress_public_key(raw: &[u8; 64]) -> PublicKey {
    let mut compressed = [0u8; 33];
    // raw is X || Y.
    // X is raw[0..32]
    // Y is raw[32..64]
    // In Big Endian, the last byte is the LSB.
    let y_last_byte = raw[63];
    compressed[0] = if y_last_byte & 1 == 1 { 0x03 } else { 0x02 };
    compressed[1..33].copy_from_slice(&raw[0..32]);
    PublicKey(compressed)
}
