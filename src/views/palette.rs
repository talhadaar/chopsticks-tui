//! The command palette overlay (MVP-2 P0): a `:`-launched fuzzy list over every
//! registered verb. Pure UI — emits a `PaletteOutcome`; the app loop routes it.

use crossterm::event::{KeyCode, KeyEvent, KeyEventKind};
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, List, ListItem, Paragraph};

use crate::views::command_registry::{CommandSpec, RpcLabel, registry};

/// The purple accent for command / dev surfaces (spec §2.3).
const PURPLE: Color = Color::Magenta;

/// Case-insensitive subsequence ("fuzzy") match — same approach as the storage
/// picker (`picker::fuzzy_match`), re-implemented here as that one is private.
fn fuzzy_match(haystack: &str, needle: &str) -> bool {
    if needle.is_empty() {
        return true;
    }
    let mut hay = haystack.chars().flat_map(char::to_lowercase);
    'outer: for nc in needle.chars().flat_map(char::to_lowercase) {
        for hc in hay.by_ref() {
            if hc == nc {
                continue 'outer;
            }
        }
        return false;
    }
    true
}

/// What the app loop should do after feeding a key to the palette.
#[derive(Debug)]
pub enum PaletteOutcome {
    /// Still typing; keep the palette open.
    Pending,
    /// `Esc` — close the palette, no command.
    Cancel,
    /// `Enter` — the raw command line to parse + route (verb + any args).
    Submit(String),
}

/// The `:`-command palette overlay.
///
/// `query` is the full line the user typed *after* the `:` (verb plus optional
/// args). The filter matches on the **verb token** so args don't break matching.
pub struct CommandPalette {
    /// The full typed line (verb + args), without the leading `:`.
    query: String,
    /// Index into the filtered list.
    selected: usize,
}

impl Default for CommandPalette {
    fn default() -> Self {
        Self::new()
    }
}

impl CommandPalette {
    pub fn new() -> Self {
        Self { query: String::new(), selected: 0 }
    }

    /// The verb token (first whitespace-delimited word) used for fuzzy matching.
    fn verb_token(&self) -> &str {
        self.query.split_whitespace().next().unwrap_or("")
    }

    /// Whether the user has started typing args (a space after the verb).
    fn has_args(&self) -> bool {
        self.query.trim().contains(char::is_whitespace)
    }

