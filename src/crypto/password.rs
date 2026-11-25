//! Password hashing.
//!
//! bcrypt at cost 12, per the plan's KTD9. Cost 12 is roughly 250ms on
//! commodity hardware — slow enough to make offline cracking expensive, fast
//! enough for an interactive login on the low-power boards this panel targets.

use crate::error::{AppError, AppResult};

/// bcrypt work factor. Bumping this invalidates nothing — `verify` reads the
/// cost out of the stored hash — so it can be raised later without a migration.
///
/// In debug / test builds, cost 4 (the bcrypt minimum) runs ~256x faster (~1ms),
/// keeping the test suite fast. Release builds use cost 12 (~250ms).
#[cfg(debug_assertions)]
pub const BCRYPT_COST: u32 = 4;
#[cfg(not(debug_assertions))]
pub const BCRYPT_COST: u32 = 12;

pub fn hash(password: &str) -> AppResult<String> {
    bcrypt::hash(password, BCRYPT_COST)
        .map_err(|e| AppError::Crypto(format!("bcrypt hash failed: {e}")))
}

pub fn verify(password: &str, hash: &str) -> bool {
    bcrypt::verify(password, hash).unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hash_and_verify_round_trip() {
        let h = hash("correct horse battery staple").unwrap();
        assert!(verify("correct horse battery staple", &h));
        assert!(!verify("wrong password", &h));
    }

    #[test]
    fn hash_is_salted() {
        let a = hash("same").unwrap();
        let b = hash("same").unwrap();
        assert_ne!(a, b, "bcrypt must salt each hash");
        assert!(verify("same", &a));
        assert!(verify("same", &b));
    }

    #[test]
    fn hash_records_the_configured_cost() {
        let h = hash("x").unwrap();
        // bcrypt hashes are "$2b$<cost>$<salt+digest>".
        let cost: u32 = h
            .split('$')
            .nth(2)
            .expect("cost field")
            .parse()
            .expect("cost parses");
        assert_eq!(cost, BCRYPT_COST);
    }

    #[test]
    fn verify_rejects_malformed_hash_without_panicking() {
        assert!(!verify("x", "not-a-bcrypt-hash"));
        assert!(!verify("x", ""));
    }
}
