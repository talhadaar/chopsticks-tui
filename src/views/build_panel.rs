//! Build staging panel (MVP-2 P3): block metadata + a queue of staged
//! extrinsics. Pure UI — renders from its own state, returns a [`PanelAction`]
//! the app loop acts on. The actual block build is `Command::BuildWithQueue`,
//! emitted by the app loop when this panel returns [`PanelAction::Build`].

use crossterm::event::{KeyCode, KeyEvent};
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, List, ListItem, Paragraph};

use crate::contracts::PreparedTx;

/// Default block author when none is overridden.
pub const DEFAULT_AUTHOR: &str = "//Alice";
/// Default inter-block interval, in milliseconds (Polkadot's 6s slot).
pub const DEFAULT_BLOCK_INTERVAL_MS: u64 = 6_000;

/// What the app loop should do after a key. The panel never calls RPC.
///
/// `PartialEq` is hand-written (not derived) because `PreparedTx` is not `PartialEq`
/// — for `Build`, equality compares the queue *length* plus the metadata, which is
/// all the panel's tests assert and all the app loop branches on.
#[derive(Debug, Clone)]
pub enum PanelAction {
    /// Nothing to do; key consumed.
    None,
    /// Open the tx-builder in Stage mode to add an extrinsic (`a`).
    AddExtrinsic,
    /// Build one block from the current queue (`↵`).
    Build {
        queue: Vec<PreparedTx>,
        timestamp: Option<u64>,
        author: Option<String>,
    },
    /// Close the panel without building (`Esc`).
    Cancel,
}

impl PartialEq for PanelAction {
    fn eq(&self, other: &Self) -> bool {
        use PanelAction::*;
        match (self, other) {
            (None, None) => true,
            (AddExtrinsic, AddExtrinsic) => true,
            (Cancel, Cancel) => true,
            (
                Build { queue: q1, timestamp: t1, author: a1 },
                Build { queue: q2, timestamp: t2, author: a2 },
            ) => q1.len() == q2.len() && t1 == t2 && a1 == a2,
            _ => false,
        }
    }
}

/// Which field the metadata editor (`e`) is editing, if any.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EditField {
    Timestamp,
    Author,
}

/// The build staging panel.
pub struct BuildPanel {
    /// Staged extrinsics, in build order.
    queue: Vec<PreparedTx>,
    /// Cursor into `queue`.
    cursor: usize,
    /// Timestamp override in unix-ms; `None` = auto (`+DEFAULT_BLOCK_INTERVAL_MS`).
    timestamp: Option<u64>,
    /// Author override; `None` = `DEFAULT_AUTHOR`.
    author: Option<String>,
    /// Active metadata edit field + its text buffer, if editing (`e`).
    editing: Option<(EditField, String)>,
    /// The last field `e` targeted, so repeated `e` cycles Timestamp → Author.
    editing_last: Option<EditField>,
}

impl Default for BuildPanel {
    fn default() -> Self {
        Self::new()
    }
}

impl BuildPanel {
    pub fn new() -> Self {
        Self {
            queue: Vec::new(),
            cursor: 0,
            timestamp: None,
            author: None,
            editing: None,
            editing_last: None,
        }
    }

    /// Push a staged extrinsic (called by the app loop when the tx-builder, in
    /// Stage mode, yields a `PreparedTx`).
    pub fn push(&mut self, tx: PreparedTx) {
        self.queue.push(tx);
        self.cursor = self.queue.len() - 1;
    }

    /// Read-only view of the queue (for the app loop / tests).
    pub fn queue(&self) -> &[PreparedTx] {
        &self.queue
    }

