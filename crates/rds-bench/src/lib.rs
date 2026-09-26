//! Benchmark harness for rds transports.
//!
//! The measurer the checkpoint protocol (docs/implementation-plan.md)
//! runs on: deterministic scenarios over real QUIC paths — direct,
//! impaired (through the in-crate UDP proxy) and relay — producing JSON
//! and markdown reports under `docs/reports/`.
//!
//! Cases follow the QUIC interop runner taxonomy where it maps:
//! `handshake`, `transfer`, `multiconnect`, plus rds-specific
//! `ping`, `relay-fallback`, `impaired`.

pub mod capacity;
pub mod impair;
pub mod receipt;
pub mod report;
pub mod scenario;
mod transfer;
pub mod world;
