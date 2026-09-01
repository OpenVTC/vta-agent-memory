//! Agent memory backed by a Verifiable Trust Agent.
//!
//! The binary in `main.rs` is a thin front end over these modules — an MCP
//! server, a setup flow, and a handful of one-shot commands. They are a library
//! so the behaviour that matters can be tested without either of those: `tests/`
//! drives [`store::Store`] against the SDK's in-process loopback transport, so
//! the payloads the client actually builds are exercised with no VTA, no
//! mediator, and no socket.
//!
//! Start at [`record`] — the key encoding and the ranking there are where the
//! design decisions live. [`store`] is the only module that talks to a VTA.

pub mod config;
pub mod enrol;
pub mod fence;
pub mod lazy;
pub mod pnm;
pub mod record;
pub mod server;
pub mod setup;
pub mod store;