    /// Handle one key, returning the action for the app loop.
    pub fn on_key(&mut self, key: KeyEvent) -> PanelAction {
        // Metadata edit sub-mode captures all keys until Enter/Esc.
        if let Some((field, buf)) = self.editing.as_mut() {
            match key.code {
                KeyCode::Enter => {
                    let value = buf.trim().to_string();
                    match field {
                        EditField::Timestamp => {
                            self.timestamp =
                                value.parse::<u64>().ok().filter(|_| !value.is_empty());
                        }
                        EditField::Author => {
                            self.author = if value.is_empty() { None } else { Some(value) };
                        }
                    }
                    self.editing = None;
                }
                KeyCode::Esc => self.editing = None,
                KeyCode::Backspace => {
                    buf.pop();
                }
                KeyCode::Char(c) => buf.push(c),
                _ => {}
            }
            return PanelAction::None;
        }

        match key.code {
            KeyCode::Esc => PanelAction::Cancel,
            KeyCode::Char('a') => PanelAction::AddExtrinsic,
            KeyCode::Char('x') => {
                if !self.queue.is_empty() {
                    self.queue.remove(self.cursor);
                    if self.cursor >= self.queue.len() && self.cursor > 0 {
                        self.cursor -= 1;
                    }
                }
                PanelAction::None
            }
            KeyCode::Char('e') => {
                // Cycle Timestamp → Author per successive `e` press.
                let next = match self.editing_last {
                    Some(EditField::Timestamp) => EditField::Author,
                    _ => EditField::Timestamp,
                };
                self.editing_last = Some(next);
                let seed = match next {
                    EditField::Timestamp => {
                        self.timestamp.map(|t| t.to_string()).unwrap_or_default()
                    }
                    EditField::Author => self.author.clone().unwrap_or_default(),
                };
                self.editing = Some((next, seed));
                PanelAction::None
            }
            KeyCode::Char('k') | KeyCode::Up => {
                self.cursor = self.cursor.saturating_sub(1);
                PanelAction::None
            }
            KeyCode::Char('j') | KeyCode::Down => {
                if self.cursor + 1 < self.queue.len() {
                    self.cursor += 1;
                }
                PanelAction::None
            }
            KeyCode::Enter => PanelAction::Build {
                queue: self.queue.clone(),
                timestamp: self.timestamp,
                author: self.author.clone(),
            },
            _ => PanelAction::None,
        }
    }

    /// Author to display / send, resolving the default.
    fn author_label(&self) -> &str {
        self.author.as_deref().unwrap_or(DEFAULT_AUTHOR)
    }

    /// One-line timestamp label.
    fn timestamp_label(&self) -> String {
        match self.timestamp {
            None => format!("+{}ms (auto)", DEFAULT_BLOCK_INTERVAL_MS),
            Some(t) => format!("{t} (override)"),
        }
    }

    /// Draw the panel into `area`. Pure: reads state only.
    pub fn render(&self, frame: &mut Frame, area: Rect) {
        let block = Block::default()
            .borders(Borders::ALL)
            .title("Build block")
            .border_style(Style::default().fg(Color::Magenta)); // purple = dev surface

        let inner = block.inner(area);
        frame.render_widget(block, area);

        let mut lines: Vec<Line> = Vec::new();
        // Metadata header.
        lines.push(Line::from(vec![
            Span::raw("timestamp "),
            Span::styled(self.timestamp_label(), Style::default().fg(Color::Cyan)),
            Span::raw("   author "),
            Span::styled(self.author_label().to_string(), Style::default().fg(Color::Cyan)),
        ]));
        if let Some((field, buf)) = &self.editing {
            lines.push(Line::from(Span::styled(
                format!("editing {field:?}: {buf}▏"),
                Style::default().fg(Color::Yellow),
            )));
        }
        lines.push(Line::from(format!("queued extrinsics — {}", self.queue.len())));

        // Queue rows.
        let items: Vec<ListItem> = self
            .queue
            .iter()
            .enumerate()
            .map(|(i, tx)| {
                let marker = if i == self.cursor { "> " } else { "  " };
                let label = format!("{marker}{}. {}.{}", i + 1, tx.pallet, tx.call);
                let mut style = Style::default();
                if i == self.cursor {
                    style = style.fg(Color::Magenta).add_modifier(Modifier::BOLD);
                }
                ListItem::new(Line::from(Span::styled(label, style)))
            })
            .collect();

        let help = Line::from(Span::styled(
            "a add  x drop  e edit meta  ↵ build  esc cancel",
            Style::default().fg(Color::DarkGray),
        ));

        // Layout: header lines, queue list, help.
        let header_h = lines.len() as u16;
        use ratatui::layout::{Constraint, Layout};
        let chunks = Layout::vertical([
            Constraint::Length(header_h),
            Constraint::Min(1),
            Constraint::Length(1),
        ])
        .split(inner);
        frame.render_widget(Paragraph::new(lines), chunks[0]);
        frame.render_widget(List::new(items), chunks[1]);
        frame.render_widget(Paragraph::new(help), chunks[2]);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::contracts::{DevAccount, PreparedTx, TxSigner};
    use crossterm::event::{KeyEvent, KeyEventKind, KeyEventState, KeyModifiers};
    use scale_value::Value;

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent {
            code,
            modifiers: KeyModifiers::NONE,
            kind: KeyEventKind::Press,
            state: KeyEventState::NONE,
        }
    }

