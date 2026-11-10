//! Bounded strict protobuf validation for untrusted msgproto bytes, as Hub's
//! `internal/wire` does (CONTRACT.md "Signed bytes and namespace trust").
//! Generated parsing accepts a repeated singular field (the last wins), so
//! [`strict`] walks the wire format against the message's descriptor first
//! and rejects malformed encoding, wire-type mismatches, a repeated known
//! singular field, conflicting oneof members, invalid UTF-8 in strings,
//! reserved field numbers and nesting deeper than 16. Unknown additive fields
//! are allowed and stay in the retained bytes.

use std::sync::OnceLock;

use bbs_core::hub_proto::{self, msgproto_v1 as mp};
use prost_reflect::{DescriptorPool, Kind, MessageDescriptor};

/// Hub's size limits (CONTRACT.md "References, IDs and limits").
pub const MAX_MESSAGE: usize = 96 << 10;
pub const MAX_ENVELOPE: usize = 128 << 10;
pub const MAX_CREDENTIAL: usize = 4 << 10;
pub const MAX_DIRECTORY: usize = 1 << 20;
pub const MAX_FRAME: usize = 4 << 20;
const MAX_DEPTH: usize = 16;

/// A strict-validation failure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WireError(pub String);

impl std::fmt::Display for WireError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "invalid protobuf: {}", self.0)
    }
}

impl std::error::Error for WireError {}

fn fail<T>(detail: String) -> Result<T, WireError> {
    Err(WireError(detail))
}

/// The vendored Hub protos' descriptors.
pub fn pool() -> &'static DescriptorPool {
    static POOL: OnceLock<DescriptorPool> = OnceLock::new();
    POOL.get_or_init(|| {
        DescriptorPool::decode(hub_proto::DESCRIPTORS).expect("the vendored descriptors decode")
    })
}

/// A generated type and the proto message it encodes.
pub trait Schema: prost::Message + Default {
    const NAME: &'static str;
}

