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
pub const STORAGE_FULL: &str = "storage_full";
pub const HOME_OFFLINE: &str = "home_offline";
pub const QUEUE_FULL: &str = "queue_full";
/// Directory results for a query this installation asked.
pub const RELAY_UNAVAILABLE: &str = "relay_unavailable";
pub const DIRECTORY_TIMEOUT: &str = "directory_timeout";
pub const INVALID_ARGUMENT: &str = "invalid_argument";

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
    if sign::small_order(&c.public_key) {
        return Err(reason(
            INVALID_CREDENTIAL,
            "credential subject key has small order",
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
/// permanent revocations, in Hub's order.
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
    use ed25519_dalek::SigningKey;
    use prost::Message as _;

    fn credential_for(authority: &SigningKey, subject: [u8; 32]) -> mp::SignedRecord {
        sign::sign(
            authority,
            mp::RecordKind::InstallationCredential,
            mp::InstallationCredential {
                namespace: "hub".into(),
                installation_id: "hub-1".into(),
                public_key: subject.to_vec(),
                permissions: vec![mp::InstallationPermission::AssertSenders as i32],
            }
            .encode_to_vec(),
        )
    }

    /// A credential naming a small-order subject is `invalid_credential`,
    /// checked before trust as in Hub; a record signed by such a key is
    /// `invalid_signature`.
    #[test]
    fn small_order_keys_are_refused() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(&dir.path().join("adapter.db"), 0).unwrap();
        let authority = SigningKey::from_bytes(&[1; 32]);
        let good = SigningKey::from_bytes(&[2; 32]).verifying_key().to_bytes();
        // No binding for `hub` yet: the subject check comes first.
        assert_eq!(
            credential(&store, &credential_for(&authority, good))
                .unwrap_err()
                .code,
            NAMESPACE_UNKNOWN
        );
        for key in crate::sign::tests::small_order_keys() {
            let cred = credential_for(&authority, key);
            let err = credential(&store, &cred).unwrap_err();
            assert_eq!(err.code, INVALID_CREDENTIAL, "{err}");
            assert!(err.detail.contains("small order"), "{err}");
            assert_eq!(own_credential(&cred).unwrap_err().code, INVALID_CREDENTIAL);

            let mut signature = [0; 64];
            signature[0] = 1;
            let forged = mp::SignedRecord {
                format_major: sign::FORMAT_MAJOR,
                kind: mp::RecordKind::Message as i32,
                signer_public_key: key.to_vec(),
                payload: b"forged".to_vec(),
                signature: signature.to_vec(),
            };
            let as_subject = Credential {
                namespace: "hub".into(),
                installation_id: "hub-1".into(),
                public_key: key.to_vec(),
                assert_senders: true,
                host_namespace: false,
            };
            let err = subject(&forged, mp::RecordKind::Message, &as_subject).unwrap_err();
            assert_eq!(err.code, INVALID_SIGNATURE, "{err}");
        }
    }

    const AUTHORITY: u8 = 1;
    const INSTALLATION: u8 = 2;

    fn key(b: u8) -> SigningKey {
        SigningKey::from_bytes(&[b; 32])
    }

    /// A store trusting `hub` under authority key 1.
    fn trusting() -> (tempfile::TempDir, Store) {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(&dir.path().join("adapter.db"), 0).unwrap();
        store
            .put_binding(
                &crate::store::Binding {
                    namespace: "hub".into(),
                    authority_key: key(AUTHORITY).verifying_key().to_bytes().to_vec(),
                    revision: 1,
                    disabled: false,
                },
                0,
            )
            .unwrap();
        (dir, store)
    }

    fn cred_with(authority: u8, namespace: &str, subject: u8, perms: &[i32]) -> mp::SignedRecord {
        sign::sign(
            &key(authority),
            mp::RecordKind::InstallationCredential,
            mp::InstallationCredential {
                namespace: namespace.into(),
                installation_id: "hub-1".into(),
                public_key: key(subject).verifying_key().to_bytes().to_vec(),
                permissions: perms.to_vec(),
            }
            .encode_to_vec(),
        )
    }

    const ASSERT: i32 = mp::InstallationPermission::AssertSenders as i32;
    const HOST: i32 = mp::InstallationPermission::HostNamespace as i32;

    fn hub_message() -> mp::Message {
        let address = |ns: &str, id: &str| {
            Some(mp::AddressRef {
                entity: Some(mp::EntityRef {
                    namespace: ns.into(),
                    entity_id: id.into(),
                }),
                address_id: "native".into(),
                run_id: None,
            })
        };
        mp::Message {
            id: Some(mp::MessageRef {
                namespace: "hub".into(),
                message_id: "m1".into(),
            }),
            sender: address("hub", "will"),
            recipient: address("bbs", "will"),
            authored_at_unix_micros: 1,
            expires_at_unix_micros: Some(crate::now_micros() + 60_000_000),
            content_type: "text/plain; charset=utf-8".into(),
            body: b"hi".to_vec(),
            kind: mp::MessageKind::Application as i32,
            ..Default::default()
        }
    }

    fn envelope_of(signer: u8, cred: mp::SignedRecord, m: &mp::Message) -> Vec<u8> {
        mp::SignedEnvelope {
            message: Some(sign::sign(
                &key(signer),
                mp::RecordKind::Message,
                m.encode_to_vec(),
            )),
            credential: Some(cred),
        }
        .encode_to_vec()
    }

    fn code_of<T: std::fmt::Debug>(r: Result<T, Reason>) -> &'static str {
        r.expect_err("refused").code
    }

    /// Each way a credential fails trust gets Hub's reason.
    #[test]
    fn credentials_are_refused_with_hubs_reasons() {
        let (_dir, store) = trusting();
        credential(
            &store,
            &cred_with(AUTHORITY, "hub", INSTALLATION, &[ASSERT, HOST]),
        )
        .unwrap();
        // No binding, or a disabled one: namespace_unknown.
        assert_eq!(
            code_of(credential(
                &store,
                &cred_with(AUTHORITY, "lab", INSTALLATION, &[ASSERT])
            )),
            NAMESPACE_UNKNOWN
        );
        store
            .put_binding(
                &crate::store::Binding {
                    namespace: "off".into(),
                    authority_key: key(AUTHORITY).verifying_key().to_bytes().to_vec(),
                    revision: 1,
                    disabled: true,
                },
                0,
            )
            .unwrap();
        assert_eq!(
            code_of(credential(
                &store,
                &cred_with(AUTHORITY, "off", INSTALLATION, &[ASSERT])
            )),
            NAMESPACE_UNKNOWN
        );
        // Signed by another key than the namespace's authority.
        assert_eq!(
            code_of(credential(
                &store,
                &cred_with(9, "hub", INSTALLATION, &[ASSERT])
            )),
            INVALID_CREDENTIAL
        );
        // A broken signature, a repeated or an unknown permission.
        let mut broken = cred_with(AUTHORITY, "hub", INSTALLATION, &[ASSERT]);
        broken.signature[0] ^= 1;
        assert_eq!(code_of(credential(&store, &broken)), INVALID_CREDENTIAL);
        assert_eq!(
            code_of(credential(
                &store,
                &cred_with(AUTHORITY, "hub", INSTALLATION, &[ASSERT, ASSERT])
            )),
            INVALID_CREDENTIAL
        );
        assert_eq!(
            code_of(credential(
                &store,
                &cred_with(AUTHORITY, "hub", INSTALLATION, &[99])
            )),
            INVALID_CREDENTIAL
        );
        // A revoked subject key: key_revoked.
        let revoked = key(INSTALLATION).verifying_key().to_bytes();
        store
            .put_revocation("hub", &revoked, &mp::SignedRecord::default(), 0)
            .unwrap();
        assert_eq!(
            code_of(credential(
                &store,
                &cred_with(AUTHORITY, "hub", INSTALLATION, &[ASSERT])
            )),
            KEY_REVOKED
        );
    }

    /// An envelope needs its credential to assert senders, its message
    /// signed by the credential's subject, and a sender in the credential's
    /// namespace.
    #[test]
    fn envelopes_are_refused_with_hubs_reasons() {
        let (_dir, store) = trusting();
        let m = hub_message();
        let good = cred_with(AUTHORITY, "hub", INSTALLATION, &[ASSERT]);
        let ok = envelope(&store, &envelope_of(INSTALLATION, good.clone(), &m)).unwrap();
        assert_eq!(ok.bytes, m.encode_to_vec());
        // Missing ASSERT_SENDERS.
        let host_only = cred_with(AUTHORITY, "hub", INSTALLATION, &[HOST]);
        assert_eq!(
            code_of(envelope(&store, &envelope_of(INSTALLATION, host_only, &m))),
            NOT_AUTHORIZED
        );
        // Signed by a key the credential does not name.
        assert_eq!(
            code_of(envelope(&store, &envelope_of(7, good.clone(), &m))),
            INVALID_SIGNATURE
        );
        // A sender outside the credential's namespace.
        let mut foreign = m.clone();
        foreign
            .sender
            .as_mut()
            .unwrap()
            .entity
            .as_mut()
            .unwrap()
            .namespace = "lab".into();
        assert_eq!(
            code_of(envelope(
                &store,
                &envelope_of(INSTALLATION, good.clone(), &foreign)
            )),
            NOT_AUTHORIZED
        );
        // Trust failures surface as the credential's.
        let stranger = cred_with(AUTHORITY, "lab", INSTALLATION, &[ASSERT]);
        assert_eq!(
            code_of(envelope(&store, &envelope_of(INSTALLATION, stranger, &m))),
            NAMESPACE_UNKNOWN
        );
        store
            .put_revocation(
                "hub",
                &key(INSTALLATION).verifying_key().to_bytes(),
                &mp::SignedRecord::default(),
                0,
            )
            .unwrap();
        assert_eq!(
            code_of(envelope(&store, &envelope_of(INSTALLATION, good, &m))),
            KEY_REVOKED
        );
    }

    /// A revocation must be signed by its namespace's trusted authority.
    #[test]
    fn revocations_need_the_namespace_authority() {
        let (_dir, store) = trusting();
        let revoke = |signer: u8, namespace: &str, public_key: Vec<u8>| {
            sign::sign(
                &key(signer),
                mp::RecordKind::InstallationRevocation,
                mp::InstallationRevocation {
                    namespace: namespace.into(),
                    public_key,
                    ..Default::default()
                }
                .encode_to_vec(),
            )
        };
        let subject = key(INSTALLATION).verifying_key().to_bytes().to_vec();
        let r = revocation(&store, &revoke(AUTHORITY, "hub", subject.clone())).unwrap();
        assert_eq!(
            (r.namespace.as_str(), r.public_key.clone()),
            ("hub", subject.clone())
        );
        // Another signer, an untrusted namespace, a bad key, a broken
        // signature, or another record kind.
        assert_eq!(
            code_of(revocation(&store, &revoke(9, "hub", subject.clone()))),
            INVALID_SIGNATURE
        );
        assert_eq!(
            code_of(revocation(
                &store,
                &revoke(AUTHORITY, "lab", subject.clone())
            )),
            INVALID_SIGNATURE
        );
        assert_eq!(
            code_of(revocation(&store, &revoke(AUTHORITY, "hub", vec![1; 31]))),
            INVALID_CREDENTIAL
        );
        let mut broken = revoke(AUTHORITY, "hub", subject);
        broken.signature[0] ^= 1;
        assert_eq!(code_of(revocation(&store, &broken)), INVALID_SIGNATURE);
        let cred = cred_with(AUTHORITY, "hub", INSTALLATION, &[ASSERT]);
        assert_eq!(code_of(revocation(&store, &cred)), INVALID_SIGNATURE);
    }

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
