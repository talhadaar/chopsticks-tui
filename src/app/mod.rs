//! Application orchestration: `AppState`, the event loop, and `Command`
//! dispatch to async tasks (ticket T14).

pub mod resilience;

use crossterm::event::{self, Event, KeyCode, KeyEventKind};
use ratatui::DefaultTerminal;
use ratatui::widgets::{Block, Paragraph};

use crate::contracts::Result;

/// Run the application event loop. (Placeholder — draws a single scaffold frame
/// and waits for `q`/`Esc`. The real `Command`/`Event` loop lands in T14; the
/// terminal lifecycle + panic guard wrap this call in `main`.)
pub fn run(mut terminal: DefaultTerminal) -> Result<()> {
    loop {
        terminal.draw(|frame| {
            let body = Paragraph::new("chopsticks-tui — T01 scaffold. Press q or Esc to quit.")
                .block(Block::bordered().title("chopsticks-tui"));
            frame.render_widget(body, frame.area());
        })?;

        if let Event::Key(key) = event::read()?
            && key.kind == KeyEventKind::Press
            && matches!(key.code, KeyCode::Char('q') | KeyCode::Esc)
        {
            break;
        }
    }
    Ok(())
}
