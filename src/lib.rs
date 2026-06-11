//! `chopsticks-tui` — a Rust TUI for Substrate/Polkadot-SDK chain dev & QA,
//! built on Chopsticks.
//!
//! The library exposes the shared contracts (and, as later tickets land, the
//! chain client, rendering, views, and app orchestration). The binary
//! (`src/main.rs`) is a thin entry point over this library.

pub mod contracts;
