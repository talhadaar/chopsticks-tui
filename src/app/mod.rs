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
    BlockColumn, BuildMode, CellDiff, CellState, ChainClient, ChopsticksSupervisor, Command, Event,
    PinnedItem, PinnedItemId, PreparedTx, RenderCtx, Result,
};
use crate::render::diff::diff_columns;
use crate::render::value::DefaultRenderer;
use crate::views::connection::ConnectionView;
use crate::views::grid::{GridCell, GridRow, GridView, GridViewModel};
use crate::views::picker::StoragePicker;
use crate::views::build_panel::{BuildPanel, PanelAction};
use crate::views::tx_builder::{ArgKind, ArgSpec, CallCatalog, TxBuilder};
use crate::app::input::{KeyRouting, Mode, route_key};
use crate::views::command_registry::{CommandRoute, LocalAction, parse_line, to_route};
use crate::views::hint_bar::render_hint_bar;
use crate::views::palette::{CommandPalette, PaletteOutcome};

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
    /// Current input mode (P0): drives key routing + the hint-bar indicator.
    pub mode: Mode,
    /// The open command palette, if any (P0).
    pub palette: Option<CommandPalette>,
    /// Baseline block for diffing — `None` = vs-previous (P1 owns the logic).
    pub baseline: Option<u32>,
    /// Staged extrinsics for `:build` (P3 owns the logic).
    pub build_queue: Vec<PreparedTx>,
    /// Current Chopsticks build mode (P3 owns the logic).
    pub build_mode: BuildMode,
    /// Ordered replayable session log (P4 owns the type + persistence).
    #[allow(dead_code)] // element type refined to RecordedAction + populated by P4
    pub action_log: Vec<crate::contracts::Command>,
    /// Name of the restored session, if any (P4).
    #[allow(dead_code)] // populated by P4
    pub loaded_session: Option<String>,
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
            mode: Mode::Normal,
            palette: None,
            baseline: None,
            build_queue: Vec::new(),
            build_mode: BuildMode::Manual,
            action_log: Vec::new(),
            loaded_session: None,
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

    /// Set the current build mode (UI-side; the RPC switch is dispatched
    /// separately via `Command::SetBuildMode`). MVP-2 P3.
    pub fn set_build_mode(&mut self, mode: BuildMode) {
        self.build_mode = mode;
    }

    /// Set (or move) the baseline. `None` resolves to the newest column's block
    /// number ("pin the current tip"); if there are no columns it stays `None`.
    /// `Some(n)` pins that exact block number.
    pub fn set_baseline(&mut self, block: Option<u32>) {
        self.baseline = match block {
            Some(n) => Some(n),
            None => self.columns.back().map(|c| c.block.number),
        };
    }

    /// Clear the baseline, reverting to vs-previous diffing.
    pub fn clear_baseline(&mut self) {
        self.baseline = None;
    }

    /// The column matching the current baseline number, if it is still in the
    /// ring buffer. Looked up by block **number**, so eviction returns `None`
    /// rather than silently pointing at a different block.
    pub fn baseline_column(&self) -> Option<&BlockColumn> {
        let n = self.baseline?;
        self.columns.iter().find(|c| c.block.number == n)
    }

    /// Apply a client-side action routed from the palette. P0 owns the seam;
    /// P1 fills the baseline arms with real mutation. The overlay-opening arms
    /// are handled by the app loop (which owns the overlays), so they are
    /// no-ops on state here.
    pub fn apply_local(&mut self, action: LocalAction) {
        match action {
            // P1: baseline mutation, applied directly to state (no RPC).
            LocalAction::SetBaseline(block) => self.set_baseline(block),
            LocalAction::ClearBaseline => self.clear_baseline(),
            // Deferred to later MVP-2 phases: surface feedback instead of a silent
            // no-op, matching the RPC stubs' "not yet implemented". P4 wires up
            // sessions.
            LocalAction::OpenSessions => {
                self.banner = Some("not yet implemented".into());
            }
            // Overlay/modal opens are performed by the loop, not by state.
            LocalAction::OpenPicker
            | LocalAction::OpenTxBuilder
            | LocalAction::OpenBuildPanel => {}
        }
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
        use crate::views::grid::BaselineState;

        let total = self.columns.len();
        if total == 0 {
            return GridViewModel::empty();
        }
        // Visible window: `VISIBLE_COLS` ending `col_offset` back from the newest.
        let end = total - self.col_offset;
        let start = end.saturating_sub(VISIBLE_COLS);
        let window: Vec<&BlockColumn> = self.columns.range(start..end).collect();

        let columns: Vec<u32> = window.iter().map(|c| c.block.number).collect();

        // Resolve the baseline (if any) to a column and classify its state.
        let baseline_col = self.baseline_column();
        let baseline_state = match (self.baseline, baseline_col) {
            (None, _) => BaselineState::Off,
            (Some(_), Some(_)) => BaselineState::Live,
            (Some(n), None) => {
                // Number set but no column: older than the front = evicted,
                // otherwise newer than everything we hold = pending.
                let front = self.columns.front().map(|c| c.block.number);
                if front.is_some_and(|f| n < f) {
                    BaselineState::Evicted
                } else {
                    BaselineState::Pending
                }
            }
        };

        // Per-window-column diff. In Live baseline mode every column diffs
        // against the frozen baseline column; otherwise (Off, or an
        // unresolvable baseline) we fall back to vs-previous (MVP-1 default).
        let diffs: Vec<std::collections::BTreeMap<PinnedItemId, CellDiff>> = window
            .iter()
            .enumerate()
            .map(|(j, col)| {
                if let (BaselineState::Live, Some(base)) = (baseline_state, baseline_col) {
                    diff_columns(base, col, renderer, &self.ctx)
                } else {
                    let prev_idx = start + j;
                    if prev_idx == 0 {
                        std::collections::BTreeMap::new()
                    } else {
                        let prev = &self.columns[prev_idx - 1];
                        diff_columns(prev, col, renderer, &self.ctx)
                    }
                }
            })
            .collect();

        let visible = self.visible_rows();

        let rows: Vec<GridRow> = visible
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

        // Frozen baseline column: only materialise it when the baseline is Live
        // AND it sits outside the visible window (otherwise the window already
        // shows it — Task 5's guard avoids drawing it twice). The frozen column
        // is the baseline diffed against itself → all Unchanged (gray).
        let baseline_column = match (baseline_state, baseline_col) {
            (BaselineState::Live, Some(base)) if !columns.contains(&base.block.number) => {
                let self_diff = diff_columns(base, base, renderer, &self.ctx);
                let frozen = visible
                    .iter()
                    .map(|item| {
                        let diff =
                            self_diff.get(&item.id).cloned().unwrap_or(CellDiff::Unchanged);
                        cell_for(base.cells.get(&item.id), &item.path, diff, renderer, &self.ctx)
                    })
                    .collect();
                Some(frozen)
            }
            _ => None,
        };

        GridViewModel {
            rows,
            columns,
            scroll: self.row_scroll,
            column_window_start: start,
            follow: self.follow,
            baseline_block: self.baseline,
            baseline_state,
            baseline_column,
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
    let mut build_panel: Option<BuildPanel> = None;

    loop {
        terminal.draw(|f| {
            render(
                f,
                &app,
                &renderer,
                picker.as_ref(),
                tx_builder.as_ref(),
                build_panel.as_ref(),
            )
        })?;
        if app.should_quit {
            break;
        }

        tokio::select! {
            Some(ct) = key_rx.recv() => {
                if let CtEvent::Key(key) = ct && key.kind == KeyEventKind::Press {
                    handle_key(
                        key, &mut app, &cmd_tx,
                        &mut picker, &mut tx_builder, &mut build_panel, catalog,
                    );
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
#[allow(clippy::too_many_arguments)]
fn handle_key(
    key: KeyEvent,
    app: &mut AppState,
    cmd_tx: &mpsc::UnboundedSender<Command>,
    picker: &mut Option<StoragePicker<'static>>,
    tx_builder: &mut Option<TxBuilder<CuratedCallCatalog>>,
    build_panel: &mut Option<BuildPanel>,
    catalog: Option<&'static MetadataCatalog<'static>>,
) {
    // Connecting: drive the connection form (unchanged MVP-1 behavior).
    if app.phase == Phase::Connecting {
        if let Some(cmd) = app.connection.on_key(key) {
            let _ = cmd_tx.send(cmd);
        }
        return;
    }

    match route_key(app.mode, key) {
        KeyRouting::OpenPalette => {
            app.palette = Some(CommandPalette::new());
            app.mode = Mode::Command;
        }
        KeyRouting::OpenPicker => {
            if let Some(cat) = catalog {
                *picker = Some(StoragePicker::new(cat));
                app.mode = Mode::Insert;
            }
        }
        KeyRouting::OpenTxBuilder => {
            *tx_builder = Some(TxBuilder::new(CuratedCallCatalog));
            app.mode = Mode::Insert;
        }
        KeyRouting::Grid(key) => {
            for cmd in app.on_key(key) {
                let _ = cmd_tx.send(cmd);
            }
        }
        KeyRouting::Palette(key) => {
            handle_palette_key(key, app, cmd_tx, picker, tx_builder, build_panel, catalog);
        }
        KeyRouting::Overlay(key) => {
            handle_overlay_key(key, app, cmd_tx, picker, tx_builder, build_panel);
        }
    }
}

/// Feed a key to the open palette and act on its outcome. Owns the Command→Normal
/// transition and the parse/route → dispatch-or-local plumbing.
#[allow(clippy::too_many_arguments)]
fn handle_palette_key(
    key: KeyEvent,
    app: &mut AppState,
    cmd_tx: &mpsc::UnboundedSender<Command>,
    picker: &mut Option<StoragePicker<'static>>,
    tx_builder: &mut Option<TxBuilder<CuratedCallCatalog>>,
    build_panel: &mut Option<BuildPanel>,
    catalog: Option<&'static MetadataCatalog<'static>>,
) {
    let outcome = match app.palette.as_mut() {
        Some(p) => p.on_key(key),
        None => {
            app.mode = Mode::Normal;
            return;
        }
    };
    match outcome {
        PaletteOutcome::Pending => {}
        PaletteOutcome::Cancel => {
            app.palette = None;
            app.mode = Mode::Normal;
        }
        PaletteOutcome::Submit(line) => {
            app.palette = None;
            app.mode = Mode::Normal;
            match parse_line(&line).and_then(|p| to_route(&p)) {
                Ok(CommandRoute::Dispatch(cmd)) => {
                    let _ = cmd_tx.send(cmd);
                }
                Ok(CommandRoute::Local(action)) => {
                    // Overlay-opening locals are performed here (the loop owns the
                    // overlays); the rest go through apply_local.
                    match action {
                        LocalAction::OpenPicker => {
                            if let Some(cat) = catalog {
                                *picker = Some(StoragePicker::new(cat));
                                app.mode = Mode::Insert;
                            }
                        }
                        LocalAction::OpenTxBuilder => {
                            *tx_builder = Some(TxBuilder::new(CuratedCallCatalog));
                            app.mode = Mode::Insert;
                        }
                        LocalAction::OpenBuildPanel => {
                            *build_panel = Some(BuildPanel::new());
                            app.mode = Mode::Insert;
                        }
                        other => app.apply_local(other),
                    }
                }
                Err(e) => {
                    app.banner = Some(format!("command: {e}"));
                }
            }
        }
    }
}

/// Feed a key to whichever overlay is open (Insert mode). Mirrors MVP-1's overlay
/// routing exactly: Esc closes (→ Normal); the picker emits Pin; the tx builder
/// emits its command.
fn handle_overlay_key(
    key: KeyEvent,
    app: &mut AppState,
    cmd_tx: &mpsc::UnboundedSender<Command>,
    picker: &mut Option<StoragePicker<'static>>,
    tx_builder: &mut Option<TxBuilder<CuratedCallCatalog>>,
    build_panel: &mut Option<BuildPanel>,
) {
    if let Some(p) = picker.as_mut() {
        if key.code == KeyCode::Esc {
            *picker = None;
            app.mode = Mode::Normal;
            return;
        }
        if let Some(Command::Pin(item)) = p.on_key(key) {
            app.pin(item.clone());
            let _ = cmd_tx.send(Command::Pin(item));
            *picker = None;
            app.mode = Mode::Normal;
        }
        return;
    }
    if let Some(tb) = tx_builder.as_mut() {
        // Esc closes the tx-builder. If a build panel is open, the tx-builder was
        // opened from it (staging flow) — return to the panel; otherwise (the
        // standalone `:tx` flow) return to the grid.
        if key.code == KeyCode::Esc {
            *tx_builder = None;
            if build_panel.is_none() {
                app.mode = Mode::Normal;
            }
            return;
        }
        if let Some(cmd) = tb.on_key(key) {
            // Submit-mode terminal action.
            let _ = cmd_tx.send(cmd);
            return;
        }
        // Stage-mode: if a tx was just staged, hand it to the build panel and
        // close the tx-builder (back to the panel).
        if let Some(tx) = tb.take_staged() {
            if let Some(bp) = build_panel.as_mut() {
                bp.push(tx);
            }
            *tx_builder = None;
        }
        return;
    }
    if let Some(bp) = build_panel.as_mut() {
        match bp.on_key(key) {
            PanelAction::None => {}
            PanelAction::AddExtrinsic => {
                // Open the tx-builder in Stage mode on top of the panel; its
                // completed tx is drained back into the panel above.
                *tx_builder = Some(TxBuilder::new_staging(CuratedCallCatalog));
            }
            PanelAction::Build { queue, timestamp, author } => {
                // Block metadata (timestamp/author) is display-only: the frozen
                // BuildWithQueue(Vec<PreparedTx>) contract carries no metadata, so
                // the dispatch helper builds with defaults. Forwarding overrides is
                // a P0-owned contract extension (shared-contracts §3).
                let _ = (timestamp, author);
                let _ = cmd_tx.send(Command::BuildWithQueue(queue));
                *build_panel = None;
                app.mode = Mode::Normal;
            }
            PanelAction::Cancel => {
                *build_panel = None;
                app.mode = Mode::Normal;
            }
        }
        return;
    }
    // No overlay actually open (mode drift): snap back to Normal.
    app.mode = Mode::Normal;
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
        // MVP-2 P3: switch the Chopsticks block-build mode.
        Command::SetBuildMode(mode) => {
            if let Some(c) = client {
                let rpc = c.rpc().clone();
                let evt = evt_tx.clone();
                tokio::spawn(async move {
                    if let Err(e) = dev_rpc::set_block_build_mode(&rpc, mode).await {
                        let _ = evt.send(Event::Error(format!("set build mode: {e}")));
                    }
                });
            }
        }
        // MVP-2 P3: build one block from the staged extrinsic queue.
        Command::BuildWithQueue(queue) => {
            if let Some(c) = client {
                let inner = c.inner().clone();
                let rpc = c.rpc().clone();
                let evt = evt_tx.clone();
                tokio::spawn(async move {
                    if let Err(e) = dev_rpc::build_with_queue(&inner, &rpc, queue).await {
                        let _ = evt.send(Event::Error(format!("build with queue: {e}")));
                    }
                });
            }
        }
        // Remaining MVP-2 commands: P0 lands stub arms; owning plans replace them.
        Command::SetStorage(_)
        | Command::SetHead(_)
        | Command::TimeTravel(_)
        | Command::SaveSession(_)
        | Command::LoadSession(_) => {
            let _ = evt_tx.send(Event::Error("not yet implemented".into()));
        }
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
    build_panel: Option<&BuildPanel>,
) {
    let area = f.area();
    match app.phase {
        Phase::Connecting => app.connection.render(f, area),
        Phase::Grid => {
            // status line (banner OR build-mode indicator) + grid + tx-result
            // panel + hint bar. The top line is always reserved: an error banner
            // takes priority, otherwise it shows the current build mode.
            let chunks = Layout::vertical([
                Constraint::Length(1),
                Constraint::Min(3),
                Constraint::Length(if app.tx_result.is_some() { 4 } else { 0 }),
                Constraint::Length(1),
            ])
            .split(area);

            if let Some(banner) = &app.banner {
                f.render_widget(
                    Paragraph::new(banner.as_str())
                        .style(ratatui::style::Style::default().fg(ratatui::style::Color::Red)),
                    chunks[0],
                );
            } else {
                // Build-mode indicator (purple = dev surface).
                f.render_widget(
                    Paragraph::new(format!("build-mode: {:?}", app.build_mode))
                        .style(ratatui::style::Style::default().fg(ratatui::style::Color::Magenta)),
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

            render_hint_bar(f, chunks[3], app.mode);

            // Overlays draw on top in a centered area; the palette anchors itself.
            if let Some(p) = picker {
                let a = centered(area, 70, 70);
                f.render_widget(Clear, a);
                p.render(f, a);
            } else if let Some(tb) = tx_builder {
                let a = centered(area, 70, 70);
                f.render_widget(Clear, a);
                tb.render(f, a);
            } else if let Some(bp) = build_panel {
                let a = centered(area, 70, 60);
                f.render_widget(Clear, a);
                bp.render(f, a);
            } else if let Some(pal) = &app.palette {
                pal.render(f, area);
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
    fn set_baseline_none_resolves_to_newest_column_number() {
        let mut app = AppState::new();
        app.push_column(column(10, 1, 100));
        app.push_column(column(11, 1, 200));
        // None means "pin the current tip": resolves to the newest column number.
        app.set_baseline(None);
        assert_eq!(app.baseline, Some(11));
    }

    #[test]
    fn set_baseline_none_with_no_columns_stays_none() {
        let mut app = AppState::new();
        app.set_baseline(None);
        assert_eq!(app.baseline, None);
    }

    #[test]
    fn set_baseline_some_pins_that_exact_number() {
        let mut app = AppState::new();
        app.push_column(column(10, 1, 100));
        app.push_column(column(11, 1, 200));
        app.set_baseline(Some(10));
        assert_eq!(app.baseline, Some(10));
    }

    #[test]
    fn clear_baseline_reverts_to_none() {
        let mut app = AppState::new();
        app.push_column(column(10, 1, 100));
        app.set_baseline(Some(10));
        app.clear_baseline();
        assert_eq!(app.baseline, None);
    }

    #[test]
    fn baseline_column_looks_up_by_number_not_index() {
        let mut app = AppState::new();
        // Fill past MAX_COLUMNS so the oldest columns are evicted and indices shift.
        for n in 0..(MAX_COLUMNS as u32 + 5) {
            app.push_column(column(n, 1, n as u128));
        }
        // Front is now block #5 (0..4 evicted). A baseline pinned at #5 still
        // resolves; a baseline at #2 (evicted) does not.
        app.set_baseline(Some(5));
        assert_eq!(app.baseline_column().map(|c| c.block.number), Some(5));
        app.set_baseline(Some(2));
        assert!(app.baseline_column().is_none());
    }

    #[test]
    fn baseline_none_keeps_vs_previous_behaviour() {
        // Regression guard: with no baseline, the newest column diffs vs its
        // immediate predecessor exactly as MVP-1 did.
        let mut app = AppState::new();
        app.pinned.push(item(1, "row"));
        app.push_column(column(10, 1, 100));
        app.push_column(column(11, 1, 200)); // changed vs #10
        let model = app.grid_view_model(&DefaultRenderer);
        assert_eq!(model.baseline_state, crate::views::grid::BaselineState::Off);
        let newest = model.rows[0].cells.last().unwrap();
        assert!(matches!(newest.diff, CellDiff::Changed { .. }));
    }

    #[test]
    fn baseline_diffs_every_column_against_the_frozen_block() {
        let mut app = AppState::new();
        app.pinned.push(item(1, "row"));
        app.push_column(column(10, 1, 100)); // baseline
        app.push_column(column(11, 1, 200)); // differs from baseline
        app.push_column(column(12, 1, 200)); // SAME as #11 but still differs from baseline
        app.set_baseline(Some(10));
        let model = app.grid_view_model(&DefaultRenderer);
        assert_eq!(model.baseline_state, crate::views::grid::BaselineState::Live);
        assert_eq!(model.baseline_block, Some(10));
        // window columns are [10, 11, 12]. Cell index aligns with columns.
        let cells = &model.rows[0].cells;
        // baseline vs itself → Unchanged
        assert_eq!(cells[0].diff, CellDiff::Unchanged);
        // #11 differs from baseline → Changed
        assert!(matches!(cells[1].diff, CellDiff::Changed { .. }));
        // #12 == #11 (vs-previous would be Unchanged) but still ≠ baseline → Changed
        assert!(matches!(cells[2].diff, CellDiff::Changed { .. }));
    }

    #[test]
    fn baseline_unresolvable_falls_back_to_vs_previous_and_flags() {
        let mut app = AppState::new();
        app.pinned.push(item(1, "row"));
        // Evict #0..#4 so #5 is the new front; pin a baseline at #2 (gone).
        for n in 0..(MAX_COLUMNS as u32 + 5) {
            app.push_column(column(n, 1, n as u128));
        }
        app.set_baseline(Some(2)); // evicted
        let model = app.grid_view_model(&DefaultRenderer);
        assert_eq!(model.baseline_state, crate::views::grid::BaselineState::Evicted);
        // Fallback basis is vs-previous: consecutive distinct values are Changed.
        let cells = &model.rows[0].cells;
        assert!(matches!(cells.last().unwrap().diff, CellDiff::Changed { .. }));
    }

    #[test]
    fn baseline_newer_than_buffer_is_pending() {
        let mut app = AppState::new();
        app.pinned.push(item(1, "row"));
        app.push_column(column(10, 1, 100));
        app.set_baseline(Some(99)); // not seen yet
        let model = app.grid_view_model(&DefaultRenderer);
        assert_eq!(model.baseline_state, crate::views::grid::BaselineState::Pending);
    }

    #[test]
    fn dod_baseline_keeps_change_lit_across_span_vs_previous_goes_gray() {
        // A value changes once at #11, then holds steady for many blocks.
        let mut app = AppState::new();
        app.pinned.push(item(1, "row"));
        app.push_column(column(10, 1, 100)); // baseline value
        app.push_column(column(11, 1, 200)); // the change
        for n in 12..=20 {
            app.push_column(column(n, 1, 200)); // steady, ≠ baseline, == predecessor
        }

        // vs-previous (MVP-1 default): only #11 flashes; later columns are gray
        // (Unchanged) because each equals its predecessor.
        app.clear_baseline();
        let prev_model = app.grid_view_model(&DefaultRenderer);
        let cells = &prev_model.rows[0].cells;
        // The newest visible column (#20) equals its predecessor → Unchanged.
        assert_eq!(
            cells.last().unwrap().diff,
            CellDiff::Unchanged,
            "vs-previous: a long-steady value must be gray"
        );

        // vs-baseline #10: every column from #11 on still differs from the
        // frozen baseline, so the highlight STAYS LIT across the whole span.
        app.set_baseline(Some(10));
        let base_model = app.grid_view_model(&DefaultRenderer);
        let cells = &base_model.rows[0].cells;
        assert!(
            cells
                .iter()
                .all(|c| matches!(c.diff, CellDiff::Changed { .. } | CellDiff::Unchanged)),
            "baseline diffs are only Changed (≠ baseline) or Unchanged (== baseline)"
        );
        // The newest visible column still differs from the baseline → Changed,
        // unlike vs-previous mode where it was gray.
        assert!(
            matches!(cells.last().unwrap().diff, CellDiff::Changed { .. }),
            "baseline: a value that changed once stays lit across the span"
        );
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

    use crate::app::input::Mode;
    use crate::views::command_registry::LocalAction;
    use crossterm::event::{KeyEvent, KeyModifiers};

    #[test]
    fn app_starts_in_normal_mode_with_no_palette() {
        let app = AppState::new();
        assert_eq!(app.mode, Mode::Normal);
        assert!(app.palette.is_none());
        assert!(app.baseline.is_none());
        assert!(app.build_queue.is_empty());
        assert_eq!(app.build_mode, crate::contracts::BuildMode::Manual);
        assert!(app.action_log.is_empty());
        assert!(app.loaded_session.is_none());
    }

    #[test]
    fn set_build_mode_updates_state() {
        use crate::contracts::BuildMode;
        let mut app = AppState::new();
        assert_eq!(app.build_mode, BuildMode::Manual); // MVP-1 default
        app.set_build_mode(BuildMode::Instant);
        assert_eq!(app.build_mode, BuildMode::Instant);
    }

    #[test]
    fn apply_local_open_picker_is_a_noop_on_state() {
        // P0 routes OpenPicker via the loop (opening the overlay), not apply_local;
        // baseline arms are thin until P1. apply_local must not panic on any arm.
        let mut app = AppState::new();
        app.apply_local(LocalAction::ClearBaseline);
        app.apply_local(LocalAction::SetBaseline(Some(42)));
        app.apply_local(LocalAction::OpenPicker);
        app.apply_local(LocalAction::OpenTxBuilder);
        app.apply_local(LocalAction::OpenSessions);
        // No state assertion beyond "did not panic"; P1 fills baseline behavior.
    }

    #[test]
    fn apply_local_sets_and_clears_baseline() {
        let mut app = AppState::new();
        app.push_column(column(42, 1, 1));
        // SetBaseline(None) pins the current tip.
        app.apply_local(LocalAction::SetBaseline(None));
        assert_eq!(app.baseline, Some(42));
        // SetBaseline(Some(n)) pins an explicit number.
        app.apply_local(LocalAction::SetBaseline(Some(7)));
        assert_eq!(app.baseline, Some(7));
        // ClearBaseline reverts.
        app.apply_local(LocalAction::ClearBaseline);
        assert_eq!(app.baseline, None);
    }

    #[test]
    fn grid_render_includes_hint_bar() {
        let mut app = AppState::new();
        app.phase = Phase::Grid;
        app.push_column(column(1, 1, 1));
        let backend = ratatui::backend::TestBackend::new(80, 20);
        let mut term = ratatui::Terminal::new(backend).unwrap();
        term.draw(|f| render(f, &app, &DefaultRenderer, None, None, None)).unwrap();
        let buf = term.backend().buffer().clone();
        let dump: String = buf.content().iter().map(|c| c.symbol()).collect();
        assert!(dump.contains("NORMAL"), "hint bar mode indicator must render: {dump}");
    }

    fn press_key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    #[test]
    fn colon_enters_command_mode_and_opens_palette() {
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel::<Command>();
        let mut app = AppState::new();
        app.phase = Phase::Grid;
        let mut picker = None;
        let mut txb = None;
        let mut bp = None;
        handle_key(press_key(KeyCode::Char(':')), &mut app, &tx, &mut picker, &mut txb, &mut bp, None);
        assert_eq!(app.mode, Mode::Command);
        assert!(app.palette.is_some());
    }

    #[test]
    fn esc_in_command_mode_closes_palette_back_to_normal() {
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel::<Command>();
        let mut app = AppState::new();
        app.phase = Phase::Grid;
        let mut picker = None;
        let mut txb = None;
        let mut bp = None;
        handle_key(press_key(KeyCode::Char(':')), &mut app, &tx, &mut picker, &mut txb, &mut bp, None);
        handle_key(press_key(KeyCode::Esc), &mut app, &tx, &mut picker, &mut txb, &mut bp, None);
        assert_eq!(app.mode, Mode::Normal);
        assert!(app.palette.is_none());
    }

    #[test]
    fn connecting_phase_routes_to_connection_not_palette() {
        // MVP-1 guarantee: in Connecting, `:` must NOT open the palette; it goes
        // to the connection form (which treats it as input).
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel::<Command>();
        let mut app = AppState::new();
        assert_eq!(app.phase, Phase::Connecting);
        let mut picker = None;
        let mut txb = None;
        let mut bp = None;
        handle_key(press_key(KeyCode::Char(':')), &mut app, &tx, &mut picker, &mut txb, &mut bp, None);
        assert!(app.palette.is_none(), "palette must not open while connecting");
        assert_eq!(app.mode, Mode::Normal, "mode unchanged while connecting");
    }
}
