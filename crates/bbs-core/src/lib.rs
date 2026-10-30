//! What every bbs executable shares: the protocol and its generated types,
//! the database and the service that runs every request against it, and the
//! client that reaches it in-process or over gRPC.

pub mod auth;
pub mod backend;
pub mod blocker_choices;
pub mod db;
pub mod names;
pub mod proto;
pub mod protocol;
pub mod release;
pub mod search;
pub mod service;
pub mod timer;
