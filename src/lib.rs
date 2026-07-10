//! LSP wire-protocol plumbing shared between the `typemux-cc` binary and its
//! integration test harness: message framing ([`framing`]), the JSON-RPC
//! message type ([`message`]), and error types ([`error`]). The rest of the
//! proxy (backend management, request dispatch, CLI) lives in binary-only
//! modules declared directly in `main.rs`, not re-exported here.

pub mod error;
pub mod framing;
pub mod message;
