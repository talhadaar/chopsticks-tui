//! Application orchestration: `AppState`, the async event loop, and `Command`
//! dispatch to background tasks (ticket T14).
//!
//! The UI loop owns a single `AppState`, mutated only here (no locks in the render
//! path). Background tokio tasks talk to the loop over `Command` (UI→tasks) and
//! `Event` (tasks→UI) channels. The connected chain client is leaked to `'static`
//! — it is a process-lifetime singleton, and that lets the borrowing overlays
//! (storage picker / tx builder) be held across frames without self-referential
//! lifetime gymnastics.

pub mod input;
pub mod resilience;

use std::collections::VecDeque;
use std::sync::Arc;

use crossterm::event::{Event as CtEvent, KeyCode, KeyEvent, KeyEventKind};
use ratatui::DefaultTerminal;
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::widgets::{Block, Clear, Paragraph};
use tokio::sync::mpsc;

use crate::chain::client::SubxtChainClient;
use crate::chain::storage_catalog::MetadataCatalog;
use crate::chain::{dev_rpc, storage_fetch};
use crate::chopsticks::Supervisor;
use crate::contracts::{
    BlockColumn, CellDiff, CellState, ChainClient, ChopsticksSupervisor, Command, Event, PinnedItem,
    PinnedItemId, RenderCtx, Result,
};
use crate::render::diff::diff_columns;
use crate::render::value::DefaultRenderer;
use crate::views::connection::ConnectionView;
use crate::views::grid::{GridCell, GridRow, GridView, GridViewModel};
use crate::views::picker::StoragePicker;
use crate::views::tx_builder::{ArgKind, ArgSpec, CallCatalog, TxBuilder};

/// Bounded grid history: the ring buffer keeps the last `MAX_COLUMNS` blocks.
pub const MAX_COLUMNS: usize = 256;
/// How many columns are visible in the grid window at once.
const VISIBLE_COLS: usize = 12;

/// Which screen the app is showing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    /// Connection / startup screen.
    Connecting,
    /// The live storage grid.
    Grid,
}

/// The whole UI state. Mutated only by the event loop.
pub struct AppState {
    pub phase: Phase,
    pub connection: ConnectionView,
    pub ctx: RenderCtx,
    /// Pinned items = grid rows, in display order.
    pub pinned: Vec<PinnedItem>,
    /// Block columns, oldest at front, newest at back (ring buffer).
    pub columns: VecDeque<BlockColumn>,
    /// Auto-follow the chain tip.
    pub follow: bool,
    /// Columns scrolled back from the newest (0 = at tip).
    pub col_offset: usize,
    /// First visible row.
    pub row_scroll: usize,
    /// Last transaction result, shown in a panel.
    pub tx_result: Option<crate::contracts::TxOutcome>,
    /// Disconnect / error banner (grid freezes, keeps last data).
    pub banner: Option<String>,
    /// Fuzzy row filter on the pinned label (`[/]`).
    pub filter: String,
    pub should_quit: bool,
    next_pin_id: u64,
}

impl Default for AppState {
    fn default() -> Self {
        Self::new()
    }
}

impl AppState {
    pub fn new() -> Self {
        Self {
            phase: Phase::Connecting,
            connection: ConnectionView::new(),
            ctx: default_ctx(),
            pinned: Vec::new(),
            columns: VecDeque::new(),
            follow: true,
            col_offset: 0,
            row_scroll: 0,
            tx_result: None,
            banner: None,
            filter: String::new(),
            should_quit: false,
            next_pin_id: 1,
        }
    }

    /// Fold a task `Event` into the state.
    pub fn on_event(&mut self, ev: Event) {
        match &ev {
            Event::BootLog(_) => self.connection.on_event(&ev),
            Event::Connected { metadata_ready, ctx } => {
                self.ctx = ctx.clone();
                self.connection.on_event(&ev);
                if *metadata_ready {
                    self.phase = Phase::Grid;
                    self.banner = None;
                }
            }
            Event::NewColumn(col) => self.push_column(col.clone()),
            Event::TxResult(outcome) => self.tx_result = Some(outcome.clone()),
            Event::Disconnected(msg) => {
                // Freeze the grid (keep last data); just raise the banner.
                self.banner = Some(resilience::disconnect_banner(msg));
                self.connection.on_event(&ev);
            }
            Event::Error(msg) => {
                self.banner = Some(msg.clone());
                self.connection.on_event(&ev);
            }
        }
    }

