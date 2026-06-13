//! TUI views: the storage grid plus the picker, tx-builder, and connection
//! overlays. Views render from `AppState` and emit `Command`; they never call
//! RPC directly.

pub mod build_panel;
pub mod command_registry;
pub mod connection;
pub mod grid;
pub mod hint_bar;
pub mod palette;
pub mod picker;
pub mod sessions;
pub mod set_storage;
pub mod tx_builder;
