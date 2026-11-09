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
