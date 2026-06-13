//! Input modes and mode-driven key routing (MVP-2 P0).
//!
//! `Mode` is pure UI-loop state (like `phase`/`follow`); it never crosses the
//! `Command`/`Event` channel. `route_key` replaces the ad-hoc phase/overlay
//! branching the grid used in MVP-1, without changing any leaf behavior.

use ratatui::style::Color;

/// The current input mode. Pure UI-loop state; never crosses the `Command`/
/// `Event` channel. Drives key routing (`route_key`) and the hint-bar indicator.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Mode {
    /// Grid focus: single-letter verbs, arrows scroll, `:` → Command.
    #[default]
    Normal,
    /// The `:` palette has focus.
    Command,
    /// A text field (overlay) has focus.
    Insert,
}

impl Mode {
    /// The uppercase label shown in the hint bar.
    pub fn label(self) -> &'static str {
        match self {
            Mode::Normal => "NORMAL",
            Mode::Command => "COMMAND",
            Mode::Insert => "INSERT",
        }
    }

    /// The accent color for this mode (spec §2.1 / §2.3).
    pub fn color(self) -> Color {
        match self {
            Mode::Normal => Color::Blue,
            Mode::Command => Color::Magenta, // purple
            Mode::Insert => Color::Green,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mode_default_is_normal() {
        assert_eq!(Mode::default(), Mode::Normal);
    }

    #[test]
    fn mode_label_and_color_per_variant() {
        assert_eq!(Mode::Normal.label(), "NORMAL");
        assert_eq!(Mode::Command.label(), "COMMAND");
        assert_eq!(Mode::Insert.label(), "INSERT");
    }
}
