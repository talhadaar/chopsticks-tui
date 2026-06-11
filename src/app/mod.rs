//! Application orchestration: `AppState`, the event loop, and `Command`
//! dispatch to async tasks (ticket T14).

pub mod resilience;

use crate::contracts::Result;

/// Run the application event loop. (Placeholder — the real loop lands in T14;
/// the terminal lifecycle wraps this call in `main`.)
pub fn run() -> Result<()> {
    Ok(())
}