    /// Append a new block column, evicting the oldest beyond `MAX_COLUMNS`.
    pub fn push_column(&mut self, col: BlockColumn) {
        self.columns.push_back(col);
        while self.columns.len() > MAX_COLUMNS {
            self.columns.pop_front();
        }
        if self.follow {
            self.col_offset = 0;
        }
    }

    /// Pin an item, assigning it a fresh id. Returns the stored item.
    pub fn pin(&mut self, mut item: PinnedItem) -> PinnedItem {
        item.id = PinnedItemId(self.next_pin_id);
        self.next_pin_id += 1;
        self.pinned.push(item.clone());
        item
    }

    pub fn unpin(&mut self, id: PinnedItemId) {
        self.pinned.retain(|p| p.id != id);
    }

    /// Scroll one column toward the past; pauses follow.
    pub fn scroll_left(&mut self) {
        let max = self.columns.len().saturating_sub(1);
        if self.col_offset < max {
            self.col_offset += 1;
            self.follow = false;
        }
    }

    /// Scroll one column toward the tip; resumes follow at the tip.
    pub fn scroll_right(&mut self) {
        if self.col_offset > 0 {
            self.col_offset -= 1;
            if self.col_offset == 0 {
                self.follow = true;
            }
        }
    }

    /// Jump to the newest column and resume follow.
    pub fn jump_newest(&mut self) {
        self.col_offset = 0;
        self.follow = true;
    }

    /// Grid-level key handling (overlays/connection are routed in `run`). Returns
    /// the commands to dispatch.
    pub fn on_key(&mut self, key: KeyEvent) -> Vec<Command> {
        match key.code {
            KeyCode::Char('q') | KeyCode::Esc => {
                self.should_quit = true;
                vec![Command::Quit]
            }
            KeyCode::Char('b') => vec![Command::BuildBlock],
            KeyCode::Char('r') => {
                self.banner = None;
                vec![Command::Reconnect]
            }
            KeyCode::Char('g') => {
                self.jump_newest();
                vec![]
            }
            KeyCode::Left => {
                self.scroll_left();
                vec![]
            }
            KeyCode::Right => {
                self.scroll_right();
                vec![]
            }
            KeyCode::Up => {
                self.row_scroll = self.row_scroll.saturating_sub(1);
                vec![]
            }
            KeyCode::Down => {
                self.row_scroll = self.row_scroll.saturating_add(1);
                vec![]
            }
            _ => vec![],
        }
    }

    /// Rows that pass the current filter.
    fn visible_rows(&self) -> Vec<&PinnedItem> {
        let f = self.filter.to_lowercase();
        self.pinned
            .iter()
            .filter(|p| f.is_empty() || p.label.to_lowercase().contains(&f))
            .collect()
    }

    /// Build the grid view model for the current window.
    pub fn grid_view_model(&self, renderer: &dyn crate::contracts::ValueRenderer) -> GridViewModel {
        let total = self.columns.len();
        if total == 0 {
            return GridViewModel::empty();
        }
        // Visible window: `VISIBLE_COLS` ending `col_offset` back from the newest.
        let end = total - self.col_offset;
        let start = end.saturating_sub(VISIBLE_COLS);
        let window: Vec<&BlockColumn> = self.columns.range(start..end).collect();

        let columns: Vec<u32> = window.iter().map(|c| c.block.number).collect();

        // Per-window-column diff vs the column immediately before it in history.
        let diffs: Vec<std::collections::BTreeMap<PinnedItemId, CellDiff>> = window
            .iter()
            .enumerate()
            .map(|(j, col)| {
                let prev_idx = start + j;
                if prev_idx == 0 {
                    std::collections::BTreeMap::new()
                } else {
                    let prev = &self.columns[prev_idx - 1];
                    diff_columns(prev, col, renderer, &self.ctx)
                }
            })
            .collect();

        let rows = self
            .visible_rows()
            .iter()
            .map(|item| {
                let cells = window
                    .iter()
                    .zip(&diffs)
                    .map(|(col, diffmap)| {
                        let diff = diffmap.get(&item.id).cloned().unwrap_or(CellDiff::Unchanged);
                        cell_for(col.cells.get(&item.id), &item.path, diff, renderer, &self.ctx)
                    })
                    .collect();
                GridRow {
                    label: item.label.clone(),
                    cells,
                }
            })
            .collect();

        GridViewModel {
            rows,
            columns,
            scroll: self.row_scroll,
            column_window_start: start,
            follow: self.follow,
        }
    }
}