    /// Commands whose name fuzzy-matches the current verb token.
    pub fn filtered(&self) -> Vec<&'static CommandSpec> {
        let needle = self.verb_token();
        registry().iter().filter(|c| fuzzy_match(c.name, needle)).collect()
    }

    /// Feed one key. Returns the resulting `PaletteOutcome`.
    pub fn on_key(&mut self, key: KeyEvent) -> PaletteOutcome {
        if key.kind == KeyEventKind::Release {
            return PaletteOutcome::Pending;
        }
        match key.code {
            KeyCode::Esc => PaletteOutcome::Cancel,
            KeyCode::Enter => {
                // If the user typed args, submit the literal line. Otherwise
                // complete to the selected command's name.
                let line = if self.has_args() {
                    self.query.trim().to_string()
                } else if let Some(spec) = self.filtered().get(self.selected) {
                    spec.name.to_string()
                } else {
                    self.query.trim().to_string()
                };
                PaletteOutcome::Submit(line)
            }
            KeyCode::Up => {
                self.selected = self.selected.saturating_sub(1);
                PaletteOutcome::Pending
            }
            KeyCode::Down => {
                let max = self.filtered().len().saturating_sub(1);
                self.selected = (self.selected + 1).min(max);
                PaletteOutcome::Pending
            }
            KeyCode::Backspace => {
                self.query.pop();
                self.selected = 0;
                PaletteOutcome::Pending
            }
            KeyCode::Char(c) => {
                self.query.push(c);
                self.selected = 0;
                PaletteOutcome::Pending
            }
            _ => PaletteOutcome::Pending,
        }
    }

    /// Bottom-anchored render: an input row plus one row per filtered command,
    /// each `name  description  …  <rpc label>` with the selection in purple.
    pub fn render(&self, frame: &mut Frame, area: Rect) {
        let matches = self.filtered();
        // Height: input row + the list's TOP border + up to 8 result rows,
        // clamped to the area. The `+ 2` covers the input row and the border so
        // the visible result rows aren't starved when only a few commands match.
        let rows = (matches.len() as u16 + 2).min(10).min(area.height.max(1));
        let popup = Rect {
            x: area.x,
            y: area.y + area.height.saturating_sub(rows),
            width: area.width,
            height: rows,
        };
        frame.render_widget(Clear, popup);

        let layout = ratatui::layout::Layout::vertical([
            ratatui::layout::Constraint::Length(1),
            ratatui::layout::Constraint::Min(0),
        ])
        .split(popup);

        // Input line: `› <query>  N of M commands`.
        let counter = format!("{} of {} commands", matches.len(), registry().len());
        let pad = (popup.width as usize)
            .saturating_sub(2 + self.query.len() + counter.len() + 2);
        let input = Line::from(vec![
            Span::styled("› ", Style::default().fg(PURPLE)),
            Span::raw(self.query.clone()),
            Span::raw(" ".repeat(pad)),
            Span::styled(counter, Style::default().add_modifier(Modifier::DIM)),
            Span::raw(" "),
        ]);
        frame.render_widget(Paragraph::new(input), layout[0]);

        // Result rows.
        let width = popup.width as usize;
        let items: Vec<ListItem> = matches
            .iter()
            .enumerate()
            .map(|(i, spec)| {
                let label = match spec.rpc {
                    RpcLabel::Dev(m) => m.to_string(),
                    RpcLabel::Local => "local".to_string(),
                };
                let left = format!("  {:<14}{}", spec.name, spec.description);
                let pad = width.saturating_sub(left.chars().count() + label.chars().count() + 1);
                let selected = i == self.selected;
                let name_style = if selected {
                    Style::default().fg(PURPLE).add_modifier(Modifier::BOLD)
                } else {
                    Style::default()
                };
                let label_style = match spec.rpc {
                    RpcLabel::Dev(_) => Style::default().fg(PURPLE),
                    RpcLabel::Local => Style::default().add_modifier(Modifier::DIM),
                };
                let line = Line::from(vec![
                    Span::styled(left, name_style),
                    Span::raw(" ".repeat(pad)),
                    Span::styled(label, label_style),
                ]);
                ListItem::new(line)
            })
            .collect();
        let list = List::new(items).block(
            Block::default()
                .borders(Borders::TOP)
                .border_style(Style::default().fg(PURPLE)),
        );
        frame.render_widget(list, layout[1]);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    fn press(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn typ(p: &mut CommandPalette, s: &str) {
        for c in s.chars() {
            p.on_key(press(KeyCode::Char(c)));
        }
    }

    fn render_to_string(p: &CommandPalette, w: u16, h: u16) -> String {
        let backend = TestBackend::new(w, h);
        let mut term = Terminal::new(backend).unwrap();
        term.draw(|f| p.render(f, f.area())).unwrap();
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
    fn fuzzy_filters_to_matching_commands() {
        let mut p = CommandPalette::new();
        // Empty query shows everything.
        assert_eq!(p.filtered().len(), super::super::command_registry::registry().len());
        // "seh" is a subsequence of "set-head".
        typ(&mut p, "seh");
        let names: Vec<&str> = p.filtered().iter().map(|c| c.name).collect();
        assert!(names.contains(&"set-head"));
        assert!(!names.contains(&"quit"));
    }

    #[test]
    fn down_up_move_selection_clamped() {
        let mut p = CommandPalette::new();
        typ(&mut p, "set"); // several matches
        assert_eq!(p.selected, 0);
        p.on_key(press(KeyCode::Down));
        assert_eq!(p.selected, 1);
        // Up past 0 clamps at 0.
        p.on_key(press(KeyCode::Up));
        p.on_key(press(KeyCode::Up));
        assert_eq!(p.selected, 0);
    }

    #[test]
    fn typing_resets_selection() {
        let mut p = CommandPalette::new();
        typ(&mut p, "set");
        p.on_key(press(KeyCode::Down));
        assert_eq!(p.selected, 1);
        typ(&mut p, "x"); // changes the filter
        assert_eq!(p.selected, 0);
    }

    #[test]
    fn enter_on_match_returns_submit_with_line() {
        let mut p = CommandPalette::new();
        typ(&mut p, "set-head 1030");
        match p.on_key(press(KeyCode::Enter)) {
            PaletteOutcome::Submit(line) => assert_eq!(line, "set-head 1030"),
            other => panic!("expected Submit, got {other:?}"),
        }
    }

    #[test]
    fn enter_on_bare_filter_completes_to_selected_name() {
        let mut p = CommandPalette::new();
        // "head" matches only set-head; Enter with no args submits its name.
        typ(&mut p, "head");
        match p.on_key(press(KeyCode::Enter)) {
            PaletteOutcome::Submit(line) => assert_eq!(line, "set-head"),
            other => panic!("expected Submit, got {other:?}"),
        }
    }

    #[test]
    fn esc_returns_cancel() {
        let mut p = CommandPalette::new();
        assert!(matches!(p.on_key(press(KeyCode::Esc)), PaletteOutcome::Cancel));
    }

    #[test]
    fn pending_keeps_palette_open() {
        let mut p = CommandPalette::new();
        assert!(matches!(p.on_key(press(KeyCode::Char('s'))), PaletteOutcome::Pending));
    }

    #[test]
    fn render_shows_name_description_and_rpc_label() {
        let mut p = CommandPalette::new();
        typ(&mut p, "set-head");
        let screen = render_to_string(&p, 80, 12);
        assert!(screen.contains("set-head"), "name visible:\n{screen}");
        assert!(screen.contains("Re-fork"), "description visible:\n{screen}");
        assert!(screen.contains("dev_setHead"), "rpc label visible:\n{screen}");
    }

    #[test]
    fn render_shows_local_tag_for_client_commands() {
        let mut p = CommandPalette::new();
        typ(&mut p, "set-baseline");
        let screen = render_to_string(&p, 80, 12);
        assert!(screen.contains("local"), "local tag visible:\n{screen}");
    }
}
