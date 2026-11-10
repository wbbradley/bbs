//! Hub's Go/Rust vectors, vendored in `tests/data/hub` (see its README for
//! the source commit): every signing vector and every federation interop
//! vector must pass here exactly as Hub's own tests check them
//! (`scripts/signing_vectors.go`, `internal/fed/interop_test.go`).

use std::path::PathBuf;

use bbs_core::hub_proto::msgproto_v1 as mp;
use bbs_hub_adapter::store::{Binding, Store};
use bbs_hub_adapter::{sign, verify, wire};
use ed25519_dalek::SigningKey;
use prost::Message as _;
use prost_reflect::DynamicMessage;
use serde::Deserialize;

fn data(name: &str) -> Vec<u8> {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/data/hub")
        .join(name);
    std::fs::read(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()))
}

fn hex(s: &str) -> Vec<u8> {
    data_encoding::HEXLOWER
        .decode(s.as_bytes())
        .unwrap_or_else(|e| panic!("bad hex {s:?}: {e}"))
}

fn to_hex(b: &[u8]) -> String {
    data_encoding::HEXLOWER.encode(b)
}

fn key(seed: &[u8]) -> SigningKey {
    SigningKey::from_bytes(seed.try_into().expect("a 32-byte seed"))
}

// ---- signing vectors -------------------------------------------------------

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SigningVector {
    kind: String,
    test_seed_hex: String,
    payload_hex: String,
    public_key_hex: String,
    signing_input_hex: String,
    record_hash_hex: String,
    signature_hex: String,
}

fn record_kind(label: &str) -> mp::RecordKind {
    use mp::RecordKind as K;
    [
        K::InstallationCredential,
        K::InstallationRevocation,
        K::Message,
        K::RouteClaim,
        K::DirectoryQuery,
        K::DirectoryAnswer,
    ]
    .into_iter()
    .find(|k| sign::label(*k as i32) == Some(label))
    .unwrap_or_else(|| panic!("unknown vector kind {label}"))
}

/// The payload decodes strictly as its kind's message.
fn decodes_as_kind(kind: mp::RecordKind, payload: &[u8]) -> bool {
    use mp::RecordKind as K;
    let max = wire::MAX_DIRECTORY;
    match kind {
        K::InstallationCredential => {
            wire::decode::<mp::InstallationCredential>(payload, max).is_ok()
        }
        K::InstallationRevocation => {
            wire::decode::<mp::InstallationRevocation>(payload, max).is_ok()
        }
        K::Message => wire::decode::<mp::Message>(payload, max).is_ok(),
        K::RouteClaim => wire::decode::<mp::RouteClaim>(payload, max).is_ok(),
        K::DirectoryQuery => wire::decode::<mp::DirectoryQuery>(payload, max).is_ok(),
        K::DirectoryAnswer => wire::decode::<mp::DirectoryAnswer>(payload, max).is_ok(),
        K::Unspecified => false,
    }
}

