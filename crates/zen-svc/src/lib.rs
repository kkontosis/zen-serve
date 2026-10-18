//! Protocol engines for relayed services (svc.md §7): sans-IO, with a state
//! that serializes at protocol boundaries, so a handler can stop between
//! two steps and another one continue the live connection. Built natively
//! and for wasm32 (`@zen/svc-wasm`).
//!
//! * [`tls`]: a TLS 1.3 and 1.2 server (svc.md §7.1).

pub mod tls;
