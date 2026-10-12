//! The wire types and gRPC client/server stubs for `cordata.harness.v1`.
//!
//! Generated at build time from `proto/harness.proto`; see that file for what every message
//! means and the evolution rules (numbers are never reused, new fields are additive).

#![allow(clippy::all, clippy::pedantic)]

/// `cordata.harness.v1`.
pub mod v1 {
    tonic::include_proto!("cordata.harness.v1");
}

pub use v1::*;

/// The call metadata key every RPC must carry the daemon token under.
pub const TOKEN_KEY: &str = "x-daemon-token";
