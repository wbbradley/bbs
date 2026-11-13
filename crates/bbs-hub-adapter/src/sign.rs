//! msgproto exact-byte signing, as Hub defines it (CONTRACT.md "Signed bytes
//! and namespace trust"): plain Ed25519 over
//!
//! ```text
//! "msgproto/v1/" || kind_label || 0x00 || U32BE(format_major)
//! || signer_public_key[32] || U64BE(len(payload)) || payload
//! ```
//!
//! The record hash is SHA-256 of that same input. The input is always built
//! from the retained payload bytes, never by re-encoding parsed protobuf.

use bbs_core::hub_proto::msgproto_v1 as mp;
use ed25519_dalek::{Signer as _, SigningKey, Verifier as _, VerifyingKey};
use sha2::{Digest, Sha256};

/// The only defined record format.
pub const FORMAT_MAJOR: u32 = 1;

/// A record kind's domain-separation label; None for unspecified or unknown
/// kinds, which are never signed or accepted.
pub fn label(kind: i32) -> Option<&'static str> {
    use mp::RecordKind as K;
    Some(match K::try_from(kind).ok()? {
        K::InstallationCredential => "installation-credential",
        K::InstallationRevocation => "installation-revocation",
        K::Message => "message",
        K::RouteClaim => "route-claim",
        K::DirectoryQuery => "directory-query",
        K::DirectoryAnswer => "directory-answer",
        K::RoomRequest => "room-request",
        K::RoomAnswer => "room-answer",
        K::Unspecified => return None,
    })
}

/// The exact signing input for a record.
pub fn input(label: &str, major: u32, signer: &[u8], payload: &[u8]) -> Vec<u8> {
    let mut b = Vec::with_capacity(12 + label.len() + 1 + 4 + signer.len() + 8 + payload.len());
    b.extend_from_slice(b"msgproto/v1/");
    b.extend_from_slice(label.as_bytes());
    b.push(0);
    b.extend_from_slice(&major.to_be_bytes());
    b.extend_from_slice(signer);
    b.extend_from_slice(&(payload.len() as u64).to_be_bytes());
    b.extend_from_slice(payload);
    b
}

/// Sign `payload` as a record of `kind` with `key`.
pub fn sign(key: &SigningKey, kind: mp::RecordKind, payload: Vec<u8>) -> mp::SignedRecord {
    let label = label(kind as i32).expect("signing a defined record kind");
    let public = key.verifying_key().to_bytes();
    let signature = key.sign(&input(label, FORMAT_MAJOR, &public, &payload));
    mp::SignedRecord {
        kind: kind as i32,
        format_major: FORMAT_MAJOR,
        payload,
        signer_public_key: public.to_vec(),
        signature: signature.to_bytes().to_vec(),
    }
}

/// Why a record's framing or signature failed; Hub's `sign.Verify` errors.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VerifyError {
    Kind,
    Major,
    KeyLength,
    SignatureLength,
    /// The signer key is a small-order point (see [`small_order`]).
    SmallKey,
    Signature,
}

impl std::fmt::Display for VerifyError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            VerifyError::Kind => "unexpected or unknown record kind",
            VerifyError::Major => "unsupported record format_major",
            VerifyError::KeyLength => "signer public key must be 32 bytes",
            VerifyError::SignatureLength => "signature must be 64 bytes",
            VerifyError::SmallKey => "signer public key has small order",
            VerifyError::Signature => "invalid signature",
        })
    }
}

/// Whether `key` encodes a point of small order (8-torsion). Under such a
/// key, R = identity and S = 0 verify for any message, and plain Ed25519
/// verification does not refuse that, so Hub refuses the key itself, as a
/// signer and as a credential's subject. A key that is not a point at all
/// is not small order (it fails verification instead), as in Hub's
/// `sign.SmallOrder`.
pub fn small_order(key: &[u8]) -> bool {
    let Ok(key) = <&[u8; 32]>::try_from(key) else {
        return false;
    };
    // is_weak decompresses the point and asks is_small_order().
    VerifyingKey::from_bytes(key).is_ok_and(|k| k.is_weak())
}

/// Whether `key` is a usable Ed25519 public key: 32 bytes, a point, and not
/// of small order.
pub fn usable_key(key: &[u8]) -> bool {
    <&[u8; 32]>::try_from(key)
        .ok()
        .and_then(|k| VerifyingKey::from_bytes(k).ok())
        .is_some_and(|k| !k.is_weak())
}

