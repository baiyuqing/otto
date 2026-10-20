//! Lowercase hex encoding for digests.

/// `bytes` as lowercase hex, two characters per byte.
pub(crate) fn to_hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::to_hex;
    use sha2::{Digest, Sha256};

    #[test]
    fn sha256_digest_is_lowercase_hex() {
        assert_eq!(
            to_hex(&Sha256::digest(b"abc")),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }
}
