//! The human's personal message history, kept on disk in step with the
//! server's sync feed so it can be searched in full and browsed offline.

mod search;
mod store;

pub use search::{Contexts, Searcher, Where};
#[cfg(test)]
pub use store::Place;
pub use store::{Batch, Found, Store, StoreError, dir_for};
