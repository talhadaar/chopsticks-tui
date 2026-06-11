//! Binary entry point: terminal lifecycle + panic guard around the app loop.

use anyhow::Result;

fn main() -> Result<()> {
    // `ratatui::init` switches to the alternate screen + raw mode AND installs a
    // panic hook that restores the terminal before unwinding. That hook is our
    // panic guard: a crash never leaves the user's terminal in raw mode.
    let terminal = ratatui::init();
    let result = chopsticks_tui::app::run(terminal);
    ratatui::restore();
    result
}
