//! Types and gRPC stubs generated from Hub's vendored protocol
//! (`proto/hub`): the shared `msgproto.v1` messages, the federation stream
//! and its signed records, and BBS's integration stream,
//! `bbs.integration.v1`. Modules mirror the proto packages so the generated
//! cross-package paths resolve.
#![allow(dead_code)]

pub mod msgproto {
    pub mod v1 {
        tonic::include_proto!("msgproto.v1");
    }
}

pub mod bbs {
    pub mod integration {
        pub mod v1 {
            tonic::include_proto!("bbs.integration.v1");
        }
    }
}

pub use bbs::integration::v1 as integration;
pub use msgproto::v1 as msgproto_v1;

/// The encoded `FileDescriptorSet` of every vendored Hub proto, for
/// validating untrusted bytes against the schema (Hub's strict wire rules).
pub const DESCRIPTORS: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/hub_descriptors.bin"));
