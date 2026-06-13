//! Input modes and mode-driven key routing (MVP-2 P0).
//!
//! `Mode` is pure UI-loop state (like `phase`/`follow`); it never crosses the
//! `Command`/`Event` channel. `route_key` replaces the ad-hoc phase/overlay
//! branching the grid used in MVP-1, without changing any leaf behavior.

use crossterm::event::{KeyCode, KeyEvent};
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

/// The routing decision for one key, given the current mode. The app loop reads
/// this and performs the side effect (it owns the overlays + channels). This
/// keeps the decision pure and unit-testable; behavior matches MVP-1 exactly.
#[derive(Debug)]
pub enum KeyRouting {
    /// `:` pressed in Normal — open the command palette (enter Command mode).
    OpenPalette,
    /// `p` / `:pin` — open the storage picker (enter Insert mode).
    OpenPicker,
    /// `t` / `:tx` — open the tx builder (enter Insert mode).
    OpenTxBuilder,
    /// A grid-navigation / verb key; forward to `AppState::on_key`.
    Grid(KeyEvent),
    /// Command mode is active; forward to the palette.
    Palette(KeyEvent),
    /// Insert mode is active; forward to the open overlay.
    Overlay(KeyEvent),
}

/// Decide where a key goes, given the current `Mode`. Pure — no side effects.
///
/// MVP-1 parity: in `Normal`, `p`/`t` open overlays, `:` opens the palette, and
/// everything else is grid navigation (the same set `AppState::on_key` handled).
/// `Command` keys go to the palette; `Insert` keys go to the active overlay.
pub fn route_key(mode: Mode, key: KeyEvent) -> KeyRouting {
    match mode {
        Mode::Command => KeyRouting::Palette(key),
        Mode::Insert => KeyRouting::Overlay(key),
        Mode::Normal => match key.code {
            KeyCode::Char(':') => KeyRouting::OpenPalette,
            KeyCode::Char('p') => KeyRouting::OpenPicker,
            KeyCode::Char('t') => KeyRouting::OpenTxBuilder,
            _ => KeyRouting::Grid(key),
        },
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

    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    fn press(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    #[test]
    fn colon_in_normal_opens_command_mode() {
        assert!(matches!(
            route_key(Mode::Normal, press(KeyCode::Char(':'))),
            KeyRouting::OpenPalette
        ));
    }

    #[test]
    fn normal_letter_verbs_route_to_grid() {
        for c in ['b', 'g', 'q', 'r'] {
            assert!(
                matches!(route_key(Mode::Normal, press(KeyCode::Char(c))), KeyRouting::Grid(_)),
                "`{c}` should route to the grid"
            );
        }
    }

    #[test]
    fn normal_p_and_t_open_overlays() {
        assert!(matches!(route_key(Mode::Normal, press(KeyCode::Char('p'))), KeyRouting::OpenPicker));
        assert!(matches!(route_key(Mode::Normal, press(KeyCode::Char('t'))), KeyRouting::OpenTxBuilder));
    }

    #[test]
    fn normal_arrows_route_to_grid() {
        for code in [KeyCode::Left, KeyCode::Right, KeyCode::Up, KeyCode::Down] {
            assert!(matches!(route_key(Mode::Normal, press(code)), KeyRouting::Grid(_)));
        }
    }

    #[test]
    fn command_mode_routes_to_palette() {
        assert!(matches!(
            route_key(Mode::Command, press(KeyCode::Char('s'))),
            KeyRouting::Palette(_)
        ));
    }

    #[test]
    fn insert_mode_routes_to_overlay() {
        assert!(matches!(
            route_key(Mode::Insert, press(KeyCode::Char('x'))),
            KeyRouting::Overlay(_)
        ));
    }
}