#[test]
fn every_signing_vector() {
    let vectors: Vec<SigningVector> =
        serde_json::from_slice(&data("signing-vectors.json")).unwrap();
    assert_eq!(vectors.len(), 6, "one fixture for each record kind");
    let mut kinds = Vec::new();
    for v in &vectors {
        let kind = record_kind(&v.kind);
        assert!(!kinds.contains(&kind), "repeated vector kind {}", v.kind);
        kinds.push(kind);
        let k = key(&hex(&v.test_seed_hex));
        let public = k.verifying_key().to_bytes();
        let payload = hex(&v.payload_hex);
        assert_eq!(to_hex(&public), v.public_key_hex, "{}: public key", v.kind);
        let input = sign::input(&v.kind, sign::FORMAT_MAJOR, &public, &payload);
        assert_eq!(
            to_hex(&input),
            v.signing_input_hex,
            "{}: signing input",
            v.kind
        );
        let rec = sign::sign(&k, kind, payload.clone());
        assert_eq!(
            to_hex(&rec.signature),
            v.signature_hex,
            "{}: signature",
            v.kind
        );
        assert_eq!(
            to_hex(&sign::hash(&rec).unwrap()),
            v.record_hash_hex,
            "{}: record hash",
            v.kind
        );
        // The vector's own signature verifies.
        let given = mp::SignedRecord {
            signature: hex(&v.signature_hex),
            ..rec.clone()
        };
        assert_eq!(sign::verify(&given, kind), Ok(()), "{}", v.kind);
        assert!(
            decodes_as_kind(kind, &payload),
            "{}: payload decodes strictly",
            v.kind
        );

        // Changed payload, domain, key and signature are refused.
        let mut changed = given.clone();
        let last = changed.payload.len() - 1;
        changed.payload[last] ^= 1;
        assert!(sign::verify(&changed, kind).is_err(), "{}: payload", v.kind);
        let other = if kind == mp::RecordKind::Message {
            mp::RecordKind::RouteClaim
        } else {
            mp::RecordKind::Message
        };
        let relabeled = mp::SignedRecord {
            kind: other as i32,
            ..given.clone()
        };
        assert!(
            sign::verify(&relabeled, other).is_err(),
            "{}: domain",
            v.kind
        );
        let mut changed = given.clone();
        changed.signer_public_key[0] ^= 1;
        assert!(sign::verify(&changed, kind).is_err(), "{}: key", v.kind);
        let mut changed = given;
        changed.signature[0] ^= 1;
        assert!(
            sign::verify(&changed, kind).is_err(),
            "{}: signature",
            v.kind
        );
    }
    eprintln!("signing vectors: {} passed", vectors.len());
}

