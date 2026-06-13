//! Sessions list overlay (spec §8): browse/filter saved sessions and choose an
//! action (restore / save-current / rename / delete). Pure UI — it renders from
//! an injected `Vec<SessionSummary>` and returns a `SessionsAction`; the app loop
//! owns the actual IO and `Command` dispatch.

use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Clear, List, ListItem, Paragraph};

use crate::session::{SessionSource, SessionSummary, format_age};

/// Case-insensitive subsequence ("fuzzy") match — same rule as the storage
/// picker (`views::picker::fuzzy_match`), duplicated here to keep the views
/// independent. An empty needle matches everything.
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

/// What the app loop should do in response to a key in the sessions overlay.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionsAction {
    /// Restore (re-fork + replay) the named session.
    Restore(String),
    /// Save the current state as a new session (the loop prompts/derives a name).
    SaveCurrent,
    /// Rename a session on disk.
    Rename { from: String, to: String },
    /// Delete the named session.
    Delete(String),
    /// Close the overlay (Esc).
    Close,
}

/// Sub-mode: browsing the list (verbs active), typing a fuzzy filter, or typing a
/// new name in the rename field.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Stage {
    Browse,
    Filter,
    Rename,
}

/// The sessions list overlay state.
pub struct SessionsView {
    sessions: Vec<SessionSummary>,
    /// Fuzzy filter query (INSERT-in-overlay, per spec §2.1).
    query: String,
    /// Cursor into the *filtered* list.
    cursor: usize,
    stage: Stage,
    /// Rename buffer + the original name being renamed.
    rename_buf: String,
    rename_from: String,
}

impl SessionsView {
    /// Build the overlay over a snapshot of saved-session summaries.
    pub fn new(sessions: Vec<SessionSummary>) -> Self {
        Self {
            sessions,
            query: String::new(),
            cursor: 0,
            stage: Stage::Browse,
            rename_buf: String::new(),
            rename_from: String::new(),
        }
    }

    /// Sessions surviving the current fuzzy query.
    pub fn filtered(&self) -> Vec<&SessionSummary> {
        self.sessions
            .iter()
            .filter(|s| fuzzy_match(&s.name, &self.query))
            .collect()
    }

    fn current_name(&self) -> Option<String> {
        self.filtered().get(self.cursor).map(|s| s.name.clone())
    }

    /// Feed a key; returns `Some(action)` when the loop should act.
    pub fn on_key(&mut self, key: crossterm::event::KeyEvent) -> Option<SessionsAction> {
        use crossterm::event::KeyEventKind;
        if key.kind == KeyEventKind::Release {
            return None;
        }
        match self.stage {
            Stage::Rename => self.on_key_rename(key.code),
            Stage::Filter => self.on_key_filter(key.code),
            Stage::Browse => self.on_key_browse(key.code),
        }
    }

    fn on_key_browse(&mut self, code: crossterm::event::KeyCode) -> Option<SessionsAction> {
        use crossterm::event::KeyCode;
        match code {
            KeyCode::Esc => return Some(SessionsAction::Close),
            KeyCode::Enter => return self.current_name().map(SessionsAction::Restore),
            KeyCode::Char('s') => return Some(SessionsAction::SaveCurrent),
            KeyCode::Char('d') => {
                if let Some(name) = self.current_name() {
                    return Some(SessionsAction::Delete(name));
                }
            }
            KeyCode::Char('r') => {
                if let Some(name) = self.current_name() {
                    self.stage = Stage::Rename;
                    self.rename_from = name;
                    // Start from an empty buffer so the user types the new name
                    // fresh (the original is shown via `rename_from` context).
                    self.rename_buf = String::new();
                }
            }
            // `/` enters the fuzzy-filter input (hint bar: "/ filter"). Browse-mode
            // bare keys are verbs (s/d/r), so filtering needs an explicit trigger.
            KeyCode::Char('/') => self.stage = Stage::Filter,
            KeyCode::Up => self.cursor = self.cursor.saturating_sub(1),
            KeyCode::Down => {
                let max = self.filtered().len().saturating_sub(1);
                self.cursor = (self.cursor + 1).min(max);
            }
            _ => {}
        }
        None
    }

    fn on_key_filter(&mut self, code: crossterm::event::KeyCode) -> Option<SessionsAction> {
        use crossterm::event::KeyCode;
        match code {
            // Esc/Enter leave the filter input (the narrowed list stays applied).
            KeyCode::Esc | KeyCode::Enter => self.stage = Stage::Browse,
            KeyCode::Backspace => {
                self.query.pop();
                self.cursor = 0;
            }
            KeyCode::Char(c) => {
                self.query.push(c);
                self.cursor = 0;
            }
            _ => {}
        }
        None
    }

    fn on_key_rename(&mut self, code: crossterm::event::KeyCode) -> Option<SessionsAction> {
        use crossterm::event::KeyCode;
        match code {
            KeyCode::Esc => {
                self.stage = Stage::Browse;
            }
            KeyCode::Backspace => {
                self.rename_buf.pop();
            }
            KeyCode::Char(c) => self.rename_buf.push(c),
            KeyCode::Enter => {
                let action = SessionsAction::Rename {
                    from: self.rename_from.clone(),
                    to: self.rename_buf.trim().to_string(),
                };
                self.stage = Stage::Browse;
                return Some(action);
            }
            _ => {}
        }
        None
    }

