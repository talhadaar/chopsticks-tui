//! Resilience helpers (ticket T14).
//!
//! MVP-1 resilience has three parts, all wired here or in the modules referenced:
//!
//! 1. **ws/RPC disconnect** — on `Event::Disconnected` the grid *freezes* (keeps
//!    its last columns rather than clearing) and a banner offers reconnect. See
//!    [`disconnect_banner`] and `AppState::on_event` / `AppState::push_column`.
//! 2. **Per-cell decode failure** — a single undecodable storage value becomes
//!    `CellState::Undecodable`, which the grid renders as `⚠ <undecodable>`; it
//!    never panics the render loop (handled in `chain::storage_fetch` + the grid
//!    widget).
//! 3. **Panic safety** — `main` installs `ratatui::init()`'s panic hook, which
//!    restores the terminal (leaves raw mode + the alternate screen) on any exit
//!    path including panics. [`panic_guard_installed`] documents the contract.

/// The banner shown when the chain connection drops. The grid keeps its last
/// data; pressing `r` issues `Command::Reconnect`.
pub fn disconnect_banner(reason: &str) -> String {
    format!("Disconnected: {reason} — press r to reconnect")
}

/// Marker documenting that the terminal-restore panic guard is owned by `main`
/// (via `ratatui::init()` / `ratatui::restore()`), not the app loop. Kept as a
/// single source of truth so the guarantee is greppable.
pub const fn panic_guard_installed() -> bool {
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn disconnect_banner_mentions_reason_and_reconnect() {
        let b = disconnect_banner("ws closed");
        assert!(b.contains("ws closed"));
        assert!(b.contains("reconnect"));
    }
}
