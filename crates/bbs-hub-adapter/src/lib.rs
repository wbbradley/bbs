//! A separate process that federates a BBS installation with Hub (D085): a
//! client of BBS's integration stream on one side and of a Hub relay's
//! federation stream on the other, with its own SQLite store for Hub
//! credentials, trust, the relay session, exact signed envelopes and
//! known-unsent work. Every handoff is at most once (D072).

pub mod config;
pub mod keys;
pub mod sign;
pub mod store;
pub mod verify;
pub mod wire;

/// Now, in Unix microseconds.
pub fn now_micros() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_micros() as i64)
        .unwrap_or_default()
}

/// A generated ID as Hub makes them (D116): 16 random bytes as exactly 22
/// base58 characters (Bitcoin alphabet, left-padded with `1`).
pub fn new_id() -> String {
    const ALPHABET: &[u8; 58] = b"123456789ABCDEFGHJKLMNPQRSTUVWXYZabcdefghijkmnopqrstuvwxyz";
    let mut n: u128 = rand::random();
    let mut out = [b'1'; 22];
    for slot in out.iter_mut().rev() {
        *slot = ALPHABET[(n % 58) as usize];
        n /= 58;
    }
    String::from_utf8(out.to_vec()).expect("ASCII")
}

/// This build, as hellos report it.
pub fn build() -> String {
    format!("bbs-hub-adapter {}", bbs_core::release::VERSION)
}

#[cfg(test)]
mod tests {
    #[test]
    fn generated_ids_are_22_base58_characters() {
        for _ in 0..100 {
            let id = super::new_id();
            assert_eq!(id.len(), 22);
            assert!(super::verify::valid_id(&id));
            assert!(!id.contains(['0', 'O', 'I', 'l']));
        }
    }
}
