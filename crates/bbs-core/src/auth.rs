//! Bearer credential generation, hashing, and authentication failures.

use sha2::{Digest, Sha256};

pub fn new_token() -> String {
    let bytes: [u8; 16] = rand::random();
    data_encoding::BASE32_NOPAD
        .encode(&bytes)
        .to_ascii_lowercase()
}

pub fn token_hash(token: &str) -> String {
    data_encoding::HEXLOWER.encode(&Sha256::digest(token.trim().to_ascii_lowercase()))
}

pub fn is_strong_token(token: &str) -> bool {
    let normalized = token.trim().to_ascii_uppercase();
    data_encoding::BASE32_NOPAD
        .decode(normalized.as_bytes())
        .is_ok_and(|bytes| bytes.len() >= 16)
}

#[derive(Debug)]
pub struct AuthenticationError(pub &'static str);

impl std::fmt::Display for AuthenticationError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.0)
    }
}

impl std::error::Error for AuthenticationError {}