macro_rules! schema {
    ($($t:ty => $name:literal),* $(,)?) => {
        $(impl Schema for $t { const NAME: &'static str = $name; })*
    };
}

schema! {
    mp::FederationFrame => "msgproto.v1.FederationFrame",
    mp::SignedEnvelope => "msgproto.v1.SignedEnvelope",
    mp::SignedRecord => "msgproto.v1.SignedRecord",
    mp::Message => "msgproto.v1.Message",
    mp::InstallationCredential => "msgproto.v1.InstallationCredential",
    mp::InstallationRevocation => "msgproto.v1.InstallationRevocation",
    mp::RouteClaim => "msgproto.v1.RouteClaim",
    mp::DirectoryQuery => "msgproto.v1.DirectoryQuery",
    mp::DirectoryAnswer => "msgproto.v1.DirectoryAnswer",
}

/// The descriptor of message `name`.
pub fn descriptor(name: &str) -> MessageDescriptor {
    pool()
        .get_message_by_name(name)
        .unwrap_or_else(|| panic!("{name} is in the vendored protos"))
}

// Wire types.
const VARINT: u8 = 0;
const I64: u8 = 1;
const LEN: u8 = 2;
const SGROUP: u8 = 3;
const EGROUP: u8 = 4;
const I32: u8 = 5;

fn varint(b: &[u8]) -> Option<(u64, usize)> {
    let mut v = 0u64;
    for (i, &byte) in b.iter().enumerate().take(10) {
        if i == 9 && byte > 1 {
            return None; // overflows 64 bits
        }
        v |= u64::from(byte & 0x7f) << (7 * i);
        if byte & 0x80 == 0 {
            return Some((v, i + 1));
        }
    }
    None
}

/// How deeply groups may nest within one field value, as protowire's
/// `DefaultRecursionLimit` allows (Go's `ConsumeFieldValue`).
const GROUP_LIMIT: usize = 10_000;

/// One field's tag: its number and wire type, and the tag's length. Field
/// numbers run from 1 to 2^31-1, as protowire's `ConsumeTag` accepts.
fn tag(b: &[u8]) -> Option<(u32, u8, usize)> {
    let (v, n) = varint(b)?;
    let num = v >> 3;
    if num == 0 || num > i32::MAX as u64 {
        return None;
    }
    Some((num as u32, (v & 7) as u8, n))
}

/// The length of a scalar or length-delimited value of wire type `typ`.
fn scalar_len(typ: u8, b: &[u8]) -> Option<usize> {
    match typ {
        VARINT => varint(b).map(|(_, n)| n),
        I64 => (b.len() >= 8).then_some(8),
        I32 => (b.len() >= 4).then_some(4),
        LEN => {
            let (len, n) = varint(b)?;
            let end = n.checked_add(usize::try_from(len).ok()?)?;
            (end <= b.len()).then_some(end)
        }
        _ => None,
    }
}

/// The length of a field's value (after its tag) of wire type `typ`,
/// including a whole group for a start-group tag, or None when malformed.
/// Groups are skipped without recursion, nested up to [`GROUP_LIMIT`]
/// below the first, each ended by its own number.
fn value_len(num: u32, typ: u8, b: &[u8]) -> Option<usize> {
    if typ != SGROUP {
        return scalar_len(typ, b);
    }
    let mut open = vec![num];
    let mut at = 0;
    loop {
        let (inner, t, n) = tag(&b[at..])?;
        at += n;
        match t {
            EGROUP => {
                if open.pop()? != inner {
                    return None;
                }
                if open.is_empty() {
                    return Some(at);
                }
            }
            SGROUP => {
                if open.len() > GROUP_LIMIT {
                    return None;
                }
                open.push(inner);
            }
            _ => at += scalar_len(t, &b[at..])?,
        }
    }
}

fn wire_type(kind: &Kind) -> u8 {
    match kind {
        Kind::Bool
        | Kind::Enum(_)
        | Kind::Int32
        | Kind::Sint32
        | Kind::Uint32
        | Kind::Int64
        | Kind::Sint64
        | Kind::Uint64 => VARINT,
        Kind::Fixed32 | Kind::Sfixed32 | Kind::Float => I32,
        Kind::Fixed64 | Kind::Sfixed64 | Kind::Double => I64,
        Kind::String | Kind::Bytes | Kind::Message(_) => LEN,
    }
}

/// The payload of a length-delimited value (its length prefix removed).
fn len_body(val: &[u8]) -> &[u8] {
    let (_, n) = varint(val).expect("a validated length");
    &val[n..]
}

/// Validate `b` as an encoding of `md`, strictly.
pub fn strict(b: &[u8], md: &MessageDescriptor) -> Result<(), WireError> {
    walk(b, md, 0)
}

fn walk(mut b: &[u8], md: &MessageDescriptor, depth: usize) -> Result<(), WireError> {
    if depth > MAX_DEPTH {
        return fail(format!("nesting deeper than {MAX_DEPTH}"));
    }
    let name = md.full_name();
    let mut seen: Vec<u32> = Vec::new();
    let mut oneofs: Vec<(String, u32)> = Vec::new();
    while !b.is_empty() {
        let Some((num, typ, n)) = tag(b) else {
            return fail(format!("{name}: malformed tag"));
        };
        b = &b[n..];
        let Some(vlen) = value_len(num, typ, b) else {
            return fail(format!("{name}: malformed field {num}"));
        };
        let val = &b[..vlen];
        b = &b[vlen..];
        let Some(fd) = md.get_field(num) else {
            if md.reserved_ranges().any(|r| r.contains(&num)) {
                return fail(format!("{name}: reserved field {num}"));
            }
            continue; // an unknown additive field: kept, ignored
        };
        let kind = fd.kind();
        let want = wire_type(&kind);
        let packed = fd.is_list() && typ == LEN && want != LEN;
        if typ != want && !packed {
            return fail(format!(
                "{name}.{}: wire type {typ}, want {want}",
                fd.name()
            ));
        }
        if !fd.is_list() && !fd.is_map() {
            if seen.contains(&num) {
                return fail(format!("{name}.{}: singular field repeated", fd.name()));
            }
            seen.push(num);
        }
        if let Some(od) = fd.containing_oneof()
            && !od.is_synthetic()
        {
            let oneof = od.full_name().to_string();
            match oneofs.iter().find(|(o, _)| *o == oneof) {
                Some((_, prev)) if *prev != num => {
                    return fail(format!(
                        "{name}: conflicting members of oneof {}",
                        od.name()
                    ));
                }
                Some(_) => {}
                None => oneofs.push((oneof, num)),
            }
        }
        if packed {
            let mut body = len_body(val);
            while !body.is_empty() {
                let m = match want {
                    VARINT => varint(body).map(|(_, n)| n),
                    I32 => (body.len() >= 4).then_some(4),
                    _ => (body.len() >= 8).then_some(8),
                };
                let Some(m) = m else {
                    return fail(format!("{name}.{}: malformed packed value", fd.name()));
                };
                body = &body[m..];
            }
            continue;
        }
        match kind {
            Kind::String if std::str::from_utf8(len_body(val)).is_err() => {
                return fail(format!("{name}.{}: invalid UTF-8", fd.name()));
            }
            Kind::Message(inner) => walk(len_body(val), &inner, depth + 1)?,
            _ => {}
        }
    }
    Ok(())
}

/// Why bytes did not decode: over the limit, or not a strict encoding.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DecodeError {
    TooLarge { len: usize, max: usize },
    Invalid(String),
}

impl std::fmt::Display for DecodeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DecodeError::TooLarge { len, max } => write!(f, "{len} bytes; limit {max}"),
            DecodeError::Invalid(detail) => f.write_str(detail),
        }
    }
}