/// Render one cell from a pinned item's `CellState` at a column.
fn cell_for(
    state: Option<&CellState>,
    path: &[crate::contracts::PathSeg],
    diff: CellDiff,
    renderer: &dyn crate::contracts::ValueRenderer,
    ctx: &RenderCtx,
) -> GridCell {
    match state {
        Some(CellState::Value(v)) => GridCell {
            text: renderer.render(v, path, ctx),
            diff,
            undecodable: false,
        },
        Some(CellState::Missing) => GridCell {
            text: "∅".to_string(),
            diff,
            undecodable: false,
        },
        Some(CellState::Undecodable { .. }) => GridCell::undecodable(),
        None => GridCell {
            text: String::new(),
            diff,
            undecodable: false,
        },
    }
}

fn default_ctx() -> RenderCtx {
    RenderCtx {
        ss58_prefix: 0,
        token_decimals: 10,
        token_symbol: "DOT".to_string(),
    }
}

// ---------------------------------------------------------------------------
// Call catalog for the tx builder
// ---------------------------------------------------------------------------

/// A small curated set of dispatchable calls for the MVP transaction builder.
///
/// NOTE: this is intentionally a hand-picked catalog covering the common
/// balance/remark flows rather than a full metadata-driven enumeration. A
/// metadata-backed `CallCatalog` (walking the runtime's call enums) is a tracked
/// follow-up; it is orthogonal to the orchestration this ticket delivers.
pub struct CuratedCallCatalog;

impl CallCatalog for CuratedCallCatalog {
    fn pallets(&self) -> Vec<String> {
        vec!["Balances".into(), "System".into()]
    }

    fn calls(&self, pallet: &str) -> Vec<String> {
        match pallet {
            "Balances" => vec!["transfer_keep_alive".into(), "transfer_allow_death".into()],
            "System" => vec!["remark".into()],
            _ => vec![],
        }
    }

    fn args(&self, pallet: &str, call: &str) -> Vec<ArgSpec> {
        match (pallet, call) {
            ("Balances", "transfer_keep_alive") | ("Balances", "transfer_allow_death") => vec![
                ArgSpec::new("dest", ArgKind::AccountId),
                ArgSpec::new("value", ArgKind::U128),
            ],
            ("System", "remark") => vec![ArgSpec::new("remark", ArgKind::Text)],
            _ => vec![],
        }
    }
}

// ---------------------------------------------------------------------------
// Event loop
// ---------------------------------------------------------------------------

/// Run the application. Builds a tokio runtime and drives the async loop; the
/// terminal lifecycle + panic guard are installed by `main`.
pub fn run(terminal: DefaultTerminal) -> Result<()> {
    let rt = tokio::runtime::Runtime::new()?;
    rt.block_on(run_async(terminal))
}

async fn run_async(mut terminal: DefaultTerminal) -> Result<()> {
    let (cmd_tx, mut cmd_rx) = mpsc::unbounded_channel::<Command>();
    let (evt_tx, mut evt_rx) = mpsc::unbounded_channel::<Event>();
    let (client_tx, mut client_rx) = mpsc::unbounded_channel::<&'static SubxtChainClient>();
    let (block_tx, mut block_rx) =
        mpsc::unbounded_channel::<Result<crate::contracts::BlockRef>>();

    // Blocking terminal-input reader → key channel (avoids the crossterm
    // event-stream feature).
    let (key_tx, mut key_rx) = mpsc::unbounded_channel::<CtEvent>();
    std::thread::spawn(move || {
        while let Ok(ev) = crossterm::event::read() {
            if key_tx.send(ev).is_err() {
                break;
            }
        }
    });

    let renderer = DefaultRenderer;
    let mut app = AppState::new();

    // Set once connected. The client is leaked to 'static (process-lifetime
    // singleton) so the borrowing overlays can be stored across frames.
    let mut client: Option<&'static SubxtChainClient> = None;
    let mut catalog: Option<&'static MetadataCatalog<'static>> = None;
    let mut picker: Option<StoragePicker<'static>> = None;
    let mut tx_builder: Option<TxBuilder<CuratedCallCatalog>> = None;

    loop {
        terminal.draw(|f| render(f, &app, &renderer, picker.as_ref(), tx_builder.as_ref()))?;
        if app.should_quit {
            break;
        }

        tokio::select! {
            Some(ct) = key_rx.recv() => {
                if let CtEvent::Key(key) = ct && key.kind == KeyEventKind::Press {
                    handle_key(key, &mut app, &cmd_tx, &mut picker, &mut tx_builder, catalog);
                }
            }
            Some(ev) = evt_rx.recv() => {
                if let Event::TxResult(ref o) = ev && let Some(tb) = tx_builder.as_mut() {
                    tb.set_result(o.clone());
                }
                app.on_event(ev);
            }
            Some(c) = client_rx.recv() => {
                client = Some(c);
                let cat: &'static MetadataCatalog<'static> =
                    Box::leak(Box::new(MetadataCatalog { metadata: c.metadata() }));
                catalog = Some(cat);
                // Drive the block subscription in its own task → block channel.
                let btx = block_tx.clone();
                tokio::spawn(async move {
                    use futures::StreamExt;
                    let mut s = c.subscribe_blocks();
                    while let Some(r) = s.next().await {
                        if btx.send(r).is_err() {
                            break;
                        }
                    }
                });
            }
            Some(block) = block_rx.recv() => {
                match block {
                    Ok(blk) => {
                        if let Some(c) = client {
                            spawn_fetch_column(c, blk, app.pinned.clone(), evt_tx.clone());
                        }
                    }
                    Err(e) => { let _ = evt_tx.send(Event::Disconnected(e.to_string())); }
                }
            }
            Some(cmd) = cmd_rx.recv() => {
                dispatch(cmd, client, &evt_tx, &client_tx);
            }
        }
    }
    Ok(())
}

