//! Hub's common entity/address references, as BBS stores them from schema 27.
//!
//! Every native human, agent and group is one entity with an immutable
//! entity ID in the installation's namespace, and one stable address,
//! [`NATIVE_ADDRESS`]. Messages and deliveries refer to addresses; a BBS name
//! is only the entity's display name.

/// The stable address ID every native entity gets.
pub const NATIVE_ADDRESS: &str = "native";

/// The installation's namespace when a new database is created without one.
pub const DEFAULT_NAMESPACE: &str = "bbs";

/// What a native entity is in BBS.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum EntityKind {
    Human,
    Agent,
    Group,
}

impl EntityKind {
    pub fn as_str(self) -> &'static str {
        match self {
            EntityKind::Human => "human",
            EntityKind::Agent => "agent",
            EntityKind::Group => "group",
        }
    }
}

/// An entity: `namespace` and its immutable entity ID, `will` or `will/scout`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EntityRef {
    pub namespace: String,
    pub entity_id: String,
}

/// One of an entity's stable delivery addresses.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AddressRef {
    pub entity: EntityRef,
    pub address_id: String,
}

/// The longest entity, address, run, message or handoff ID (Hub's contract).
pub const MAX_ID: usize = 128;

/// An opaque Hub ID (message, address, run, handoff or query): 1-128
/// characters from `[A-Za-z0-9_-]`.
pub fn valid_id(id: &str) -> bool {
    (1..=MAX_ID).contains(&id.len()) && id.bytes().all(id_byte)
}

/// An entity ID: a top-level name or `owner/name`, each part from
/// `[A-Za-z0-9_-]+`, at most 128 characters in all (D126).
pub fn valid_entity_id(id: &str) -> bool {
    id.len() <= MAX_ID
        && id.split('/').count() <= 2
        && id
            .split('/')
            .all(|part| !part.is_empty() && part.bytes().all(id_byte))
}

fn id_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_' || b == b'-'
}

/// How BBS names an address in another installation: Hub's explicit-ID text
/// reference `id:ENTITY@NAMESPACE`, with `:ADDRESS` unless it is
/// [`NATIVE_ADDRESS`] and `#RUN` when a run is selected. Sends accept the same
/// form (see [`parse_remote`]).
pub fn remote_display(namespace: &str, entity_id: &str, address_id: &str, run: &str) -> String {
    let mut out = format!("id:{entity_id}@{namespace}");
    if address_id != NATIVE_ADDRESS {
        out.push(':');
        out.push_str(address_id);
    }
    if !run.is_empty() {
        out.push('#');
        out.push_str(run);
    }
    out
}

/// A remote address as a send names it: [`remote_display`]'s form.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteAddress {
    pub address: AddressRef,
    pub run: Option<String>,
}

/// Parse `ENTITY@NAMESPACE[:ADDRESS][#RUN]`, the part after `id:`.
pub fn parse_remote(reference: &str) -> anyhow::Result<RemoteAddress> {
    let usage = || {
        anyhow::anyhow!(
            "invalid remote address id:{reference}: expected id:ENTITY@NAMESPACE, optionally \
             followed by :ADDRESS and #RUN, such as id:will/scout@hub"
        )
    };
    let (rest, run) = match reference.split_once('#') {
        Some((rest, run)) => (rest, Some(run)),
        None => (reference, None),
    };
    let (entity_id, rest) = rest.split_once('@').ok_or_else(usage)?;
    let (namespace, address_id) = match rest.split_once(':') {
        Some((namespace, address)) => (namespace, address),
        None => (rest, NATIVE_ADDRESS),
    };
    let valid = valid_entity_id(entity_id)
        && crate::db::validate_namespace(namespace).is_ok()
        && valid_id(address_id)
        && run.is_none_or(valid_id);
    if !valid {
        return Err(usage());
    }
    Ok(RemoteAddress {
        address: AddressRef {
            entity: EntityRef {
                namespace: namespace.to_string(),
                entity_id: entity_id.to_string(),
            },
            address_id: address_id.to_string(),
        },
        run: run.map(str::to_string),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_follow_hubs_grammar() {
        assert!(valid_id("abc_D-9"));
        assert!(!valid_id(""));
        assert!(!valid_id("a/b"));
        assert!(!valid_id(&"a".repeat(129)));
        assert!(valid_entity_id("will"));
        assert!(valid_entity_id("will/scout"));
        assert!(!valid_entity_id("will/scout/run"));
        assert!(!valid_entity_id("will/"));
        assert!(!valid_entity_id("/scout"));
        assert!(!valid_entity_id("will.scout"));
        assert!(!valid_entity_id(&format!("w/{}", "a".repeat(127))));
    }

    #[test]
    fn remote_addresses_round_trip() {
        for text in ["will/scout@hub", "ada@acme:slack#run-1", "ada@acme#r"] {
            let r = parse_remote(text).unwrap();
            let shown = remote_display(
                &r.address.entity.namespace,
                &r.address.entity.entity_id,
                &r.address.address_id,
                r.run.as_deref().unwrap_or(""),
            );
            assert_eq!(shown, format!("id:{text}"));
        }
        assert_eq!(
            parse_remote("will@hub").unwrap().address.address_id,
            "native"
        );
        for bad in [
            "will",
            "will@Hub",
            "a/b/c@hub",
            "will@hub:",
            "will@hub#",
            "@hub",
        ] {
            assert!(parse_remote(bad).is_err(), "{bad}");
        }
    }
}
