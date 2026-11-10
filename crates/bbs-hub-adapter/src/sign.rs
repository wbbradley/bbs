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
    Signature,
}

impl std::fmt::Display for VerifyError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            VerifyError::Kind => "unexpected or unknown record kind",
            VerifyError::Major => "unsupported record format_major",
            VerifyError::KeyLength => "signer public key must be 32 bytes",
            VerifyError::SignatureLength => "signature must be 64 bytes",
            VerifyError::Signature => "invalid signature",
        })
    }
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
    let message = input(label, r.format_major, &r.signer_public_key, &r.payload);
    // dalek's verify, not verify_strict: it accepts and refuses what Go's
    // ed25519.Verify does.
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
mod tests {
    use super::*;

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