/// Route a key to the connection screen, an open overlay, or the grid.
fn handle_key(
    key: KeyEvent,
    app: &mut AppState,
    cmd_tx: &mpsc::UnboundedSender<Command>,
    picker: &mut Option<StoragePicker<'static>>,
    tx_builder: &mut Option<TxBuilder<CuratedCallCatalog>>,
    catalog: Option<&'static MetadataCatalog<'static>>,
) {
    // Connecting: drive the connection form.
    if app.phase == Phase::Connecting {
        if let Some(cmd) = app.connection.on_key(key) {
            let _ = cmd_tx.send(cmd);
        }
        return;
    }

    // Overlay open: route to it; Esc closes.
    if let Some(p) = picker.as_mut() {
        if key.code == KeyCode::Esc {
            *picker = None;
            return;
        }
        if let Some(Command::Pin(item)) = p.on_key(key) {
            app.pin(item.clone());
            let _ = cmd_tx.send(Command::Pin(item));
            *picker = None;
        }
        return;
    }
    if let Some(tb) = tx_builder.as_mut() {
        if key.code == KeyCode::Esc {
            *tx_builder = None;
            return;
        }
        if let Some(cmd) = tb.on_key(key) {
            let _ = cmd_tx.send(cmd);
        }
        return;
    }

    // Grid: 'p'/'t' open overlays; everything else is grid navigation.
    match key.code {
        KeyCode::Char('p') => {
            if let Some(cat) = catalog {
                *picker = Some(StoragePicker::new(cat));
            }
        }
        KeyCode::Char('t') => {
            *tx_builder = Some(TxBuilder::new(CuratedCallCatalog));
        }
        _ => {
            for cmd in app.on_key(key) {
                let _ = cmd_tx.send(cmd);
            }
        }
    }
}

/// Act on a UI command: spawn the matching background task.
fn dispatch(
    cmd: Command,
    client: Option<&'static SubxtChainClient>,
    evt_tx: &mpsc::UnboundedSender<Event>,
    client_tx: &mpsc::UnboundedSender<&'static SubxtChainClient>,
) {
    match cmd {
        Command::Connect(cfg) => spawn_connect(cfg, evt_tx.clone(), client_tx.clone()),
        Command::Reconnect => {
            // Reconnect re-runs spawn with the same default; surfaced as a banner
            // clear. A full reconnect flow reuses Connect from the form.
            let _ = evt_tx.send(Event::BootLog("reconnect requested".into()));
        }
        Command::BuildBlock => {
            if let Some(c) = client {
                let rpc = c.rpc().clone();
                let evt = evt_tx.clone();
                tokio::spawn(async move {
                    if let Err(e) = dev_rpc::new_block(&rpc).await {
                        let _ = evt.send(Event::Error(format!("build block: {e}")));
                    }
                });
            }
        }
        Command::SubmitTx(tx) => {
            if let Some(c) = client {
                let inner = c.inner().clone();
                let evt = evt_tx.clone();
                tokio::spawn(async move {
                    match dev_rpc::submit(&inner, tx).await {
                        Ok(outcome) => {
                            let _ = evt.send(Event::TxResult(outcome));
                        }
                        Err(e) => {
                            let _ = evt.send(Event::Error(format!("submit: {e}")));
                        }
                    }
                });
            }
        }
        // Pin/Unpin are applied to AppState in the UI loop; the worker re-reads the
        // pinned set on each block, so nothing to spawn here.
        Command::Pin(_) | Command::Unpin(_) => {}
        Command::Quit => {}
    }
}