    fn tx(call: &str) -> PreparedTx {
        PreparedTx {
            pallet: "Balances".into(),
            call: call.into(),
            args: vec![Value::u128(1)],
            signer: TxSigner::Dev(DevAccount::Alice),
            encoded_preview: String::new(),
        }
    }

    #[test]
    fn push_appends_and_moves_cursor_to_new_item() {
        let mut p = BuildPanel::new();
        p.push(tx("transfer_keep_alive"));
        p.push(tx("remark"));
        assert_eq!(p.queue().len(), 2);
        assert_eq!(p.cursor, 1);
    }

    #[test]
    fn a_requests_add_extrinsic() {
        let mut p = BuildPanel::new();
        assert_eq!(p.on_key(key(KeyCode::Char('a'))), PanelAction::AddExtrinsic);
    }

    #[test]
    fn x_drops_selected_and_clamps_cursor() {
        let mut p = BuildPanel::new();
        p.push(tx("a"));
        p.push(tx("b"));
        assert_eq!(p.cursor, 1);
        p.on_key(key(KeyCode::Char('x'))); // drop "b"
        assert_eq!(p.queue().len(), 1);
        assert_eq!(p.cursor, 0);
        assert_eq!(p.queue()[0].call, "a");
    }

    #[test]
    fn jk_moves_cursor_within_bounds() {
        let mut p = BuildPanel::new();
        p.push(tx("a"));
        p.push(tx("b"));
        p.on_key(key(KeyCode::Char('k'))); // up: 1 -> 0
        assert_eq!(p.cursor, 0);
        p.on_key(key(KeyCode::Char('k'))); // clamp at 0
        assert_eq!(p.cursor, 0);
        p.on_key(key(KeyCode::Char('j'))); // down: 0 -> 1
        assert_eq!(p.cursor, 1);
        p.on_key(key(KeyCode::Char('j'))); // clamp at last
        assert_eq!(p.cursor, 1);
    }

    #[test]
    fn enter_emits_build_with_queue_and_metadata() {
        let mut p = BuildPanel::new();
        p.push(tx("transfer_keep_alive"));
        match p.on_key(key(KeyCode::Enter)) {
            PanelAction::Build { queue, timestamp, author } => {
                assert_eq!(queue.len(), 1);
                assert_eq!(queue[0].call, "transfer_keep_alive");
                assert_eq!(timestamp, None); // auto by default
                assert_eq!(author, None); // default //Alice
            }
            other => panic!("expected Build, got {other:?}"),
        }
    }

    #[test]
    fn esc_cancels() {
        let mut p = BuildPanel::new();
        assert_eq!(p.on_key(key(KeyCode::Esc)), PanelAction::Cancel);
    }

    #[test]
    fn edit_author_overrides_default() {
        let mut p = BuildPanel::new();
        p.on_key(key(KeyCode::Char('e'))); // enter edit (Timestamp first)
        p.on_key(key(KeyCode::Enter)); // commit timestamp (empty -> None)
        p.on_key(key(KeyCode::Char('e'))); // now Author
        for c in "//Bob".chars() {
            p.on_key(key(KeyCode::Char(c)));
        }
        p.on_key(key(KeyCode::Enter)); // commit
        assert_eq!(p.author_label(), "//Bob");
    }

    #[test]
    fn edit_timestamp_parses_override() {
        let mut p = BuildPanel::new();
        p.on_key(key(KeyCode::Char('e'))); // Timestamp
        for c in "12345".chars() {
            p.on_key(key(KeyCode::Char(c)));
        }
        p.on_key(key(KeyCode::Enter));
        match p.on_key(key(KeyCode::Enter)) {
            PanelAction::Build { timestamp, .. } => assert_eq!(timestamp, Some(12345)),
            other => panic!("expected Build, got {other:?}"),
        }
    }

    #[test]
    fn renders_without_panicking() {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;
        let mut p = BuildPanel::new();
        p.push(tx("transfer_keep_alive"));
        let backend = TestBackend::new(80, 20);
        let mut term = Terminal::new(backend).unwrap();
        term.draw(|f| p.render(f, f.area())).unwrap();
        let buf = term.backend().buffer().clone();
        let dump: String = buf.content().iter().map(|c| c.symbol()).collect();
        assert!(dump.contains("Build block"));
        assert!(dump.contains("queued extrinsics"));
    }
}