    /// Draw the centered modal (purple border, per spec §8).
    pub fn render(&self, frame: &mut Frame, area: Rect) {
        frame.render_widget(Clear, area);
        let purple = Style::default().fg(Color::Magenta);
        let amber = Style::default().fg(Color::Yellow);

        let rows = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(3),
                Constraint::Min(1),
                Constraint::Length(1),
            ])
            .split(area);

        // Filter / rename box.
        let header = match self.stage {
            Stage::Rename => format!("rename {} → {}", self.rename_from, self.rename_buf),
            _ => format!("filter: {}", self.query),
        };
        frame.render_widget(
            Paragraph::new(Line::from(header))
                .block(Block::bordered().border_style(purple).title("Sessions")),
            rows[0],
        );

        // List.
        let filtered = self.filtered();
        let items: Vec<ListItem> = filtered
            .iter()
            .enumerate()
            .map(|(i, s)| {
                let (tag, tag_style) = match s.source {
                    SessionSource::Manual => ("manual", Style::default()),
                    SessionSource::AutoSnapshot => ("⏳ auto", amber),
                };
                let line = Line::from(vec![
                    Span::raw(format!("{:<24} ", s.name)),
                    Span::raw(format!("#{:<6} ", s.head)),
                    Span::raw(format!("{} pins  ", s.pin_count)),
                    Span::styled(format!("{:<8}", tag), tag_style),
                    Span::raw(format_age(s.age_secs)),
                ]);
                let style = if i == self.cursor {
                    Style::default().add_modifier(Modifier::REVERSED)
                } else {
                    Style::default()
                };
                ListItem::new(line).style(style)
            })
            .collect();
        frame.render_widget(List::new(items), rows[1]);

        // Hint bar.
        frame.render_widget(
            Paragraph::new("↵ restore  s save  r rename  d delete  / filter  esc"),
            rows[2],
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::{SessionSource, SessionSummary};
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    fn summaries() -> Vec<SessionSummary> {
        vec![
            SessionSummary {
                name: "before-upgrade".into(),
                head: 1042,
                pin_count: 7,
                source: SessionSource::Manual,
                age_secs: 120,
            },
            SessionSummary {
                name: "drained-treasury".into(),
                head: 1030,
                pin_count: 5,
                source: SessionSource::Manual,
                age_secs: 900,
            },
            SessionSummary {
                name: "timeline-#1045".into(),
                head: 1045,
                pin_count: 7,
                source: SessionSource::AutoSnapshot,
                age_secs: 1,
            },
        ]
    }

    fn press(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    #[test]
    fn enter_restores_highlighted_session() {
        let mut v = SessionsView::new(summaries());
        let action = v.on_key(press(KeyCode::Enter));
        assert_eq!(action, Some(SessionsAction::Restore("before-upgrade".into())));
    }

    #[test]
    fn fuzzy_filter_narrows_then_restores() {
        let mut v = SessionsView::new(summaries());
        // `/` enters filter input; bare verbs (s/d/r) stay reserved in Browse, so
        // filtering needs the explicit trigger (hint bar: "/ filter").
        assert_eq!(v.on_key(press(KeyCode::Char('/'))), None);
        for c in "drain".chars() {
            assert_eq!(v.on_key(press(KeyCode::Char(c))), None);
        }
        assert_eq!(v.filtered().len(), 1);
        // Enter leaves the filter input (list stays narrowed)…
        assert_eq!(v.on_key(press(KeyCode::Enter)), None);
        // …then Enter in Browse restores the single survivor.
        assert_eq!(
            v.on_key(press(KeyCode::Enter)),
            Some(SessionsAction::Restore("drained-treasury".into()))
        );
    }

    #[test]
    fn s_saves_current_d_deletes_esc_closes() {
        let mut v = SessionsView::new(summaries());
        assert_eq!(v.on_key(press(KeyCode::Char('s'))), Some(SessionsAction::SaveCurrent));
        assert_eq!(
            v.on_key(press(KeyCode::Char('d'))),
            Some(SessionsAction::Delete("before-upgrade".into()))
        );
        assert_eq!(v.on_key(press(KeyCode::Esc)), Some(SessionsAction::Close));
    }

    #[test]
    fn r_enters_rename_mode_and_enter_confirms() {
        let mut v = SessionsView::new(summaries());
        assert_eq!(v.on_key(press(KeyCode::Char('r'))), None); // enters rename
        for c in "x".chars() {
            v.on_key(press(KeyCode::Char(c)));
        }
        assert_eq!(
            v.on_key(press(KeyCode::Enter)),
            Some(SessionsAction::Rename { from: "before-upgrade".into(), to: "x".into() })
        );
    }

    #[test]
    fn renders_rows_with_source_and_age_columns() {
        let v = SessionsView::new(summaries());
        let backend = TestBackend::new(80, 12);
        let mut term = Terminal::new(backend).unwrap();
        term.draw(|f| v.render(f, f.area())).unwrap();
        let buf = term.backend().buffer().clone();
        let text: String = buf.content().iter().map(|c| c.symbol()).collect();
        assert!(text.contains("before-upgrade"));
        assert!(text.contains("#1042"));
        assert!(text.contains("auto")); // the amber auto-snap tag
        assert!(text.contains("now")); // age of timeline-#1045
    }
}