/// Spawn the connect flow: start the supervisor (forwarding boot logs), connect
/// the chain client, and hand the leaked client back to the loop.
fn spawn_connect(
    cfg: crate::contracts::ForkConfig,
    evt_tx: mpsc::UnboundedSender<Event>,
    client_tx: mpsc::UnboundedSender<&'static SubxtChainClient>,
) {
    tokio::spawn(async move {
        let sup = Arc::new(Supervisor::new());
        {
            let mut logs = sup.log_lines();
            let evt = evt_tx.clone();
            tokio::spawn(async move {
                while let Ok(line) = logs.recv().await {
                    let _ = evt.send(Event::BootLog(line));
                }
            });
        }

        let endpoint = match sup.start(&cfg).await {
            Ok(e) => e,
            Err(e) => {
                let _ = evt_tx.send(Event::Error(format!("chopsticks: {e}")));
                return;
            }
        };

        match SubxtChainClient::connect(&endpoint).await {
            Ok(c) => {
                let ctx = c.render_ctx().clone();
                let leaked: &'static SubxtChainClient = Box::leak(Box::new(c));
                let _ = evt_tx.send(Event::Connected {
                    metadata_ready: true,
                    ctx,
                });
                let _ = client_tx.send(leaked);
            }
            Err(e) => {
                let _ = evt_tx.send(Event::Error(format!("connect: {e}")));
            }
        }

        // Keep the supervisor (and its child process) alive for the session.
        std::future::pending::<()>().await;
        drop(sup);
    });
}

/// Fetch every pinned item at `blk` and emit the resulting `BlockColumn`.
fn spawn_fetch_column(
    client: &'static SubxtChainClient,
    blk: crate::contracts::BlockRef,
    pinned: Vec<PinnedItem>,
    evt_tx: mpsc::UnboundedSender<Event>,
) {
    let inner = client.inner().clone();
    tokio::spawn(async move {
        let mut cells = std::collections::BTreeMap::new();
        for item in &pinned {
            let state = storage_fetch::fetch(&inner, item, blk.hash)
                .await
                .unwrap_or_else(|e| CellState::Undecodable {
                    raw_hex: String::new(),
                    error: e.to_string(),
                });
            cells.insert(item.id, state);
        }
        let _ = evt_tx.send(Event::NewColumn(BlockColumn { block: blk, cells }));
    });
}

// ---------------------------------------------------------------------------
// Rendering
// ---------------------------------------------------------------------------

fn render(
    f: &mut Frame,
    app: &AppState,
    renderer: &dyn crate::contracts::ValueRenderer,
    picker: Option<&StoragePicker<'static>>,
    tx_builder: Option<&TxBuilder<CuratedCallCatalog>>,
) {
    let area = f.area();
    match app.phase {
        Phase::Connecting => app.connection.render(f, area),
        Phase::Grid => {
            // banner (optional) + grid + tx-result panel.
            let chunks = Layout::vertical([
                Constraint::Length(if app.banner.is_some() { 1 } else { 0 }),
                Constraint::Min(3),
                Constraint::Length(if app.tx_result.is_some() { 4 } else { 0 }),
            ])
            .split(area);

            if let Some(banner) = &app.banner {
                f.render_widget(
                    Paragraph::new(banner.as_str())
                        .style(ratatui::style::Style::default().fg(ratatui::style::Color::Red)),
                    chunks[0],
                );
            }

            let model = app.grid_view_model(renderer);
            f.render_widget(GridView::new(&model), chunks[1]);

            if let Some(outcome) = &app.tx_result {
                let status = if outcome.success { "OK" } else { "FAILED" };
                let body = format!(
                    "tx {}: {} event(s){}",
                    status,
                    outcome.events.len(),
                    outcome
                        .error
                        .as_ref()
                        .map(|e| format!(" — {e}"))
                        .unwrap_or_default()
                );
                f.render_widget(
                    Paragraph::new(body).block(Block::bordered().title("Transaction")),
                    chunks[2],
                );
            }

            // Overlays draw on top in a centered area.
            if let Some(p) = picker {
                let a = centered(area, 70, 70);
                f.render_widget(Clear, a);
                p.render(f, a);
            } else if let Some(tb) = tx_builder {
                let a = centered(area, 70, 70);
                f.render_widget(Clear, a);
                tb.render(f, a);
            }
        }
    }
}