impl std::error::Error for DecodeError {}

/// Bound, strictly validate, then decode `b`.
pub fn decode<M: Schema>(b: &[u8], max: usize) -> Result<M, DecodeError> {
    if b.len() > max {
        return Err(DecodeError::TooLarge { len: b.len(), max });
    }
    strict(b, &descriptor(M::NAME)).map_err(|e| DecodeError::Invalid(e.to_string()))?;
    M::decode(b).map_err(|e| DecodeError::Invalid(format!("invalid protobuf: {e}")))
}

/// The bytes of length-delimited field `num` in `b`, which has passed
/// [`strict`], so a known field occurs at most once.
pub fn field(mut b: &[u8], num: u32) -> Option<&[u8]> {
    while !b.is_empty() {
        let (n, typ, l) = tag(b)?;
        b = &b[l..];
        let vl = value_len(n, typ, b)?;
        if n == num && typ == LEN {
            return Some(len_body(&b[..vl]));
        }
        b = &b[vl..];
    }
    None
}

fn put_varint(out: &mut Vec<u8>, mut v: u64) {
    while v >= 0x80 {
        out.push((v as u8) | 0x80);
        v >>= 7;
    }
    out.push(v as u8);
}

/// Append length-delimited field `num` holding `value`.
pub fn put_bytes(out: &mut Vec<u8>, num: u32, value: &[u8]) {
    put_varint(out, (u64::from(num) << 3) | u64::from(LEN));
    put_varint(out, value.len() as u64);
    out.extend_from_slice(value);
}

/// A `FederationFrame` carrying a `Transfer` of exactly `envelope`'s bytes,
/// written by hand so encoding cannot alter them.
pub fn transfer_frame(handoff_id: &str, envelope: &[u8]) -> Vec<u8> {
    let mut transfer = Vec::with_capacity(handoff_id.len() + envelope.len() + 16);
    put_bytes(&mut transfer, 1, handoff_id.as_bytes());
    put_bytes(&mut transfer, 2, envelope);
    let mut frame = Vec::with_capacity(transfer.len() + 8);
    put_bytes(&mut frame, 6, &transfer);
    frame
}

/// A received federation frame, decoded strictly. A transfer's envelope is
/// also kept as the exact bytes it arrived in.
#[derive(Debug)]
pub struct Frame {
    pub frame: mp::FederationFrame,
    pub envelope: Option<Vec<u8>>,
}

