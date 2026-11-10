//! What every hop checks before admitting federation traffic, in Hub's
//! order (`internal/fed/transfer.go` and `trust.go`): strict bounded
//! decoding, the credential against the namespace's trusted binding and its
//! revocations, the record signature by the credential's subject, the
//! permission it needs, and the structural message rules (`checkMessage`).
//! Failures carry msgproto transfer reasons (D108).

use bbs_core::hub_proto::msgproto_v1 as mp;

use crate::sign::{self, VerifyError};
use crate::store::Store;
use crate::wire::{self, DecodeError, Schema};

/// msgproto reasons used in transfer, route and directory results.
pub const NAMESPACE_UNKNOWN: &str = "namespace_unknown";
pub const INVALID_CREDENTIAL: &str = "invalid_credential";
pub const INVALID_SIGNATURE: &str = "invalid_signature";
pub const KEY_REVOKED: &str = "key_revoked";
pub const NOT_AUTHORIZED: &str = "route_not_authorized";
pub const INVALID_MESSAGE: &str = "invalid_message";
pub const FRAME_TOO_LARGE: &str = "frame_too_large";
pub const MESSAGE_CONFLICT: &str = "message_conflict";
pub const EXPIRED: &str = "expired";
pub const UNSUPPORTED_CONTENT: &str = "unsupported_content";
pub const STORAGE_UNAVAILABLE: &str = "storage_unavailable";
pub const HOME_OFFLINE: &str = "home_offline";
pub const QUEUE_FULL: &str = "queue_full";

/// A refusal with its msgproto reason and a diagnostic detail.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Reason {
    pub code: &'static str,
    pub detail: String,
}

impl std::fmt::Display for Reason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.code, self.detail)
    }
}

impl std::error::Error for Reason {}

pub fn reason(code: &'static str, detail: impl Into<String>) -> Reason {
    Reason {
        code,
        detail: detail.into(),
    }
}

fn storage(err: anyhow::Error) -> Reason {
    tracing::error!("adapter store: {err:#}");
    reason(STORAGE_UNAVAILABLE, "the adapter could not use its store")
}

/// Bound and strictly decode untrusted bytes: `frame_too_large` over the
/// limit, else `invalid_message`.
pub fn decode<M: Schema>(b: &[u8], max: usize) -> Result<M, Reason> {
    wire::decode(b, max).map_err(|e| match e {
        DecodeError::TooLarge { .. } => reason(FRAME_TOO_LARGE, e.to_string()),
        DecodeError::Invalid(_) => reason(INVALID_MESSAGE, e.to_string()),
    })
}