/// Check a record's framing and signature and that it has the expected
/// kind, in Hub's order. It does not decide whether the signer is
/// authorized.
pub fn verify(r: &mp::SignedRecord, want: mp::RecordKind) -> Result<(), VerifyError> {
    let Some(label) = label(r.kind).filter(|_| r.kind == want as i32) else {
        return Err(VerifyError::Kind);
    };
    if r.format_major != FORMAT_MAJOR {
        return Err(VerifyError::Major);
    }
    let key: &[u8; 32] = r
        .signer_public_key
        .as_slice()
        .try_into()
        .map_err(|_| VerifyError::KeyLength)?;
    let signature: &[u8; 64] = r
        .signature
        .as_slice()
        .try_into()
        .map_err(|_| VerifyError::SignatureLength)?;
    // A key that is not a valid point fails like a bad signature, as Go's
    // ed25519.Verify returns false for it.
    let key = VerifyingKey::from_bytes(key).map_err(|_| VerifyError::Signature)?;
    // Refused before verification, as Hub does: plain Ed25519 accepts a
    // forgery under a small-order key.
    if key.is_weak() {
        return Err(VerifyError::SmallKey);
    }
    let message = input(label, r.format_major, &r.signer_public_key, &r.payload);
    // dalek's plain verify, not verify_strict, for parity with Go's
    // ed25519.Verify (RFC 8032, cofactorless; S must be reduced). Its one
    // gap, small-order keys, is closed explicitly above.
    key.verify(&message, &ed25519_dalek::Signature::from_bytes(signature))
        .map_err(|_| VerifyError::Signature)
}

/// The record hash: SHA-256 of the signing input. None for an unknown kind.
pub fn hash(r: &mp::SignedRecord) -> Option<[u8; 32]> {
    let label = label(r.kind)?;
    Some(
        Sha256::digest(input(
            label,
            r.format_major,
            &r.signer_public_key,
            &r.payload,
        ))
        .into(),
    )
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    fn unhex(s: &str) -> [u8; 32] {
        let b: Vec<u8> = (0..s.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap())
            .collect();
        b.try_into().unwrap()
    }

    /// Small-order points: the identity (order 1), (0, -1) (order 2) and a
    /// point of order 8.
    pub(crate) fn small_order_keys() -> [[u8; 32]; 3] {
        let mut identity = [0; 32];
        identity[0] = 1;
        [
            identity,
            unhex("ecffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff7f"),
            unhex("c7176a703d4dd84fba3c0b760d10670f2a2053fa2c39ccc64ec7fd7792ac037a"),
        ]
    }

    #[test]
    fn small_order_keys_are_refused_as_signers() {
        let normal = SigningKey::from_bytes(&[7; 32]).verifying_key().to_bytes();
        assert!(!small_order(&normal));
        assert!(!small_order(&[0; 31]), "not 32 bytes");
        // Not a point: verification fails it, as in Hub.
        let not_a_point = unhex("0200000000000000000000000000000000000000000000000000000000000000");
        assert!(VerifyingKey::from_bytes(&not_a_point).is_err());
        assert!(!small_order(&not_a_point));
        for key in small_order_keys() {
            assert!(small_order(&key), "{key:02x?}");
            // The universal forgery: R = identity, S = 0, for any message.
            let mut signature = [0; 64];
            signature[0] = 1;
            let rec = mp::SignedRecord {
                format_major: FORMAT_MAJOR,
                kind: mp::RecordKind::Message as i32,
                signer_public_key: key.to_vec(),
                payload: b"anything at all".to_vec(),
                signature: signature.to_vec(),
            };
            // Plain verification accepts it; only the explicit check refuses.
            let input = input("message", FORMAT_MAJOR, &key, &rec.payload);
            assert!(
                VerifyingKey::from_bytes(&key)
                    .unwrap()
                    .verify(&input, &ed25519_dalek::Signature::from_bytes(&signature))
                    .is_ok(),
                "{key:02x?}"
            );
            assert_eq!(
                verify(&rec, mp::RecordKind::Message),
                Err(VerifyError::SmallKey)
            );
        }
    }

    #[test]
    fn signs_and_verifies_and_rejects_changes() {
        let key = SigningKey::from_bytes(&[7; 32]);
        let rec = sign(&key, mp::RecordKind::Message, b"payload".to_vec());
        assert_eq!(verify(&rec, mp::RecordKind::Message), Ok(()));
        assert_eq!(
            verify(&rec, mp::RecordKind::RouteClaim),
            Err(VerifyError::Kind)
        );
        let mut bad = rec.clone();
        bad.payload[0] ^= 1;
        assert_eq!(
            verify(&bad, mp::RecordKind::Message),
            Err(VerifyError::Signature)
        );
        let mut bad = rec.clone();
        bad.format_major = 2;
        assert_eq!(
            verify(&bad, mp::RecordKind::Message),
            Err(VerifyError::Major)
        );
        let mut bad = rec.clone();
        bad.signer_public_key.pop();
        assert_eq!(
            verify(&bad, mp::RecordKind::Message),
            Err(VerifyError::KeyLength)
        );
        let mut bad = rec.clone();
        bad.signature.push(0);
        assert_eq!(
            verify(&bad, mp::RecordKind::Message),
            Err(VerifyError::SignatureLength)
        );
        let mut bad = rec;
        bad.kind = mp::RecordKind::Unspecified as i32;
        assert_eq!(
            verify(&bad, mp::RecordKind::Unspecified),
            Err(VerifyError::Kind)
        );
    }
}
