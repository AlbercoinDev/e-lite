//! Bitcoin's double SHA-256.

use sha2::{Digest, Sha256};

/// `SHA256(SHA256(parts concatenated))`, in internal (wire) byte order.
pub fn sha256d(parts: &[&[u8]]) -> [u8; 32] {
    let mut h = Sha256::new();
    for part in parts {
        h.update(part);
    }
    let first = h.finalize();
    let mut out = [0u8; 32];
    out.copy_from_slice(&Sha256::digest(first));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matches_the_known_empty_digest_and_ignores_the_split() {
        // SHA256d("") = 5df6e0e2761359d30a8275058e299fcc0381534545f55cf43e41983f5d4c9456
        let empty = sha256d(&[]);
        assert_eq!(
            empty[..4],
            [0x5d, 0xf6, 0xe0, 0xe2],
            "double SHA-256 of the empty input"
        );
        assert_eq!(sha256d(&[b"ab", b"c"]), sha256d(&[b"abc"]));
    }
}
