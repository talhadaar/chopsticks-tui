//! Connection/startup screen: spawn-vs-attach config input and the streamed
//! Chopsticks boot log; transitions to the grid on connect (ticket T13).
//!
//! This view owns only its form + log-buffer state. It renders from that state
//! and emits a single [`Command::Connect`] when the user submits the form; it
//! never touches RPC. Streamed [`Event::BootLog`] lines are appended to a
//! scrollable pane, and [`Event::Connected`] flips a "ready" indicator (the
//! actual swap to the grid is T14's responsibility).

use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph, Wrap};

use crate::contracts::{BuildMode, Command, Event, ForkConfig};

/// Which kind of fork the user is configuring.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// Spawn a fresh Chopsticks process from a chain name or YAML path.
    Spawn,
    /// Attach to an already-running fork via its ws endpoint.
    Attach,
}

/// Connection / startup screen state.
///
/// Holds the two form fields (one per mode), the streamed boot log with its
/// scroll offset, the last spawn error (rendered inline), and the "ready"
/// indicator that lights up once metadata has loaded.
#[derive(Debug, Clone)]
pub struct ConnectionView {
    mode: Mode,
    /// Spawn-mode input: a chain name (e.g. `polkadot`) or a path to a YAML.
    spawn_input: String,
    /// Attach-mode input: a `ws://` endpoint URL.
    attach_input: String,
    /// Streamed boot-log lines, oldest first.
    log: Vec<String>,
    /// Number of lines scrolled away from the bottom (0 = pinned to newest).
    scroll_back: u16,
    /// Set once `Event::Connected { metadata_ready: true, .. }` arrives.
    ready: bool,
    /// Inline error hint surfaced from `Event::Error` / `Event::Disconnected`.
    error: Option<String>,
}

impl Default for ConnectionView {
    fn default() -> Self {
        Self {
            mode: Mode::Spawn,
            spawn_input: String::new(),
            attach_input: String::new(),
            log: Vec::new(),
            scroll_back: 0,
            ready: false,
            error: None,
        }
    }
}

impl ConnectionView {
    /// Create an empty connection view in spawn mode.
    pub fn new() -> Self {
        Self::default()
    }

    /// Whether the fork is connected with metadata loaded — T14 reads this to
    /// decide when to swap in the grid.
    pub fn is_ready(&self) -> bool {
        self.ready
    }

    /// Current form mode (spawn vs attach). Exposed for tests/host wiring.
    pub fn mode(&self) -> Mode {
        self.mode
    }

    /// The active input field for the current mode.
    fn input_mut(&mut self) -> &mut String {
        match self.mode {
            Mode::Spawn => &mut self.spawn_input,
            Mode::Attach => &mut self.attach_input,
        }
    }

    fn input(&self) -> &str {
        match self.mode {
            Mode::Spawn => &self.spawn_input,
            Mode::Attach => &self.attach_input,
        }
    }

    /// Build the `Command::Connect` for the current form, if the input is valid.
    ///
    /// MVP-1 fixes `build_mode = Manual` and enables `mock_signature_host` for
    /// spawn. Returns `None` (and sets an inline error) when the field is empty
    /// or, in attach mode, when the URL is not a websocket endpoint.
    fn submit(&mut self) -> Option<Command> {
        let value = self.input().trim().to_string();
        if value.is_empty() {
            self.error = Some(match self.mode {
                Mode::Spawn => "enter a chain name or a path to a YAML config".to_string(),
                Mode::Attach => "enter a ws:// endpoint URL".to_string(),
            });
            return None;
        }

        match self.mode {
            Mode::Spawn => {
                self.error = None;
                Some(Command::Connect(ForkConfig::Spawn {
                    chain_or_path: value,
                    build_mode: BuildMode::Manual,
                    mock_signature_host: true,
                }))
            }
            Mode::Attach => {
                if !(value.starts_with("ws://") || value.starts_with("wss://")) {
                    self.error =
                        Some("endpoint must start with ws:// or wss:// (e.g. ws://localhost:8000)"
                            .to_string());
                    return None;
                }
                self.error = None;
                Some(Command::Connect(ForkConfig::Attach { url: value }))
            }
        }
    }