/// Decode a received federation frame. A frame that is not a strict
/// encoding ends the stream, as Hub's codec does.
pub fn parse_frame(raw: &[u8]) -> Result<Frame, DecodeError> {
    let frame: mp::FederationFrame = decode(raw, MAX_FRAME)?;
    let envelope = match &frame.payload {
        Some(mp::federation_frame::Payload::Transfer(_)) => Some(
            field(field(raw, 6).unwrap_or_default(), 2)
                .unwrap_or_default()
                .to_vec(),
        ),
        _ => None,
    };
    Ok(Frame { frame, envelope })
}

#[cfg(test)]
mod tests {
    use super::*;
    use prost::Message as _;

    fn message() -> mp::Message {
        mp::Message {
            id: Some(mp::MessageRef {
                namespace: "hub".into(),
                message_id: "m1".into(),
            }),
            content_type: "text/plain; charset=utf-8".into(),
            body: b"hi".to_vec(),
            kind: mp::MessageKind::Application as i32,
            ..Default::default()
        }
    }

    fn check(b: &[u8]) -> Result<(), WireError> {
        strict(b, &descriptor("msgproto.v1.Message"))
    }

    #[test]
    fn accepts_a_plain_encoding_and_unknown_fields() {
        let mut b = message().encode_to_vec();
        assert_eq!(check(&b), Ok(()));
        // Unknown field 99, varint, and an unknown group field 98.
        b.extend_from_slice(&[0x98, 0x06, 0x01]);
        b.extend_from_slice(&[0x93, 0x06, 0x08, 0x01, 0x94, 0x06]);
        assert_eq!(check(&b), Ok(()));
        let decoded: mp::Message = decode(&b, MAX_MESSAGE).unwrap();
        assert_eq!(decoded, message());
    }

    #[test]
    fn rejects_a_duplicated_singular_field() {
        let mut b = message().encode_to_vec();
        put_bytes(&mut b, 6, b"text/html");
        let err = check(&b).unwrap_err();
        assert!(
            err.0.contains("content_type: singular field repeated"),
            "{err}"
        );
        // Generated parsing alone accepts it, taking the last value.
        assert_eq!(
            mp::Message::decode(b.as_slice()).unwrap().content_type,
            "text/html"
        );
        // A duplicated nested message, too.
        let mut b = message().encode_to_vec();
        put_bytes(&mut b, 1, &mp::MessageRef::default().encode_to_vec());
        assert!(check(&b).is_err());
    }

    #[test]
    fn rejects_duplicates_inside_nested_messages() {
        let mut id = mp::MessageRef {
            namespace: "hub".into(),
            message_id: "m1".into(),
        }
        .encode_to_vec();
        put_bytes(&mut id, 2, b"m2");
        let mut b = Vec::new();
        put_bytes(&mut b, 1, &id);
        let err = check(&b).unwrap_err();
        assert!(err.0.contains("MessageRef.message_id"), "{err}");
    }

    #[test]
    fn rejects_wire_type_mismatch_bad_utf8_reserved_and_malformed() {
        // content_type (6) as a varint.
        assert!(check(&[0x30, 0x01]).is_err());
        // content_type with invalid UTF-8.
        assert!(check(&[0x32, 0x01, 0xff]).is_err());
        // Reserved field 8 (the old reply_to), any wire type.
        assert!(
            check(&[0x42, 0x00])
                .unwrap_err()
                .0
                .contains("reserved field 8")
        );
        // Truncated length, truncated varint, field number 0, and a stray
        // end-group tag.
        assert!(check(&[0x32, 0x05, b'a']).is_err());
        assert!(check(&[0x20, 0xff]).is_err());
        assert!(check(&[0x02, 0x00]).is_err());
        assert!(check(&[0x9c, 0x06]).is_err());
        // A varint over 64 bits.
        assert!(
            check(&[
                0x20, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0x02
            ])
            .is_err()
        );
        // A group closed by another field's end tag.
        assert!(check(&[0x93, 0x06, 0x9c, 0x06]).is_err());
    }

