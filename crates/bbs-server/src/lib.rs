//! The bbs server: gRPC for agents and clients over the shared service, and
//! a browser UI for humans.

// The shared core, under the names the server's modules use.
#[cfg(test)]
use bbs_core::backend;
use bbs_core::release;
use bbs_core::{auth, db, proto, protocol, service};

pub mod serve;