    /// Handle a key event; returns a `Command` when the user submits the form.
    ///
    /// Bindings: `Tab` toggles spawn/attach, `Enter` submits, `Backspace`
    /// edits, `Up`/`Down` (or `PageUp`/`PageDown`) scroll the log, and printable
    /// characters are appended to the active field.
    pub fn on_key(&mut self, key: KeyEvent) -> Option<Command> {
        // Ignore key *releases*; we only act on presses (crossterm on some
        // platforms emits both).
        if key.kind == KeyEventKind::Release {
            return None;
        }

        match key.code {
            KeyCode::Tab => {
                self.mode = match self.mode {
                    Mode::Spawn => Mode::Attach,
                    Mode::Attach => Mode::Spawn,
                };
                self.error = None;
                None
            }
            KeyCode::Enter => self.submit(),
            KeyCode::Backspace => {
                self.input_mut().pop();
                None
            }
            KeyCode::Up => {
                self.scroll_back = self.scroll_back.saturating_add(1);
                None
            }
            KeyCode::Down => {
                self.scroll_back = self.scroll_back.saturating_sub(1);
                None
            }
            KeyCode::PageUp => {
                self.scroll_back = self.scroll_back.saturating_add(10);
                None
            }
            KeyCode::PageDown => {
                self.scroll_back = self.scroll_back.saturating_sub(10);
                None
            }
            KeyCode::Char(c)
                if !key
                    .modifiers
                    .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
            {
                self.input_mut().push(c);
                None
            }
            _ => None,
        }
    }

    /// Fold an async event into the view: append boot-log lines, light the
    /// ready indicator on connect, and surface errors inline. Never panics.
    pub fn on_event(&mut self, event: &Event) {
        match event {
            Event::BootLog(line) => {
                self.log.push(line.clone());
                // Stay pinned to the newest line unless the user scrolled up.
                if self.scroll_back != 0 {
                    self.scroll_back = self.scroll_back.saturating_add(1);
                }
            }
            Event::Connected { metadata_ready, .. } => {
                if *metadata_ready {
                    self.ready = true;
                    self.error = None;
                }
            }
            Event::Disconnected(reason) => {
                self.ready = false;
                self.error = Some(format!("disconnected: {reason}"));
            }
            Event::Error(msg) => {
                self.error = Some(msg.clone());
            }
            // Grid/tx events are not this view's concern.
            Event::NewColumn(_) | Event::TxResult(_) => {}
        }
    }