    #[test]
    fn rejects_conflicting_oneof_members_and_repeats_of_one() {
        let hello = mp::PeerHello::default().encode_to_vec();
        let beat = mp::Heartbeat { nonce: 1 }.encode_to_vec();
        let md = descriptor("msgproto.v1.FederationFrame");
        let mut b = Vec::new();
        put_bytes(&mut b, 1, &hello);
        put_bytes(&mut b, 5, &beat);
        assert!(
            strict(&b, &md)
                .unwrap_err()
                .0
                .contains("conflicting members")
        );
        let mut b = Vec::new();
        put_bytes(&mut b, 5, &beat);
        put_bytes(&mut b, 5, &beat);
        assert!(
            strict(&b, &md)
                .unwrap_err()
                .0
                .contains("singular field repeated")
        );
    }

    #[test]
    fn repeated_fields_may_repeat_packed_or_not() {
        let md = descriptor("msgproto.v1.InstallationCredential");
        // permissions (4): two unpacked entries, then a packed run.
        assert_eq!(strict(&[0x20, 0x01, 0x20, 0x02], &md), Ok(()));
        assert_eq!(strict(&[0x22, 0x02, 0x01, 0x02], &md), Ok(()));
        assert!(strict(&[0x22, 0x02, 0x01, 0xff], &md).is_err());
    }

    #[test]
    fn unknown_groups_nest_as_deep_as_go_allows() {
        // Unknown field 98 as nested groups: Go's protowire skips them up to
        // its recursion limit (10,000 below the first), so 20 deep is fine,
        // and so is the limit itself; one more is refused.
        let nested = |depth: usize| {
            let mut b = Vec::new();
            for _ in 0..depth {
                b.extend_from_slice(&[0x93, 0x06]);
            }
            for _ in 0..depth {
                b.extend_from_slice(&[0x94, 0x06]);
            }
            b
        };
        check(&nested(20)).unwrap();
        check(&nested(GROUP_LIMIT + 1)).unwrap();
        assert!(check(&nested(GROUP_LIMIT + 2)).is_err());
        // A group must end with its own number, and an end alone is malformed.
        assert!(check(&[0x93, 0x06, 0xa4, 0x06]).is_err());
        assert!(check(&[0x94, 0x06]).is_err());
    }

    #[test]
    fn field_numbers_run_to_two_to_the_31() {
        let field = |num: u64| {
            let mut b = Vec::new();
            prost::encoding::encode_varint(num << 3, &mut b);
            b.push(1);
            b
        };
        // Unknown varint fields: the largest number Go accepts, then past it.
        check(&field((1 << 31) - 1)).unwrap();
        check(&field(1 << 29)).unwrap();
        assert!(check(&field(1 << 31)).is_err());
        assert!(check(&field(0)).is_err());
    }

    #[test]
    fn decode_bounds_before_parsing() {
        let b = message().encode_to_vec();
        assert_eq!(
            decode::<mp::Message>(&b, 3),
            Err(DecodeError::TooLarge {
                len: b.len(),
                max: 3
            })
        );
    }

    #[test]
    fn transfer_frames_keep_the_envelope_bytes_exactly() {
        // Bytes no encoder would write for a SignedEnvelope: an unknown
        // field first. They survive the round trip unchanged.
        let mut envelope = vec![0xf8, 0x06, 0x01];
        put_bytes(
            &mut envelope,
            1,
            &mp::SignedRecord::default().encode_to_vec(),
        );
        let raw = transfer_frame("h1", &envelope);
        let parsed = parse_frame(&raw).unwrap();
        assert_eq!(parsed.envelope.as_deref(), Some(envelope.as_slice()));
        match parsed.frame.payload {
            Some(mp::federation_frame::Payload::Transfer(t)) => assert_eq!(t.handoff_id, "h1"),
            other => panic!("{other:?}"),
        }
    }
}
