//! REALITY keypair generation.
//!
//! Xray's REALITY transport uses an X25519 keypair. The private key never
//! leaves the panel (it lives in SQLite); the public key is what goes into a
//! client's share link, so it is safe to display.
//!
//! Xray expects both halves base64url-encoded *without* padding, over the
//! 32 raw bytes — not the 48-byte `to_bytes()` form some docs show.

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use rand::rngs::OsRng;
use x25519_dalek::{PublicKey, StaticSecret};

use crate::error::AppResult;

#[derive(Clone)]
pub struct RealityKeypair {
    pub private_key: String,
    pub public_key: String,
}

/// Generate a fresh X25519 keypair for a REALITY inbound.
pub fn generate_keypair() -> AppResult<RealityKeypair> {
    let secret = StaticSecret::random_from_rng(OsRng);
    let public = PublicKey::from(&secret);
    Ok(RealityKeypair {
        private_key: URL_SAFE_NO_PAD.encode(secret.to_bytes()),
        public_key: URL_SAFE_NO_PAD.encode(public.as_bytes()),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generates_base64url_32_byte_keys() {
        let kp = generate_keypair().unwrap();
        // 32 bytes base64url-unpadded is always 43 characters.
        assert_eq!(kp.private_key.len(), 43, "{}", kp.private_key);
        assert_eq!(kp.public_key.len(), 43, "{}", kp.public_key);
        assert!(!kp.private_key.contains('='));
        assert!(!kp.private_key.contains('+'));
        assert!(!kp.private_key.contains('/'));
    }

    #[test]
    fn public_key_derives_from_private_key() {
        let kp = generate_keypair().unwrap();
        let secret_bytes: [u8; 32] = URL_SAFE_NO_PAD
            .decode(&kp.private_key)
            .unwrap()
            .try_into()
            .unwrap();
        let secret = StaticSecret::from(secret_bytes);
        let derived = PublicKey::from(&secret);
        assert_eq!(URL_SAFE_NO_PAD.encode(derived.as_bytes()), kp.public_key);
    }

    #[test]
    fn keypairs_are_distinct_across_calls() {
        let a = generate_keypair().unwrap();
        let b = generate_keypair().unwrap();
        assert_ne!(a.private_key, b.private_key);
        assert_ne!(a.public_key, b.public_key);
    }
}