    /// Render the form, status line, and scrollable boot-log pane into `area`.
    pub fn render(&self, frame: &mut Frame, area: Rect) {
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(3), // form / input
                Constraint::Length(1), // status / error line
                Constraint::Min(3),    // boot log
            ])
            .split(area);

        self.render_form(frame, chunks[0]);
        self.render_status(frame, chunks[1]);
        self.render_log(frame, chunks[2]);
    }

    fn render_form(&self, frame: &mut Frame, area: Rect) {
        let (title, prompt) = match self.mode {
            Mode::Spawn => ("Spawn fork", "chain or YAML path"),
            Mode::Attach => ("Attach fork", "ws:// endpoint"),
        };

        let line = Line::from(vec![
            Span::styled(
                format!("{prompt}: "),
                Style::default().add_modifier(Modifier::DIM),
            ),
            Span::raw(self.input()),
            // A simple block cursor.
            Span::styled(" ", Style::default().add_modifier(Modifier::REVERSED)),
        ]);

        let hint = match self.mode {
            Mode::Spawn => "Tab: attach mode · Enter: connect",
            Mode::Attach => "Tab: spawn mode · Enter: connect",
        };

        let block = Block::default()
            .borders(Borders::ALL)
            .title(Span::styled(
                format!(" {title} "),
                Style::default().add_modifier(Modifier::BOLD),
            ))
            .title_bottom(Line::from(Span::styled(
                format!(" {hint} "),
                Style::default().add_modifier(Modifier::DIM),
            )));

        frame.render_widget(Paragraph::new(line).block(block), area);
    }

    fn render_status(&self, frame: &mut Frame, area: Rect) {
        let line = if let Some(err) = &self.error {
            Line::from(vec![
                Span::styled(
                    "error: ",
                    Style::default()
                        .fg(Color::Red)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled(err.clone(), Style::default().fg(Color::Red)),
            ])
        } else if self.ready {
            Line::from(Span::styled(
                "ready / entering grid\u{2026}",
                Style::default()
                    .fg(Color::Green)
                    .add_modifier(Modifier::BOLD),
            ))
        } else {
            Line::from(Span::styled(
                "idle \u{2014} configure a fork and press Enter",
                Style::default().add_modifier(Modifier::DIM),
            ))
        };
        frame.render_widget(Paragraph::new(line), area);
    }

    fn render_log(&self, frame: &mut Frame, area: Rect) {
        let block = Block::default()
            .borders(Borders::ALL)
            .title(Span::styled(" Boot log ", Style::default().add_modifier(Modifier::BOLD)));

        // Visible height inside the borders.
        let inner_height = area.height.saturating_sub(2);
        let total = self.log.len() as u16;
        // Bottom-anchored: the offset that shows the newest lines, then back off
        // by the user's scroll amount (clamped so we never scroll past the top).
        let max_top = total.saturating_sub(inner_height);
        let scroll_back = self.scroll_back.min(max_top);
        let top = max_top.saturating_sub(scroll_back);

        let lines: Vec<Line> = self
            .log
            .iter()
            .map(|l| Line::from(l.as_str()))
            .collect();

        let paragraph = Paragraph::new(lines)
            .block(block)
            .wrap(Wrap { trim: false })
            .scroll((top, 0));

        frame.render_widget(paragraph, area);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::contracts::RenderCtx;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    fn press(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::empty())
    }

    fn type_str(view: &mut ConnectionView, s: &str) {
        for c in s.chars() {
            view.on_key(press(KeyCode::Char(c)));
        }
    }

    /// Flatten the rendered `TestBackend` buffer into one string so tests can
    /// assert on visible text without caring about cell coordinates.
    fn render_to_string(view: &ConnectionView, w: u16, h: u16) -> String {
        let backend = TestBackend::new(w, h);
        let mut terminal = Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| view.render(frame, frame.area()))
            .unwrap();
        let buffer = terminal.backend().buffer().clone();
        let mut out = String::new();
        for y in 0..buffer.area.height {
            for x in 0..buffer.area.width {
                out.push_str(buffer[(x, y)].symbol());
            }
            out.push('\n');
        }
        out
    }

    fn ctx() -> RenderCtx {
        RenderCtx {
            ss58_prefix: 42,
            token_decimals: 12,
            token_symbol: "UNIT".to_string(),
        }
    }

    #[test]
    fn spawn_form_emits_connect_spawn_with_manual_mode() {
        let mut view = ConnectionView::new();
        assert_eq!(view.mode(), Mode::Spawn);

        type_str(&mut view, "polkadot");
        let cmd = view.on_key(press(KeyCode::Enter)).expect("should connect");

        match cmd {
            Command::Connect(ForkConfig::Spawn {
                chain_or_path,
                build_mode,
                mock_signature_host,
            }) => {
                assert_eq!(chain_or_path, "polkadot");
                assert_eq!(build_mode, BuildMode::Manual);
                assert!(mock_signature_host, "MVP-1 enables mock signature host");
            }
            other => panic!("expected Connect(Spawn), got {other:?}"),
        }
    }

    #[test]
    fn attach_form_emits_connect_attach_with_url() {
        let mut view = ConnectionView::new();
        // Toggle to attach mode.
        view.on_key(press(KeyCode::Tab));
        assert_eq!(view.mode(), Mode::Attach);

        type_str(&mut view, "ws://localhost:8000");
        let cmd = view.on_key(press(KeyCode::Enter)).expect("should connect");

        match cmd {
            Command::Connect(ForkConfig::Attach { url }) => {
                assert_eq!(url, "ws://localhost:8000");
            }
            other => panic!("expected Connect(Attach), got {other:?}"),
        }
    }

    #[test]
    fn boot_log_lines_append_and_render() {
        let mut view = ConnectionView::new();
        view.on_event(&Event::BootLog("loading chain spec".to_string()));
        view.on_event(&Event::BootLog("listening on ws://localhost:8000".to_string()));

        let screen = render_to_string(&view, 60, 12);
        assert!(
            screen.contains("loading chain spec"),
            "first log line should render:\n{screen}"
        );
        assert!(
            screen.contains("listening on ws://localhost:8000"),
            "second log line should render:\n{screen}"
        );
    }

    #[test]
    fn connected_event_sets_ready_indicator() {
        let mut view = ConnectionView::new();
        assert!(!view.is_ready());

        view.on_event(&Event::Connected {
            metadata_ready: true,
            ctx: ctx(),
        });

        assert!(view.is_ready(), "ready flag should be set");
        let screen = render_to_string(&view, 60, 12);
        assert!(
            screen.contains("ready") && screen.contains("entering grid"),
            "ready indicator should render:\n{screen}"
        );
    }

    #[test]
    fn connected_without_metadata_does_not_set_ready() {
        let mut view = ConnectionView::new();
        view.on_event(&Event::Connected {
            metadata_ready: false,
            ctx: ctx(),
        });
        assert!(!view.is_ready());
    }

    #[test]
    fn spawn_error_renders_inline_hint() {
        let mut view = ConnectionView::new();
        // Submit with an empty field → inline error, no command.
        let cmd = view.on_key(press(KeyCode::Enter));
        assert!(cmd.is_none(), "empty submit should not connect");

        let screen = render_to_string(&view, 70, 12);
        assert!(
            screen.contains("error:"),
            "inline error prefix should render:\n{screen}"
        );
        assert!(
            screen.contains("chain name") || screen.contains("YAML"),
            "error should hint at the expected input:\n{screen}"
        );
    }

    #[test]
    fn attach_rejects_non_ws_url_with_hint() {
        let mut view = ConnectionView::new();
        view.on_key(press(KeyCode::Tab));
        type_str(&mut view, "http://nope");
        let cmd = view.on_key(press(KeyCode::Enter));
        assert!(cmd.is_none(), "non-ws url should not connect");

        let screen = render_to_string(&view, 70, 12);
        assert!(
            screen.contains("ws://"),
            "should hint the ws:// scheme:\n{screen}"
        );
    }

    #[test]
    fn error_event_surfaces_inline() {
        let mut view = ConnectionView::new();
        view.on_event(&Event::Error("spawn failed: chopsticks not found".to_string()));
        let screen = render_to_string(&view, 70, 12);
        assert!(
            screen.contains("spawn failed"),
            "Event::Error should surface inline:\n{screen}"
        );
    }

    #[test]
    fn backspace_edits_active_field() {
        let mut view = ConnectionView::new();
        type_str(&mut view, "polkadotX");
        view.on_key(press(KeyCode::Backspace));
        let cmd = view.on_key(press(KeyCode::Enter)).unwrap();
        match cmd {
            Command::Connect(ForkConfig::Spawn { chain_or_path, .. }) => {
                assert_eq!(chain_or_path, "polkadot");
            }
            other => panic!("expected Spawn, got {other:?}"),
        }
    }
}
