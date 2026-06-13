//! The persistent bottom hint bar (MVP-2 P0): a mode indicator plus the
//! high-frequency single-letter verbs. Stateless; rendered from the current `Mode`.

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

use crate::app::input::Mode;

/// The high-frequency verbs shown after the mode indicator (spec §2.2 footer).
const HINTS: &str = ": command   p pin   t tx   b build   g tip   q quit";

/// Render the one-line hint bar into `area`. Stateless: derives everything from
/// `mode`. The mode indicator is reverse-video in the mode's accent color.
pub fn render_hint_bar(frame: &mut Frame, area: Rect, mode: Mode) {
    let indicator = Span::styled(
        format!(" {} ", mode.label()),
        Style::default()
            .fg(mode.color())
            .add_modifier(Modifier::REVERSED | Modifier::BOLD),
    );
    let hints = Span::styled(
        format!(" {HINTS}"),
        Style::default().add_modifier(Modifier::DIM),
    );
    frame.render_widget(Paragraph::new(Line::from(vec![indicator, hints])), area);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::input::Mode;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    fn render_to_string(mode: Mode, w: u16, h: u16) -> String {
        let backend = TestBackend::new(w, h);
        let mut term = Terminal::new(backend).unwrap();
        term.draw(|f| render_hint_bar(f, f.area(), mode)).unwrap();
        let buf = term.backend().buffer().clone();
        let mut out = String::new();
        for y in 0..buf.area.height {
            for x in 0..buf.area.width {
                out.push_str(buf[(x, y)].symbol());
            }
            out.push('\n');
        }
        out
    }

    #[test]
    fn shows_mode_indicator_and_high_frequency_verbs() {
        let screen = render_to_string(Mode::Normal, 80, 1);
        assert!(screen.contains("NORMAL"), "mode label:\n{screen}");
        assert!(screen.contains(": command"), "command hint:\n{screen}");
        assert!(screen.contains("p pin"), "pin hint:\n{screen}");
        assert!(screen.contains("q quit"), "quit hint:\n{screen}");
    }

    #[test]
    fn reflects_command_mode_label() {
        let screen = render_to_string(Mode::Command, 80, 1);
        assert!(screen.contains("COMMAND"), "command mode label:\n{screen}");
    }
}
