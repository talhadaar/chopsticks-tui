//! TUI views: the storage grid plus the picker, tx-builder, and connection
//! overlays. Views render from `AppState` and emit `Command`; they never call
//! RPC directly.

pub mod connection;
pub mod grid;
pub mod picker;
pub mod tx_builder;