// ---- interop vectors -------------------------------------------------------

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct InteropFile {
    #[allow(dead_code)]
    comment: String,
    authorities: Vec<Authority>,
    vectors: Vec<InteropVector>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Authority {
    namespace: String,
    test_seed_hex: String,
    public_key_hex: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct InteropVector {
    name: String,
    producer: String,
    frame_hex: String,
    #[serde(default)]
    message_hex: String,
    #[serde(default)]
    message: Option<serde_json::Value>,
    #[serde(default)]
    reject: String,
}

fn interop() -> InteropFile {
    // The schema is closed: an unknown field fails here (deny_unknown_fields),
    // and so does anything after the object.
    serde_json::from_slice(&data("interop-vectors.json")).expect("the interop file parses")
}

fn trusting(authorities: &[Authority]) -> (tempfile::TempDir, Store) {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(&dir.path().join("adapter.db"), 0).unwrap();
    for a in authorities {
        let seed = hex(&a.test_seed_hex);
        assert_eq!(
            seed.len(),
            32,
            "authority {}: test_seed_hex is 32 bytes",
            a.namespace
        );
        let public = key(&seed).verifying_key().to_bytes();
        assert_eq!(
            to_hex(&public),
            a.public_key_hex,
            "authority {}: seed and key disagree",
            a.namespace
        );
        store
            .put_binding(
                &Binding {
                    namespace: a.namespace.clone(),
                    authority_key: public.to_vec(),
                    revision: 1,
                    disabled: false,
                },
                0,
            )
            .unwrap();
    }
    (dir, store)
}

/// The message as Hub's vectors give it: protobuf JSON with proto field
/// names, decoded with the vendored schema.
fn json_message(value: &serde_json::Value) -> DynamicMessage {
    let desc = wire::descriptor("msgproto.v1.Message");
    DynamicMessage::deserialize(desc, value).expect("the vector's message is protobuf JSON")
}

#[test]
fn every_interop_vector() {
    let file = interop();
    let (_dir, store) = trusting(&file.authorities);
    let mut passed = 0;
    let mut transfers = 0;
    for v in &file.vectors {
        let raw = hex(&v.frame_hex);
        let parsed = wire::parse_frame(&raw)
            .unwrap_or_else(|e| panic!("{}: the frame does not decode strictly: {e}", v.name));
        assert!(
            parsed.frame.payload.is_some(),
            "{}: the frame is empty",
            v.name
        );
        let Some(mp::federation_frame::Payload::Transfer(_)) = parsed.frame.payload else {
            assert!(
                v.message_hex.is_empty() && v.message.is_none() && v.reject.is_empty(),
                "{}: only a transfer has a message or a verdict",
                v.name
            );
            passed += 1;
            continue;
        };
        transfers += 1;
        let envelope = parsed
            .envelope
            .expect("a transfer keeps its envelope bytes");
        let verdict = verify::envelope(&store, &envelope);
        let got = match &verdict {
            Ok(_) => "",
            Err(r) => r.code,
        };
        assert_eq!(got, v.reject, "{}: verdict ({verdict:?})", v.name);

        // message_hex and message come together or not at all; when given
        // they describe the envelope's signed payload exactly, whatever the
        // verdict.
        assert_eq!(
            v.message_hex.is_empty(),
            v.message.is_none(),
            "{}: give both message_hex and message, or neither",
            v.name
        );
        if let Some(message) = &v.message {
            let payload = match verdict {
                Ok(ok) => ok.bytes,
                Err(_) => wire::decode::<mp::SignedEnvelope>(&envelope, wire::MAX_ENVELOPE)
                    .ok()
                    .and_then(|e| e.message)
                    .map(|m| m.payload)
                    .unwrap_or_default(),
            };
            assert_eq!(to_hex(&payload), v.message_hex, "{}: message_hex", v.name);
            wire::decode::<mp::Message>(&payload, wire::MAX_MESSAGE)
                .unwrap_or_else(|e| panic!("{}: the payload is a strict Message: {e}", v.name));
            let from_bytes =
                DynamicMessage::decode(wire::descriptor("msgproto.v1.Message"), payload.as_slice())
                    .unwrap();
            assert_eq!(from_bytes, json_message(message), "{}: message", v.name);
        }
        passed += 1;
    }
    assert_eq!(passed, file.vectors.len());
    assert!(transfers > 0);
    eprintln!("interop vectors: {passed} passed ({transfers} transfers)");
}

/// The vector for a duplicated singular field: lenient decoding would take
/// the last content type, so only the strict walk refuses it, as
/// `invalid_message`.
#[test]
fn the_duplicate_field_vector_needs_the_strict_walk() {
    let file = interop();
    let v = file
        .vectors
        .iter()
        .find(|v| v.name == "transfer-duplicate-field")
        .expect("the vector is vendored");
    assert_eq!(v.reject, "invalid_message");
    let parsed = wire::parse_frame(&hex(&v.frame_hex)).unwrap();
    let env = mp::SignedEnvelope::decode(parsed.envelope.unwrap().as_slice()).unwrap();
    let payload = env.message.unwrap().payload;
    let lenient = mp::Message::decode(payload.as_slice()).expect("generated parsing accepts it");
    assert_eq!(lenient.content_type, "text/html");
    let err = wire::decode::<mp::Message>(&payload, wire::MAX_MESSAGE).unwrap_err();
    assert!(err.to_string().contains("singular field repeated"), "{err}");
}

/// Signing here matches Go byte for byte: rebuilding Go's verified transfer
/// frames from the same public test seeds (authority 0x01, installation
/// 0x02, as `goVectors` uses) and the retained message bytes gives the
/// identical frames.
#[test]
fn rust_rebuilds_go_transfers_byte_for_byte() {
    let file = interop();
    let authority = key(&[1; 32]);
    let installation = key(&[2; 32]);
    let credential = sign::sign(
        &authority,
        mp::RecordKind::InstallationCredential,
        mp::InstallationCredential {
            namespace: "hub".into(),
            installation_id: "hub-1".into(),
            public_key: installation.verifying_key().to_bytes().to_vec(),
            permissions: vec![
                mp::InstallationPermission::AssertSenders as i32,
                mp::InstallationPermission::HostNamespace as i32,
            ],
        }
        .encode_to_vec(),
    );
    let mut rebuilt = 0;
    for v in file
        .vectors
        .iter()
        .filter(|v| v.producer == "go" && v.reject.is_empty() && !v.message_hex.is_empty())
    {
        let envelope = mp::SignedEnvelope {
            message: Some(sign::sign(
                &installation,
                mp::RecordKind::Message,
                hex(&v.message_hex),
            )),
            credential: Some(credential.clone()),
        }
        .encode_to_vec();
        let frame = wire::transfer_frame(&format!("handoff-{}", v.name), &envelope);
        assert_eq!(to_hex(&frame), v.frame_hex, "{}", v.name);
        rebuilt += 1;
    }
    assert!(rebuilt >= 3, "plain, reply-to-run and group-copy");
}