/// A centered rect `pct_w`% × `pct_h`% of `area`.
fn centered(area: Rect, pct_w: u16, pct_h: u16) -> Rect {
    let v = Layout::vertical([
        Constraint::Percentage((100 - pct_h) / 2),
        Constraint::Percentage(pct_h),
        Constraint::Percentage((100 - pct_h) / 2),
    ])
    .split(area);
    Layout::horizontal([
        Constraint::Percentage((100 - pct_w) / 2),
        Constraint::Percentage(pct_w),
        Constraint::Percentage((100 - pct_w) / 2),
    ])
    .split(v[1])[1]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::contracts::{BlockRef, KeyArg};
    use scale_value::Value;
    use subxt::utils::H256;

    fn item(id: u64, label: &str) -> PinnedItem {
        PinnedItem {
            id: PinnedItemId(id),
            pallet: "System".into(),
            entry: "Account".into(),
            keys: vec![KeyArg::U(id as u128)],
            path: vec![],
            label: label.into(),
        }
    }

    fn column(number: u32, id: u64, val: u128) -> BlockColumn {
        let mut cells = std::collections::BTreeMap::new();
        cells.insert(
            PinnedItemId(id),
            CellState::Value(Value::u128(val).map_context(|_| 0u32)),
        );
        BlockColumn {
            block: BlockRef {
                number,
                hash: H256::zero(),
            },
            cells,
        }
    }

    #[test]
    fn new_column_appends_and_computes_diffs() {
        let mut app = AppState::new();
        app.pinned.push(item(1, "row"));
        app.push_column(column(10, 1, 100));
        app.push_column(column(11, 1, 200)); // value changed
        let model = app.grid_view_model(&DefaultRenderer);
        assert_eq!(model.columns, vec![10, 11]);
        // Newest column's cell is Changed.
        let newest = model.rows[0].cells.last().unwrap();
        assert!(matches!(newest.diff, CellDiff::Changed { .. }));
    }

    #[test]
    fn ring_buffer_evicts_oldest_beyond_n() {
        let mut app = AppState::new();
        for n in 0..(MAX_COLUMNS as u32 + 5) {
            app.push_column(column(n, 1, n as u128));
        }
        assert_eq!(app.columns.len(), MAX_COLUMNS);
        assert_eq!(app.columns.front().unwrap().block.number, 5);
    }

    #[test]
    fn scroll_left_pauses_follow_jump_newest_resumes() {
        let mut app = AppState::new();
        for n in 0..5 {
            app.push_column(column(n, 1, n as u128));
        }
        assert!(app.follow);
        app.scroll_left();
        assert!(!app.follow);
        assert_eq!(app.col_offset, 1);
        app.jump_newest();
        assert!(app.follow);
        assert_eq!(app.col_offset, 0);
    }

    #[test]
    fn disconnect_sets_banner_and_freezes_grid() {
        let mut app = AppState::new();
        app.phase = Phase::Grid;
        app.push_column(column(1, 1, 1));
        let before = app.columns.len();
        app.on_event(Event::Disconnected("ws closed".into()));
        assert!(app.banner.is_some());
        assert_eq!(app.columns.len(), before); // grid frozen, not cleared
    }

    #[test]
    fn pin_command_adds_row_unpin_removes() {
        let mut app = AppState::new();
        let stored = app.pin(item(0, "System.Account(Alice)"));
        assert_eq!(app.pinned.len(), 1);
        assert_ne!(stored.id, PinnedItemId(0)); // id was assigned
        app.unpin(stored.id);
        assert!(app.pinned.is_empty());
    }

    #[test]
    fn tx_result_populates_panel_without_touching_grid() {
        let mut app = AppState::new();
        app.phase = Phase::Grid;
        app.push_column(column(1, 1, 1));
        let before = app.columns.len();
        app.on_event(Event::TxResult(crate::contracts::TxOutcome {
            success: true,
            events: vec![],
            error: None,
        }));
        assert!(app.tx_result.is_some());
        assert_eq!(app.columns.len(), before);
    }
}
