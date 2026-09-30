//! UUID v4 generation.
//!
//! Random 128 bits with the version (4) and variant (RFC 4122) bits set per
//! RFC 4122 §4.4. We use `rand`'s OS RNG so generated IDs are unpredictable
//! to anyone without kernel `/dev/urandom` access.

use rand::RngCore;

use crate::error::{AppError, AppResult};

pub const UUID_BYTES: usize = 16;

/// Generate a random UUID v4 in the standard `8-4-4-4-12` lowercase hex form.
pub fn v4() -> AppResult<String> {
    let mut bytes = [0u8; UUID_BYTES];
    rand::rngs::OsRng
        .try_fill_bytes(&mut bytes)
        .map_err(|e| AppError::Crypto(format!("OS RNG failed while generating UUID: {e}")))?;
    // version 4 (random)
    bytes[6] = (bytes[6] & 0x0f) | 0x40;
    // variant RFC 4122
    bytes[8] = (bytes[8] & 0x3f) | 0x80;

    let hex = bytes_to_lower_hex(&bytes);
    Ok(format!(
        "{}-{}-{}-{}-{}",
        &hex[0..8],
        &hex[8..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..32]
    ))
}

fn bytes_to_lower_hex(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        out.push(HEX[(b >> 4) as usize] as char);
        out.push(HEX[(b & 0x0f) as usize] as char);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn has_canonical_shape_and_version_bits() {
        for _ in 0..16 {
            let id = v4().unwrap();
            assert_eq!(id.len(), 36);
            assert_eq!(id.chars().filter(|c| *c == '-').count(), 4);
            let chars: Vec<char> = id.chars().collect();
            assert_eq!(chars[8], '-');
            assert_eq!(chars[13], '-');
            assert_eq!(chars[18], '-');
            assert_eq!(chars[23], '-');

            // version nibble: position 14 (the first hex char of group 3)
            assert_eq!(chars[14], '4', "version nibble in {id}");
            // variant nibble: position 19 must be 8/9/a/b
            let variant = chars[19];
            assert!(
                matches!(variant, '8' | '9' | 'a' | 'b'),
                "variant nibble in {id}"
            );
        }
    }

    #[test]
    fn ids_are_distinct() {
        let a = v4().unwrap();
        let b = v4().unwrap();
        let c = v4().unwrap();
        assert_ne!(a, b);
        assert_ne!(b, c);
        assert_ne!(a, c);
    }
}
