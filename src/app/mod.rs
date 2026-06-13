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
use crate::session::{self, RecordedAction, Session, SessionSource};
use crate::views::connection::ConnectionView;
use crate::views::grid::{GridCell, GridRow, GridView, GridViewModel};
use crate::views::picker::StoragePicker;
use crate::views::set_storage::{SetStorageEditor, ValueMode};
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
    /// Ordered replayable session log (freeze §4). P2/P3/P4 append via `record`.
    pub action_log: Vec<RecordedAction>,
    /// Name of the currently-restored (or just-saved) session, if any (P4).
    pub loaded_session: Option<String>,
    /// The fork config in use, captured at connect (for session save/restore).
    pub fork: Option<crate::contracts::ForkConfig>,
    /// Amber off-timeline badge: `Some(n)` once the head was rewound to `n`.
    pub off_timeline_from: Option<u32>,
    /// Recorded actions awaiting replay during a session restore (front = next).
    pub replay_queue: VecDeque<RecordedAction>,
    /// Expected head height of a session being restored, for drift detection.
    pub expected_restore_head: Option<u32>,
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
            fork: None,
            off_timeline_from: None,
            replay_queue: VecDeque::new(),
            expected_restore_head: None,
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

    /// Append a state-changing dev action to the replayable log (freeze §4).
    /// No-op-safe: callers (P2/P3/P4) invoke this in the same place they apply
    /// the effect; it never fails.
    pub fn record(&mut self, action: RecordedAction) {
        self.action_log.push(action);
    }

    /// Build the session payload for the *current* state (fork + pins + baseline +
    /// action log + head). Used both for manual saves and auto-snapshots.
    pub fn to_session(&self, name: &str, source: SessionSource) -> Session {
        let head = self.columns.back().map(|c| c.block.number).unwrap_or(0);
        Session {
            name: name.to_string(),
            // A session with no fork is meaningless; fall back to an Attach stub
            // so serialization never panics. Restore will surface a drift error.
            fork: self
                .fork
                .clone()
                .unwrap_or(crate::contracts::ForkConfig::Attach { url: String::new() }),
            pins: self.pinned.clone(),
            baseline: self.baseline,
            actions: self.action_log.clone(),
            head,
            source,
        }
    }

    /// `set-head` core (spec §7.1): snapshot-then-truncate. Saves the abandoned
    /// future as an auto-named session, drops every column past `new_head`,
    /// records the `SetHead` action, and raises the amber off-timeline banner.
    /// Returns the auto-snapshot session name. The `dev_setHead` RPC + the
    /// fresh-forward build are issued separately by the dispatch loop.
    pub fn snapshot_and_truncate(&mut self, new_head: u32) -> Result<String> {
        let old_tip = self.columns.back().map(|c| c.block.number).unwrap_or(new_head);
        let snap_name = session::auto_snapshot_name(old_tip);

        // 1. Snapshot the abandoned future (the full current state, head = old tip).
        let snapshot = self.to_session(&snap_name, SessionSource::AutoSnapshot);
        session::save_session(&snapshot)?;

        // 2. Drop columns past the new head; the grid ends cleanly at new_head.
        self.columns.retain(|c| c.block.number <= new_head);
        self.col_offset = 0;
        self.follow = true;

        // 3. Record + raise off-timeline state.
        self.record(RecordedAction::SetHead(new_head));
        self.off_timeline_from = Some(new_head);
        self.banner = Some(format!(
            "⏳ Head rewound to #{new_head}. Abandoned future → session \"{snap_name}\"."
        ));

        Ok(snap_name)
    }

    /// Apply the in-UI-loop portion of a P4 command and return follow-up
    /// `Command`s the dispatch loop should spawn (RPC + builds). Mirrors how
    /// `Pin` is applied to state in the loop while a fetch is spawned. Commands
    /// not handled here return no follow-ups.
    pub fn on_command_local(&mut self, cmd: Command) -> Vec<Command> {
        match cmd {
            Command::SetHead(block) => {
                // State effect: snapshot the future + truncate the grid. On a
                // failed snapshot, surface the error but still re-fork.
                if let Err(e) = self.snapshot_and_truncate(block) {
                    self.banner = Some(format!("set-head snapshot failed: {e}"));
                }
                // Follow-up: the actual dev_setHead RPC re-forks the live chain;
                // building forward then resumes via the normal block subscription.
                vec![Command::SetHead(block)]
            }
            Command::TimeTravel(spec) => {
                self.record(RecordedAction::TimeTravel(spec.clone()));
                self.banner = Some(format!("⏳ time-travelled to {}", spec.source));
                vec![Command::TimeTravel(spec)]
            }
            Command::SaveSession(name) => {
                let session = self.to_session(&name, SessionSource::Manual);
                match session::save_session(&session) {
                    Ok(()) => {
                        self.loaded_session = Some(name.clone());
                        self.banner = Some(format!("saved session \"{name}\""));
                    }
                    Err(e) => self.banner = Some(format!("save failed: {e}")),
                }
                vec![]
            }
            Command::LoadSession(name) => match session::load_session(&name) {
                Ok(session) => self.begin_restore(session),
                Err(e) => {
                    self.banner = Some(format!("load failed: {e}"));
                    vec![]
                }
            },
            // Non-P4 commands are not handled here.
            _ => vec![],
        }
    }

    /// Begin restoring a loaded session: apply the chain-independent state (pins,
    /// baseline, name, expected head, replay queue) and emit a `Connect` to
    /// re-fork via the existing supervisor path (`spawn_connect`). The replay
    /// queue is drained as the re-forked chain comes up.
    fn begin_restore(&mut self, session: Session) -> Vec<Command> {
        self.pinned = session.pins;
        self.baseline = session.baseline;
        self.loaded_session = Some(session.name);
        self.expected_restore_head = Some(session.head);
        self.replay_queue = session.actions.into_iter().collect();
        // Reset the live grid; the re-fork rebuilds it.
        self.columns.clear();
        self.col_offset = 0;
        self.follow = true;
        self.off_timeline_from = None;
        self.fork = Some(session.fork.clone());
        vec![Command::Connect(session.fork)]
    }

    /// Pop the next recorded action and translate it into the `Command` that
    /// reproduces it, or `None` when the queue is drained. Actions with no P4
    /// command yet (e.g. `SetStorage`/`SetBuildMode`, owned by P2/P3) are skipped,
    /// advancing to the next replayable action.
    pub fn drain_next_replay(&mut self) -> Option<Command> {
        while let Some(action) = self.replay_queue.pop_front() {
            match action {
                RecordedAction::SetHead(b) => return Some(Command::SetHead(b)),
                RecordedAction::TimeTravel(spec) => return Some(Command::TimeTravel(spec)),
                // Replay a built block as an empty +6s build (advances height).
                // Full re-staging of `extrinsics` lands when P3's tx machinery is
                // available (see plan §"Replay scope note").
                RecordedAction::BuiltBlock { .. } => return Some(Command::BuildBlock),
                // P2/P3 own these commands; until full replay lands, skip (no
                // panic, no silent corruption — the action stays in the file).
                RecordedAction::SetStorage { .. } | RecordedAction::SetBuildMode(_) => continue,
            }
        }
        None
    }

    /// Note the head height reached during a restore; warn on base drift (spec
    /// §8 risk / §11 open question). "Drift" = the re-forked tip differs from the
    /// expected saved head once replay has finished advancing the chain.
    pub fn note_restore_progress(&mut self, current_head: u32) {
        if let Some(expected) = self.expected_restore_head
            && self.replay_queue.is_empty()
            && current_head != expected
        {
            self.banner = Some(format!(
                "⚠ session base drift: expected head #{expected}, restored to #{current_head}"
            ));
            self.expected_restore_head = None;
        }
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
            // Overlay/modal opens are performed by the loop, not by state (P4
            // wires the sessions modal open at the loop site, like the others).
            LocalAction::OpenPicker
            | LocalAction::OpenTxBuilder
            | LocalAction::OpenSetStorage
            | LocalAction::OpenBuildPanel
            | LocalAction::OpenSessions => {}
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
    let mut set_storage_editor: Option<SetStorageEditor<'static>> = None;
    let mut build_panel: Option<BuildPanel> = None;
    let mut sessions: Option<crate::views::sessions::SessionsView> = None;

    loop {
        terminal.draw(|f| {
            render(
                f,
                &app,
                &renderer,
                picker.as_ref(),
                tx_builder.as_ref(),
                set_storage_editor.as_ref(),
                build_panel.as_ref(),
                sessions.as_ref(),
            )
        })?;
        if app.should_quit {
            break;
        }

        tokio::select! {
            Some(ct) = key_rx.recv() => {
                if let CtEvent::Key(key) = ct && key.kind == KeyEventKind::Press {
                    handle_key(
                        key,
                        &mut app,
                        &cmd_tx,
                        &mut picker,
                        &mut tx_builder,
                        &mut set_storage_editor,
                        &mut build_panel,
                        &mut sessions,
                        client,
                        catalog,
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
                        let number = blk.number;
                        if let Some(c) = client {
                            spawn_fetch_column(c, blk, app.pinned.clone(), evt_tx.clone());
                        }
                        // Restore in progress: advance the replay queue one step
                        // per new block, then check for base drift once drained.
                        if !app.replay_queue.is_empty()
                            && let Some(cmd) = app.drain_next_replay()
                        {
                            let _ = cmd_tx.send(cmd);
                        }
                        app.note_restore_progress(number);
                    }
                    Err(e) => { let _ = evt_tx.send(Event::Disconnected(e.to_string())); }
                }
            }
            Some(cmd) = cmd_rx.recv() => {
                // P4 commands have an in-loop state effect (snapshot/save/load)
                // that also produces follow-up RPC commands; route them first.
                let followups = if is_p4_command(&cmd) {
                    app.on_command_local(cmd)
                } else {
                    vec![cmd]
                };
                for f in followups {
                    dispatch(f, client, &evt_tx, &client_tx, app.pinned.clone());
                }
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
    set_storage_editor: &mut Option<SetStorageEditor<'static>>,
    build_panel: &mut Option<BuildPanel>,
    sessions: &mut Option<crate::views::sessions::SessionsView>,
    client: Option<&'static SubxtChainClient>,
    catalog: Option<&'static MetadataCatalog<'static>>,
) {
    // Connecting: drive the connection form (unchanged MVP-1 behavior).
    if app.phase == Phase::Connecting {
        if let Some(cmd) = app.connection.on_key(key) {
            // Capture the fork config so session save/restore has it (P4).
            if let Command::Connect(cfg) = &cmd {
                app.fork = Some(cfg.clone());
            }
            let _ = cmd_tx.send(cmd);
        }
        return;
    }

    // Set-storage editor open: route to it ahead of grid/palette. Esc closes;
    // a confirmed value dispatches `Command::SetStorage`.
    if let Some(ed) = set_storage_editor.as_mut() {
        if key.code == KeyCode::Esc {
            *set_storage_editor = None;
            app.mode = Mode::Normal;
            return;
        }
        if let Some(cmd) = ed.on_key(key) {
            let _ = cmd_tx.send(cmd);
            *set_storage_editor = None;
            app.mode = Mode::Normal;
        }
        return;
    }

    // Sessions modal open: route to it ahead of grid/palette (spec §8). It emits
    // a `SessionsAction` the loop turns into IO + Commands; the loop owns IO.
    if sessions.is_some() {
        route_sessions_key(key, app, cmd_tx, sessions);
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
            // `S` opens the sessions modal (standalone fallback before/besides the
            // `:sessions` palette verb; mirrors how `p`/`t` open overlays directly).
            if key.code == KeyCode::Char('S') {
                open_sessions(app, sessions);
                return;
            }
            for cmd in app.on_key(key) {
                let _ = cmd_tx.send(cmd);
            }
        }
        KeyRouting::Palette(key) => {
            handle_palette_key(
                key,
                app,
                cmd_tx,
                picker,
                tx_builder,
                set_storage_editor,
                build_panel,
                sessions,
                client,
                catalog,
            );
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
    set_storage_editor: &mut Option<SetStorageEditor<'static>>,
    build_panel: &mut Option<BuildPanel>,
    sessions: &mut Option<crate::views::sessions::SessionsView>,
    client: Option<&'static SubxtChainClient>,
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
                        LocalAction::OpenSetStorage => {
                            if let (Some(c), Some(cat)) = (client, catalog) {
                                *set_storage_editor = Some(build_set_storage_editor(c, cat));
                                app.mode = Mode::Insert;
                            } else {
                                app.banner = Some("set-storage: not connected".into());
                            }
                        }
                        LocalAction::OpenBuildPanel => {
                            *build_panel = Some(BuildPanel::new());
                            app.mode = Mode::Insert;
                        }
                        LocalAction::OpenSessions => open_sessions(app, sessions),
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

/// Open the sessions modal over the current on-disk session list (P4). A read
/// failure is non-fatal: open with an empty list and surface a banner.
fn open_sessions(
    app: &mut AppState,
    sessions: &mut Option<crate::views::sessions::SessionsView>,
) {
    let list = match crate::session::list_sessions() {
        Ok(l) => l,
        Err(e) => {
            app.banner = Some(format!("sessions: {e}"));
            Vec::new()
        }
    };
    *sessions = Some(crate::views::sessions::SessionsView::new(list));
    app.mode = Mode::Insert;
}

/// Feed a key to the open sessions modal and act on its `SessionsAction` (P4).
/// Restore/Save dispatch `Command`s the loop applies via `on_command_local`;
/// Delete/Rename perform IO here and reopen the refreshed list.
fn route_sessions_key(
    key: KeyEvent,
    app: &mut AppState,
    cmd_tx: &mpsc::UnboundedSender<Command>,
    sessions: &mut Option<crate::views::sessions::SessionsView>,
) {
    use crate::views::sessions::SessionsAction;
    let Some(sv) = sessions.as_mut() else { return };
    match sv.on_key(key) {
        Some(SessionsAction::Close) => {
            *sessions = None;
            app.mode = Mode::Normal;
        }
        Some(SessionsAction::Restore(name)) => {
            let _ = cmd_tx.send(Command::LoadSession(name));
            *sessions = None;
            app.mode = Mode::Normal;
        }
        Some(SessionsAction::SaveCurrent) => {
            // Derive a default name from the current head; the loop persists it.
            let head = app.columns.back().map(|c| c.block.number).unwrap_or(0);
            let _ = cmd_tx.send(Command::SaveSession(format!("session-#{head}")));
            *sessions = None;
            app.mode = Mode::Normal;
        }
        Some(SessionsAction::Delete(name)) => {
            if let Err(e) = crate::session::delete_session(&name) {
                app.banner = Some(format!("delete failed: {e}"));
            }
            open_sessions(app, sessions); // reopen with the refreshed list
        }
        Some(SessionsAction::Rename { from, to }) => {
            // Rename = load, re-save under the new name, delete the old file.
            if let Ok(mut s) = crate::session::load_session(&from) {
                s.name = to.clone();
                if crate::session::save_session(&s).is_ok() {
                    let _ = crate::session::delete_session(&from);
                }
            }
            open_sessions(app, sessions);
        }
        None => {}
    }
}

/// Act on a UI command: spawn the matching background task.
fn dispatch(
    cmd: Command,
    client: Option<&'static SubxtChainClient>,
    evt_tx: &mpsc::UnboundedSender<Event>,
    client_tx: &mpsc::UnboundedSender<&'static SubxtChainClient>,
    pinned: Vec<PinnedItem>,
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
        // P2: write raw storage at head (no new block), then refetch pinned items
        // at the current head so the change lands yellow on the tip column.
        Command::SetStorage(req) => {
            if let Some(c) = client {
                let rpc = c.rpc().clone();
                let evt = evt_tx.clone();
                tokio::spawn(async move {
                    // Build the raw edits array `[[keyHex, valueHex]]` (a null value
                    // would delete the key; the editor never sends that yet).
                    let value_json = match &req.value_hex {
                        Some(v) => serde_json::Value::String(v.clone()),
                        None => serde_json::Value::Null,
                    };
                    let edits = serde_json::json!([[req.key_hex, value_json]]);
                    if let Err(e) = dev_rpc::set_storage(&rpc, edits).await {
                        let _ = evt.send(Event::Error(format!("set storage: {e}")));
                        return;
                    }
                    match head_block_ref(c).await {
                        Ok(head) => refetch_at_head(c, head, pinned, evt.clone()).await,
                        Err(e) => {
                            let _ = evt.send(Event::Error(format!("set storage refetch: {e}")));
                        }
                    }
                });
            } else {
                let _ = evt_tx.send(Event::Error("set storage: not connected".into()));
            }
        }
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
        // MVP-2 P4: re-fork the live chain head. The state effect (snapshot +
        // truncate) already ran in the UI loop via `on_command_local`; this arm
        // issues the actual `dev_setHead` RPC.
        Command::SetHead(block) => {
            if let Some(c) = client {
                let rpc = c.rpc().clone();
                let evt = evt_tx.clone();
                tokio::spawn(async move {
                    if let Err(e) = dev_rpc::set_head(&rpc, block).await {
                        let _ = evt.send(Event::Error(format!("set-head: {e}")));
                    }
                });
            }
        }
        // MVP-2 P4: set the chain timestamp.
        Command::TimeTravel(spec) => {
            if let Some(c) = client {
                let rpc = c.rpc().clone();
                let evt = evt_tx.clone();
                tokio::spawn(async move {
                    if let Err(e) = dev_rpc::time_travel(&rpc, &spec).await {
                        let _ = evt.send(Event::Error(format!("time-travel: {e}")));
                    }
                });
            }
        }
        // P4: SaveSession/LoadSession are applied to AppState in the UI loop
        // (via on_command_local); nothing to spawn here.
        Command::SaveSession(_) | Command::LoadSession(_) => {}
    }
}

/// P4 commands carry an in-loop state effect handled by `AppState::on_command_local`
/// before any RPC follow-up is dispatched.
fn is_p4_command(cmd: &Command) -> bool {
    matches!(
        cmd,
        Command::SetHead(_)
            | Command::TimeTravel(_)
            | Command::SaveSession(_)
            | Command::LoadSession(_)
    )
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

/// Resolve the current head as a `BlockRef` (number + hash) by pinning the
/// current block (as `blocks::poll_head` does). After an in-place
/// `dev_setStorage` the head number is unchanged, so the refetched column lands
/// on the tip and diffs yellow vs the previous column (P2).
async fn head_block_ref(
    client: &'static SubxtChainClient,
) -> Result<crate::contracts::BlockRef> {
    let at = client.inner().at_current_block().await?;
    Ok(crate::contracts::BlockRef {
        number: at.block_number() as u32,
        hash: at.block_hash(),
    })
}

/// Fetch every pinned item at `head` and emit the column (same body as
/// `spawn_fetch_column`, awaited inline so the write→refetch is sequential).
async fn refetch_at_head(
    client: &'static SubxtChainClient,
    head: crate::contracts::BlockRef,
    pinned: Vec<PinnedItem>,
    evt_tx: mpsc::UnboundedSender<Event>,
) {
    let inner = client.inner().clone();
    let mut cells = std::collections::BTreeMap::new();
    for item in &pinned {
        let state = storage_fetch::fetch(&inner, item, head.hash)
            .await
            .unwrap_or_else(|e| CellState::Undecodable {
                raw_hex: String::new(),
                error: e.to_string(),
            });
        cells.insert(item.id, state);
    }
    let _ = evt_tx.send(Event::NewColumn(BlockColumn { block: head, cells }));
}

/// Build a [`SetStorageEditor`] wired to the live client + metadata catalog (P2).
///
/// * key resolver: derives the hashed storage key offline from metadata
///   (`storage_fetch::storage_key_hex`) and resolves the entry's value type-id
///   (`storage_fetch::entry_value_type_id`) — both sync, no network round-trip.
/// * text encoder: scale-value text / raw hex → `value_hex`, encoding against the
///   resolved value type-id using the live metadata registry.
/// * tree encoder: re-encodes the edited typed-tree value against the type-id.
fn build_set_storage_editor(
    client: &'static SubxtChainClient,
    catalog: &'static MetadataCatalog<'static>,
) -> SetStorageEditor<'static> {
    use crate::views::set_storage::{encode_raw_hex, encode_scale_text, encode_value};

    let metadata = client.metadata();

    // Key resolver: offline storage-key derivation + value type-id lookup.
    let resolve_key = move |item: &PinnedItem| -> std::result::Result<(String, u32), String> {
        let key_hex = storage_fetch::storage_key_hex(metadata, item).map_err(|e| e.to_string())?;
        let type_id = storage_fetch::entry_value_type_id(metadata, &item.pallet, &item.entry)
            .map_err(|e| e.to_string())?;
        Ok((key_hex, type_id))
    };

    // Text encoder: raw hex needs nothing; scale-value text encodes against the
    // entry's value type-id using the live metadata registry.
    let encode_text = move |mode, raw: &str, type_id: u32| -> std::result::Result<String, String> {
        match mode {
            ValueMode::RawHex => encode_raw_hex(raw),
            ValueMode::ScaleText => encode_scale_text(raw, type_id, metadata.types()),
            ValueMode::Tree => Err("tree mode encodes via Enter, not text".to_string()),
        }
    };

    // Tree encoder: re-encode the whole edited decoded value against the type-id.
    let encode_tree = move |value: &scale_value::Value<u32>,
                            type_id: u32|
          -> std::result::Result<String, String> {
        encode_value(value, type_id, metadata.types())
    };

    SetStorageEditor::new(catalog, resolve_key)
        .with_encoder(encode_text)
        .with_tree_encoder(encode_tree)
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

#[allow(clippy::too_many_arguments)]
fn render(
    f: &mut Frame,
    app: &AppState,
    renderer: &dyn crate::contracts::ValueRenderer,
    picker: Option<&StoragePicker<'static>>,
    tx_builder: Option<&TxBuilder<CuratedCallCatalog>>,
    set_storage_editor: Option<&SetStorageEditor<'static>>,
    build_panel: Option<&BuildPanel>,
    sessions: Option<&crate::views::sessions::SessionsView>,
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
            } else if let Some(n) = app.off_timeline_from {
                // Amber off-timeline badge after a set-head rewind (spec §7.1).
                f.render_widget(
                    Paragraph::new(format!("⏳ off-timeline from #{n}"))
                        .style(ratatui::style::Style::default().fg(ratatui::style::Color::Yellow)),
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
            // The sessions modal takes precedence when open (spec §8).
            if let Some(sv) = sessions {
                let a = centered(area, 70, 70);
                f.render_widget(Clear, a);
                sv.render(f, a);
            } else if let Some(ed) = set_storage_editor {
                let a = centered(area, 70, 70);
                f.render_widget(Clear, a);
                ed.render(f, a);
            } else if let Some(p) = picker {
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
    fn off_timeline_badge_renders_in_grid() {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;
        let mut app = AppState::new();
        app.phase = Phase::Grid;
        app.pinned.push(item(1, "row"));
        app.push_column(column(1030, 1, 1));
        app.off_timeline_from = Some(1030);

        let backend = TestBackend::new(80, 12);
        let mut term = Terminal::new(backend).unwrap();
        let renderer = DefaultRenderer;
        term.draw(|f| render(f, &app, &renderer, None, None, None, None, None)).unwrap();
        let buf = term.backend().buffer().clone();
        let text: String = buf.content().iter().map(|c| c.symbol()).collect();
        assert!(text.contains("from #1030"), "off-timeline badge must render: {text}");
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

    use crate::session::SessionSource;

    #[test]
    fn snapshot_and_truncate_rewinds_columns_and_snapshots_future() {
        use crate::contracts::{BuildMode, ForkConfig};
        let _guard = crate::session::SESSIONS_ENV_LOCK.lock().unwrap();
        let dir = std::env::temp_dir().join(format!("ctui-sh-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        // SAFETY: serialized by SESSIONS_ENV_LOCK.
        unsafe {
            std::env::set_var("CHOPSTICKS_TUI_SESSIONS_DIR", &dir);
        }

        let mut app = AppState::new();
        app.phase = Phase::Grid;
        app.fork = Some(ForkConfig::Spawn {
            chain_or_path: "polkadot".into(),
            build_mode: BuildMode::Manual,
            mock_signature_host: false,
        });
        app.pinned.push(item(1, "row"));
        for n in 1028..=1045 {
            app.push_column(column(n, 1, n as u128));
        }
        assert_eq!(app.columns.back().unwrap().block.number, 1045);

        let snap = app.snapshot_and_truncate(1030).expect("snapshot ok");

        assert_eq!(app.columns.back().unwrap().block.number, 1030);
        assert!(app.columns.iter().all(|c| c.block.number <= 1030));
        assert_eq!(app.off_timeline_from, Some(1030));
        assert!(app.banner.as_deref().unwrap().contains("timeline-#1045"));
        assert!(matches!(app.action_log.last(), Some(RecordedAction::SetHead(1030))));
        assert_eq!(snap, "timeline-#1045");
        let loaded = session::load_session("timeline-#1045").unwrap();
        assert_eq!(loaded.head, 1045);
        assert_eq!(loaded.source, SessionSource::AutoSnapshot);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn set_head_command_truncates_and_emits_followups() {
        use crate::contracts::{BuildMode, ForkConfig};
        let _guard = crate::session::SESSIONS_ENV_LOCK.lock().unwrap();
        let dir = std::env::temp_dir().join(format!("ctui-cmd-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        // SAFETY: serialized by SESSIONS_ENV_LOCK.
        unsafe {
            std::env::set_var("CHOPSTICKS_TUI_SESSIONS_DIR", &dir);
        }

        let mut app = AppState::new();
        app.phase = Phase::Grid;
        app.fork = Some(ForkConfig::Spawn {
            chain_or_path: "polkadot".into(),
            build_mode: BuildMode::Manual,
            mock_signature_host: false,
        });
        for n in 1028..=1045 {
            app.push_column(column(n, 1, n as u128));
        }
        let follow = app.on_command_local(Command::SetHead(1030));
        assert_eq!(app.columns.back().unwrap().block.number, 1030);
        assert!(follow.iter().any(|c| matches!(c, Command::SetHead(1030))));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn save_session_command_persists_named_session() {
        let _guard = crate::session::SESSIONS_ENV_LOCK.lock().unwrap();
        let dir = std::env::temp_dir().join(format!("ctui-save-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        // SAFETY: serialized by SESSIONS_ENV_LOCK.
        unsafe {
            std::env::set_var("CHOPSTICKS_TUI_SESSIONS_DIR", &dir);
        }

        let mut app = AppState::new();
        app.fork = Some(crate::contracts::ForkConfig::Attach { url: "ws://x".into() });
        app.push_column(column(7, 1, 1));
        let follow = app.on_command_local(Command::SaveSession("manual-1".into()));
        assert!(follow.is_empty());
        let loaded = crate::session::load_session("manual-1").unwrap();
        assert_eq!(loaded.name, "manual-1");
        assert_eq!(loaded.head, 7);
        assert_eq!(loaded.source, SessionSource::Manual);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn begin_restore_refork_and_queues_replay() {
        use crate::contracts::{BuildMode, ForkConfig};
        use crate::session::{RecordedAction, Session, SessionSource, TimeSpec};
        let mut app = AppState::new();
        let session = Session {
            name: "repro".into(),
            fork: ForkConfig::Spawn {
                chain_or_path: "polkadot".into(),
                build_mode: BuildMode::Manual,
                mock_signature_host: false,
            },
            pins: vec![item(1, "row")],
            baseline: Some(1000),
            actions: vec![
                RecordedAction::SetHead(1005),
                RecordedAction::TimeTravel(TimeSpec::from_epoch_ms(1_750_000_000_000, "+1d")),
            ],
            head: 1010,
            source: SessionSource::Manual,
        };
        let follow = app.begin_restore(session.clone());
        assert!(matches!(follow.first(), Some(Command::Connect(_))));
        assert_eq!(app.pinned.len(), 1);
        assert_eq!(app.baseline, Some(1000));
        assert_eq!(app.loaded_session.as_deref(), Some("repro"));
        assert_eq!(app.replay_queue.len(), 2);
    }

    #[test]
    fn drain_replay_queue_emits_commands_in_order() {
        use crate::session::{RecordedAction, TimeSpec};
        let mut app = AppState::new();
        app.replay_queue = std::collections::VecDeque::from(vec![
            RecordedAction::TimeTravel(TimeSpec::from_epoch_ms(1_750_000_000_000, "+1d")),
            RecordedAction::SetBuildMode(crate::contracts::BuildMode::Instant),
            RecordedAction::BuiltBlock { extrinsics: vec![], timestamp: None, author: None },
        ]);
        let c1 = app.drain_next_replay();
        assert!(matches!(c1, Some(Command::TimeTravel(_))));
        // SetBuildMode is skipped (P3 owns it) → next is the BuiltBlock build.
        let c2 = app.drain_next_replay();
        assert!(matches!(c2, Some(Command::BuildBlock)));
        assert!(app.replay_queue.is_empty());
        assert!(app.drain_next_replay().is_none());
    }

    #[test]
    fn restore_warns_on_base_drift() {
        let mut app = AppState::new();
        app.expected_restore_head = Some(1010);
        app.note_restore_progress(1008);
        assert!(app.banner.as_deref().unwrap().contains("drift"));
    }

    #[test]
    fn record_appends_to_action_log() {
        use crate::session::RecordedAction;
        let mut app = AppState::new();
        assert!(app.action_log.is_empty());
        app.record(RecordedAction::SetHead(1030));
        app.record(RecordedAction::SetBuildMode(crate::contracts::BuildMode::Instant));
        assert_eq!(app.action_log.len(), 2);
        assert!(matches!(app.action_log[0], RecordedAction::SetHead(1030)));
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
        term.draw(|f| render(f, &app, &DefaultRenderer, None, None, None, None, None)).unwrap();
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
        let mut sse = None;
        let mut bp = None;
        let mut sess = None;
        handle_key(
            press_key(KeyCode::Char(':')),
            &mut app,
            &tx,
            &mut picker,
            &mut txb,
            &mut sse,
            &mut bp,
            &mut sess,
            None,
            None,
        );
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
        let mut sse = None;
        let mut bp = None;
        let mut sess = None;
        handle_key(
            press_key(KeyCode::Char(':')),
            &mut app,
            &tx,
            &mut picker,
            &mut txb,
            &mut sse,
            &mut bp,
            &mut sess,
            None,
            None,
        );
        handle_key(
            press_key(KeyCode::Esc),
            &mut app,
            &tx,
            &mut picker,
            &mut txb,
            &mut sse,
            &mut bp,
            &mut sess,
            None,
            None,
        );
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
        let mut sse = None;
        let mut bp = None;
        let mut sess = None;
        handle_key(
            press_key(KeyCode::Char(':')),
            &mut app,
            &tx,
            &mut picker,
            &mut txb,
            &mut sse,
            &mut bp,
            &mut sess,
            None,
            None,
        );
        assert!(app.palette.is_none(), "palette must not open while connecting");
        assert_eq!(app.mode, Mode::Normal, "mode unchanged while connecting");
    }
}