/// An opaque ID: `[A-Za-z0-9_-]{1,128}`.
pub fn valid_id(s: &str) -> bool {
    (1..=128).contains(&s.len())
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

/// A namespace: `[a-z][a-z0-9-]{0,62}`.
pub fn valid_namespace(s: &str) -> bool {
    let b = s.as_bytes();
    (1..=63).contains(&b.len())
        && b[0].is_ascii_lowercase()
        && b.iter()
            .all(|&c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == b'-')
}

/// An entity path as Hub's federation accepts it (`config.ValidEntityID`):
/// segments of `[A-Za-z0-9_-]` joined by `/`, at most 128 characters. Depth
/// limits are each home's own policy; the wire treats IDs as opaque.
pub fn valid_entity_id(s: &str) -> bool {
    s.len() <= 128
        && s.split('/').all(|part| {
            !part.is_empty()
                && part
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
        })
}

fn valid_address(a: Option<&mp::AddressRef>) -> bool {
    a.is_some_and(|a| {
        a.entity
            .as_ref()
            .is_some_and(|e| valid_namespace(&e.namespace) && valid_entity_id(&e.entity_id))
            && valid_id(&a.address_id)
            && a.run_id.as_deref().is_none_or(valid_id)
    })
}

/// The structural rules every hop applies to a message (Hub's
/// `checkMessage`), in its order.
pub fn check_message(m: &mp::Message) -> Result<(), Reason> {
    let id_ok =
        m.id.as_ref()
            .is_some_and(|id| valid_namespace(&id.namespace) && valid_id(&id.message_id));
    if !id_ok {
        return Err(reason(INVALID_MESSAGE, "message ID is incomplete"));
    }
    if !valid_address(m.sender.as_ref()) || !valid_address(m.recipient.as_ref()) {
        return Err(reason(
            INVALID_MESSAGE,
            "sender and recipient must be complete addresses",
        ));
    }
    if m.kind != mp::MessageKind::Application as i32 || m.delivery_error.is_some() {
        return Err(reason(
            UNSUPPORTED_CONTENT,
            "only APPLICATION messages are accepted",
        ));
    }
    if m.authored_at_unix_micros <= 0 || m.expires_at_unix_micros.is_none() {
        return Err(reason(
            INVALID_MESSAGE,
            "authored time and delivery deadline are required",
        ));
    }
    if m.body.len() > 65536 {
        return Err(reason(FRAME_TOO_LARGE, "body exceeds 65536 bytes"));
    }
    if m.content_type.is_empty() || m.content_type.len() > 128 {
        return Err(reason(INVALID_MESSAGE, "invalid content type"));
    }
    if let Some(r) = &m.in_reply_to
        && !(valid_namespace(&r.namespace) && valid_id(&r.message_id))
    {
        return Err(reason(INVALID_MESSAGE, "invalid in_reply_to"));
    }
    Ok(())
}

/// A credential checked against current trust.
#[derive(Debug, Clone)]
pub struct Credential {
    pub namespace: String,
    pub installation_id: String,
    pub public_key: Vec<u8>,
    pub assert_senders: bool,
    pub host_namespace: bool,
}

/// Decode a credential record's payload, without checking trust: Hub's
/// field checks. Its permissions are still to be checked
/// ([`permissions`]).
fn credential_fields(rec: &mp::SignedRecord) -> Result<(Credential, Vec<i32>), Reason> {
    let c: mp::InstallationCredential = decode(&rec.payload, wire::MAX_CREDENTIAL)?;
    if !valid_namespace(&c.namespace) || !valid_id(&c.installation_id) || c.public_key.len() != 32 {
        return Err(reason(
            INVALID_CREDENTIAL,
            "credential fields are incomplete",
        ));
    }
    let out = Credential {
        namespace: c.namespace,
        installation_id: c.installation_id,
        public_key: c.public_key,
        assert_senders: false,
        host_namespace: false,
    };
    Ok((out, c.permissions))
}

/// Apply a credential's permissions: each known and listed once.
fn permissions(c: &mut Credential, perms: &[i32]) -> Result<(), Reason> {
    let mut seen = Vec::new();
    for &p in perms {
        if seen.contains(&p) {
            return Err(reason(
                INVALID_CREDENTIAL,
                format!("duplicate permission {p}"),
            ));
        }
        seen.push(p);
        match mp::InstallationPermission::try_from(p) {
            Ok(mp::InstallationPermission::AssertSenders) => c.assert_senders = true,
            Ok(mp::InstallationPermission::HostNamespace) => c.host_namespace = true,
            _ => {
                return Err(reason(
                    INVALID_CREDENTIAL,
                    format!("unknown permission {p}"),
                ));
            }
        }
    }
    Ok(())
}

/// Read this installation's own credential, checking its signature and
/// fields but not trust (the relay checks that).
pub fn own_credential(rec: &mp::SignedRecord) -> Result<Credential, Reason> {
    sign::verify(rec, mp::RecordKind::InstallationCredential)
        .map_err(|e| reason(INVALID_CREDENTIAL, e.to_string()))?;
    let (mut c, perms) = credential_fields(rec)?;
    permissions(&mut c, &perms)?;
    Ok(c)
}

/// Check a credential record against its namespace's trusted binding and
/// permanent revocations, and cache it.
pub fn credential(store: &Store, rec: &mp::SignedRecord) -> Result<Credential, Reason> {
    sign::verify(rec, mp::RecordKind::InstallationCredential)
        .map_err(|e| reason(INVALID_CREDENTIAL, e.to_string()))?;
    let (mut c, perms) = credential_fields(rec)?;
    let binding = store.binding(&c.namespace).map_err(storage)?;
    let Some(binding) = binding.filter(|b| !b.disabled) else {
        return Err(reason(
            NAMESPACE_UNKNOWN,
            format!("no trusted binding for namespace {}", c.namespace),
        ));
    };
    if rec.signer_public_key != binding.authority_key {
        return Err(reason(
            INVALID_CREDENTIAL,
            format!("credential not signed by the {} authority", c.namespace),
        ));
    }
    permissions(&mut c, &perms)?;
    if store
        .revoked(&c.namespace, &c.public_key)
        .map_err(storage)?
    {
        return Err(reason(
            KEY_REVOKED,
            format!("key revoked in namespace {}", c.namespace),
        ));
    }
    store.put_credential(rec, &c).map_err(storage)?;
    Ok(c)
}

/// Check a record signed by the credential's subject key.
pub fn subject(
    rec: &mp::SignedRecord,
    kind: mp::RecordKind,
    cred: &Credential,
) -> Result<(), Reason> {
    sign::verify(rec, kind).map_err(|e: VerifyError| reason(INVALID_SIGNATURE, e.to_string()))?;
    if rec.signer_public_key != cred.public_key {
        return Err(reason(
            INVALID_SIGNATURE,
            "record signer is not the credential subject",
        ));
    }
    Ok(())
}

/// A verified envelope: its message and the exact signed message bytes.
#[derive(Debug)]
pub struct Verified {
    pub message: mp::Message,
    pub bytes: Vec<u8>,
}

/// Validate a received envelope from its exact bytes: bounds, strict
/// structure, the credential against trust, the message signature by the
/// credential's subject, ASSERT_SENDERS, the message rules and namespace
/// agreement.
pub fn envelope(store: &Store, raw: &[u8]) -> Result<Verified, Reason> {
    let env: mp::SignedEnvelope = decode(raw, wire::MAX_ENVELOPE)?;
    let (Some(record), Some(cred)) = (&env.message, &env.credential) else {
        return Err(reason(
            INVALID_MESSAGE,
            "envelope needs a message and a credential",
        ));
    };
    let cred = credential(store, cred)?;
    subject(record, mp::RecordKind::Message, &cred)?;
    if !cred.assert_senders {
        return Err(reason(NOT_AUTHORIZED, "credential lacks ASSERT_SENDERS"));
    }
    let message: mp::Message = decode(&record.payload, wire::MAX_MESSAGE)?;
    check_message(&message)?;
    let id = message.id.as_ref().expect("checked");
    let sender = message.sender.as_ref().and_then(|s| s.entity.as_ref());
    let sender_ns = sender.map(|e| e.namespace.as_str()).unwrap_or_default();
    if id.namespace != cred.namespace || sender_ns != cred.namespace {
        return Err(reason(
            NOT_AUTHORIZED,
            format!(
                "credential for {} cannot assert sender {sender_ns}",
                cred.namespace
            ),
        ));
    }
    Ok(Verified {
        message,
        bytes: env.message.map(|m| m.payload).unwrap_or_default(),
    })
}

/// Check a namespace-authority-signed revocation.
pub fn revocation(
    store: &Store,
    rec: &mp::SignedRecord,
) -> Result<mp::InstallationRevocation, Reason> {
    sign::verify(rec, mp::RecordKind::InstallationRevocation)
        .map_err(|e| reason(INVALID_SIGNATURE, e.to_string()))?;
    let r: mp::InstallationRevocation = decode(&rec.payload, wire::MAX_CREDENTIAL)?;
    if r.public_key.len() != 32 {
        return Err(reason(
            INVALID_CREDENTIAL,
            "revocation key must be 32 bytes",
        ));
    }
    let binding = store.binding(&r.namespace).map_err(storage)?;
    if binding.is_none_or(|b| b.authority_key != rec.signer_public_key) {
        return Err(reason(
            INVALID_SIGNATURE,
            format!("revocation not signed by the {} authority", r.namespace),
        ));
    }
    Ok(r)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_follow_hub() {
        assert!(valid_id("a-B_9"));
        assert!(!valid_id(""));
        assert!(!valid_id("a/b"));
        assert!(!valid_id(&"x".repeat(129)));
        assert!(valid_namespace("bbs"));
        assert!(valid_namespace("a-1"));
        assert!(!valid_namespace("1a"));
        assert!(!valid_namespace("Bbs"));
        assert!(!valid_namespace(&"a".repeat(64)));
        assert!(valid_entity_id("will"));
        assert!(valid_entity_id("will/scout"));
        assert!(valid_entity_id("a/b/c"));
        assert!(!valid_entity_id("will/"));
        assert!(!valid_entity_id("/will"));
        assert!(!valid_entity_id(&"x".repeat(129)));
    }
}
