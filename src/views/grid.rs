//! Storage grid widget: pinned items (rows) × successive blocks (columns), with
//! auto-follow and diff highlighting (ticket T09). Renders from a borrowed view
//! model; owns no data and issues no RPC.

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Cell, Row, Table, Widget};

use crate::contracts::CellDiff;

/// Where the pinned baseline column sits relative to the live grid window.
///
/// - `Off`: no baseline pinned (vs-previous mode).
/// - `Live`: the baseline column is present in the ring buffer.
/// - `Pending`: a baseline number is set but no matching column is held yet
///   (e.g. pinned a future block, or it has not streamed in).
/// - `Evicted`: the baseline column has fallen off the front of the ring buffer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BaselineState {
    Off,
    Live,
    Pending,
    Evicted,
}

/// Glyph shown when a cell could not be decoded.
pub const UNDECODABLE_GLYPH: &str = "⚠ <undecodable>";

/// One pre-rendered cell: the caller renders the value to a `String` (the grid
/// never decodes), tagged with the diff state for styling.
#[derive(Debug, Clone)]
pub struct GridCell {
    /// The already-rendered display text. Ignored when `undecodable` is set.
    pub text: String,
    /// Diff classification driving the highlight.
    pub diff: CellDiff,
    /// When true the cell renders [`UNDECODABLE_GLYPH`] regardless of `text`.
    pub undecodable: bool,
}

impl GridCell {
    /// A plain, unchanged cell with the given text.
    pub fn text(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            diff: CellDiff::Unchanged,
            undecodable: false,
        }
    }

    /// A cell whose value could not be decoded.
    pub fn undecodable() -> Self {
        Self {
            text: String::new(),
            diff: CellDiff::Unchanged,
            undecodable: true,
        }
    }
}

/// One row of the grid: a pinned item's label plus its cell per visible column.
///
/// `cells` is index-aligned with [`GridViewModel::columns`].
#[derive(Debug, Clone)]
pub struct GridRow {
    /// Left-gutter label (a `PinnedItem.label`).
    pub label: String,
    /// Pre-rendered cells, one per visible column, left → right (oldest → newest).
    pub cells: Vec<GridCell>,
}

/// Everything the grid needs to draw a frame. The widget is stateless given
/// this; the caller owns scrolling/windowing decisions.
#[derive(Debug, Clone)]
pub struct GridViewModel {
    /// All rows (pinned items). Only a vertical slice is shown — see `scroll`.
    pub rows: Vec<GridRow>,
    /// Block numbers for the visible columns, left → right (oldest → newest).
    pub columns: Vec<u32>,
    /// First visible row index (vertical scroll offset).
    pub scroll: usize,
    /// Index of the first visible column within the full block history. Purely
    /// informational for the widget; `columns`/row `cells` are already windowed.
    pub column_window_start: usize,
    /// Whether the grid is auto-following the chain tip.
    pub follow: bool,
    /// Baseline block number, if baseline mode is active.
    pub baseline_block: Option<u32>,
    /// Where the baseline column sits relative to the window (drives the badge).
    pub baseline_state: BaselineState,
    /// A frozen render of the baseline column's cells, anchored left when the
    /// baseline column is not already inside the visible window. One cell per
    /// row, index-aligned with `rows`. `None` unless `baseline_state == Live`
    /// and the baseline column is outside the window.
    pub baseline_column: Option<Vec<GridCell>>,
}

impl GridViewModel {
    /// An empty grid (no rows, no columns), following.
    pub fn empty() -> Self {
        Self {
            rows: Vec::new(),
            columns: Vec::new(),
            scroll: 0,
            column_window_start: 0,
            follow: true,
            baseline_block: None,
            baseline_state: BaselineState::Off,
            baseline_column: None,
        }
    }
}

const LABEL_WIDTH: u16 = 28;
const COL_WIDTH: u16 = 14;

/// Stateless storage-grid widget rendering a [`GridViewModel`].
pub struct GridView<'a> {
    model: &'a GridViewModel,
}

impl<'a> GridView<'a> {
    pub fn new(model: &'a GridViewModel) -> Self {
        Self { model }
    }

    /// Style for a cell given its diff classification.
    fn cell_style(diff: &CellDiff) -> Style {
        match diff {
            CellDiff::Unchanged => Style::default().fg(Color::Gray),
            CellDiff::Changed { .. } => Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
            CellDiff::Added => Style::default()
                .fg(Color::Green)
                .add_modifier(Modifier::BOLD),
            CellDiff::Removed => Style::default()
                .fg(Color::Red)
                .add_modifier(Modifier::CROSSED_OUT),
        }
    }

    /// Display text for a cell, including the diff marker prefix.
    fn cell_text(cell: &GridCell) -> String {
        if cell.undecodable {
            return UNDECODABLE_GLYPH.to_string();
        }
        match &cell.diff {
            CellDiff::Added => format!("+ {}", cell.text),
            CellDiff::Removed => "✗ removed".to_string(),
            CellDiff::Changed { .. } | CellDiff::Unchanged => cell.text.clone(),
        }
    }
}

impl Widget for GridView<'_> {
    fn render(self, area: Rect, buf: &mut Buffer) {
        let model = self.model;

        // Header: block numbers, newest on the right, plus a follow badge and
        // (when active) a coexisting purple baseline badge.
        let badge = if model.follow { "FOLLOW" } else { "PAUSED" };
        let badge_style = if model.follow {
            Style::default()
                .fg(Color::Black)
                .bg(Color::Green)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default()
                .fg(Color::Black)
                .bg(Color::Yellow)
                .add_modifier(Modifier::BOLD)
        };

        let mut title_spans = vec![
            Span::raw("Storage grid "),
            Span::styled(format!(" {badge} "), badge_style),
        ];
        // Baseline badge (purple), coexisting with FOLLOW/PAUSED.
        if model.baseline_state != BaselineState::Off {
            let label = match model.baseline_state {
                BaselineState::Live => {
                    format!(" ★ BASELINE #{} ", model.baseline_block.unwrap_or(0))
                }
                BaselineState::Pending => {
                    format!(" ★ BASELINE #{} (pending) ", model.baseline_block.unwrap_or(0))
                }
                BaselineState::Evicted => {
                    format!(" ★ BASELINE #{} (gone) ", model.baseline_block.unwrap_or(0))
                }
                BaselineState::Off => unreachable!("guarded by the != Off check"),
            };
            title_spans.push(Span::raw(" "));
            title_spans.push(Span::styled(
                label,
                Style::default()
                    .fg(Color::Black)
                    .bg(Color::Magenta)
                    .add_modifier(Modifier::BOLD),
            ));
        }
        let title = Line::from(title_spans);
        let block = Block::bordered().title(title);

        // Whether to draw the frozen baseline anchor column on the far left.
        let frozen = model.baseline_column.as_ref();
        let divider_style = Style::default()
            .fg(Color::Magenta)
            .add_modifier(Modifier::BOLD);

        // Header cells: gutter label, then (optional) frozen baseline header +
        // purple divider, then each live block number.
        let mut header_cells: Vec<Cell> = vec![Cell::from("item")];
        if frozen.is_some() {
            header_cells.push(
                Cell::from(format!("★#{}", model.baseline_block.unwrap_or(0)))
                    .style(divider_style),
            );
            header_cells.push(Cell::from("║").style(divider_style));
        }
        header_cells.extend(
            model
                .columns
                .iter()
                .map(|n| Cell::from(format!("#{n}")).style(Style::default().add_modifier(Modifier::BOLD))),
        );
        let header = Row::new(header_cells)
            .style(Style::default().add_modifier(Modifier::BOLD))
            .bottom_margin(0);

        // Body rows: apply the vertical scroll offset. Each row gets the same
        // optional frozen-cell + divider prefix as the header.
        let body_rows = model.rows.iter().enumerate().skip(model.scroll).map(|(i, row)| {
            let label = Cell::from(Span::styled(
                row.label.clone(),
                Style::default().add_modifier(Modifier::BOLD),
            ));
            let mut cells: Vec<Cell> = vec![label];
            if let Some(frozen_cells) = frozen {
                let fc = frozen_cells
                    .get(i)
                    .cloned()
                    .unwrap_or_else(|| GridCell::text(""));
                cells.push(Cell::from(Self::cell_text(&fc)).style(Self::cell_style(&fc.diff)));
                cells.push(Cell::from("║").style(divider_style));
            }
            cells.extend(
                row.cells
                    .iter()
                    .map(|c| Cell::from(Self::cell_text(c)).style(Self::cell_style(&c.diff))),
            );
            Row::new(cells)
        });

        // Widths: gutter, then (optional) frozen col + divider, then live cols.
        let mut widths = Vec::with_capacity(model.columns.len() + 3);
        widths.push(ratatui::layout::Constraint::Length(LABEL_WIDTH));
        if frozen.is_some() {
            widths.push(ratatui::layout::Constraint::Length(COL_WIDTH));
            widths.push(ratatui::layout::Constraint::Length(1)); // divider
        }
        widths.extend(
            model
                .columns
                .iter()
                .map(|_| ratatui::layout::Constraint::Length(COL_WIDTH)),
        );

        let table = Table::new(body_rows, widths)
            .header(header)
            .block(block)
            .column_spacing(1);

        Widget::render(table, area, buf);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use ratatui::buffer::Buffer;

    /// Render a model into a `w×h` test buffer and return the resulting buffer.
    fn render(model: &GridViewModel, w: u16, h: u16) -> Buffer {
        let backend = TestBackend::new(w, h);
        let mut terminal = Terminal::new(backend).expect("terminal");
        terminal
            .draw(|frame| frame.render_widget(GridView::new(model), frame.area()))
            .expect("draw");
        terminal.backend().buffer().clone()
    }

    /// Flatten the whole buffer to a single string of symbols.
    fn buffer_text(buf: &Buffer) -> String {
        buf.content().iter().map(|c| c.symbol()).collect()
    }

    /// Find the first cell whose symbol equals `needle`'s first char run; here we
    /// locate the (x, y) of the first cell that starts the given substring on its
    /// row. Returns the cell at the match start.
    fn find_cell_xy(buf: &Buffer, needle: &str) -> (u16, u16) {
        let area = buf.area;
        for y in 0..area.height {
            let mut row = String::new();
            for x in 0..area.width {
                row.push_str(buf.cell((x, y)).unwrap().symbol());
            }
            if let Some(byte_idx) = row.find(needle) {
                // Map byte index → column index (cells are 1 char wide mostly;
                // count chars before the match).
                let col = row[..byte_idx].chars().count() as u16;
                return (col, y);
            }
        }
        panic!("needle {needle:?} not found in buffer:\n{}", {
            let mut s = String::new();
            for y in 0..area.height {
                for x in 0..area.width {
                    s.push_str(buf.cell((x, y)).unwrap().symbol());
                }
                s.push('\n');
            }
            s
        });
    }

    fn model_with(rows: Vec<GridRow>, columns: Vec<u32>) -> GridViewModel {
        GridViewModel {
            rows,
            columns,
            scroll: 0,
            column_window_start: 0,
            follow: true,
            baseline_block: None,
            baseline_state: BaselineState::Off,
            baseline_column: None,
        }
    }

    #[test]
    fn renders_row_labels_and_block_headers() {
        let model = model_with(
            vec![
                GridRow {
                    label: "System.Number".to_string(),
                    cells: vec![GridCell::text("100"), GridCell::text("101")],
                },
                GridRow {
                    label: "Balances.Issuance".to_string(),
                    cells: vec![GridCell::text("9000"), GridCell::text("9001")],
                },
            ],
            vec![100, 101],
        );
        let buf = render(&model, 80, 12);
        let text = buffer_text(&buf);

        assert!(text.contains("System.Number"), "missing first row label");
        assert!(
            text.contains("Balances.Issuance"),
            "missing second row label"
        );
        assert!(text.contains("#100"), "missing block header #100");
        assert!(text.contains("#101"), "missing block header #101");
    }

    #[test]
    fn changed_cell_is_styled_distinctly() {
        let model = model_with(
            vec![GridRow {
                label: "row".to_string(),
                cells: vec![
                    GridCell::text("AAA"),
                    GridCell {
                        text: "BBB".to_string(),
                        diff: CellDiff::Changed {
                            from: "AAA".to_string(),
                            to: "BBB".to_string(),
                        },
                        undecodable: false,
                    },
                ],
            }],
            vec![1, 2],
        );
        let buf = render(&model, 80, 8);

        let (ux, uy) = find_cell_xy(&buf, "AAA");
        let (cx, cy) = find_cell_xy(&buf, "BBB");
        let unchanged = buf.cell((ux, uy)).unwrap();
        let changed = buf.cell((cx, cy)).unwrap();

        assert_ne!(
            (changed.fg, changed.modifier),
            (unchanged.fg, unchanged.modifier),
            "Changed cell must be styled differently from Unchanged"
        );
        assert!(
            changed.modifier.contains(Modifier::BOLD),
            "Changed cell should be bold"
        );
    }

    #[test]
    fn undecodable_cell_shows_warning_glyph() {
        let model = model_with(
            vec![GridRow {
                label: "row".to_string(),
                cells: vec![GridCell::undecodable()],
            }],
            vec![7],
        );
        let buf = render(&model, 80, 8);
        let text = buffer_text(&buf);
        assert!(
            text.contains("⚠"),
            "undecodable cell must show the warning glyph; buffer:\n{text}"
        );
    }

    #[test]
    fn follow_badge_reflects_view_model() {
        let mut model = model_with(
            vec![GridRow {
                label: "row".to_string(),
                cells: vec![GridCell::text("x")],
            }],
            vec![1],
        );

        model.follow = true;
        let following = buffer_text(&render(&model, 80, 8));
        assert!(following.contains("FOLLOW"), "expected FOLLOW badge");
        assert!(!following.contains("PAUSED"), "unexpected PAUSED badge");

        model.follow = false;
        let paused = buffer_text(&render(&model, 80, 8));
        assert!(paused.contains("PAUSED"), "expected PAUSED badge");
        assert!(!paused.contains("FOLLOW"), "unexpected FOLLOW badge");
    }

    #[test]
    fn baseline_badge_coexists_with_follow() {
        let mut model = model_with(
            vec![GridRow {
                label: "row".to_string(),
                cells: vec![GridCell::text("x")],
            }],
            vec![1043],
        );
        model.follow = true;
        model.baseline_block = Some(1042);
        model.baseline_state = BaselineState::Live;
        let text = buffer_text(&render(&model, 100, 8));
        assert!(text.contains("FOLLOW"), "FOLLOW badge must still show");
        assert!(text.contains("BASELINE #1042"), "baseline badge must show #n");
        assert!(text.contains("★"), "baseline badge must show the star glyph");
    }

    #[test]
    fn baseline_badge_shows_pending_and_gone() {
        let mut model = model_with(
            vec![GridRow {
                label: "row".to_string(),
                cells: vec![GridCell::text("x")],
            }],
            vec![1043],
        );
        model.baseline_block = Some(2000);
        model.baseline_state = BaselineState::Pending;
        let pending = buffer_text(&render(&model, 100, 8));
        assert!(pending.contains("pending"), "pending baseline must be flagged");

        model.baseline_state = BaselineState::Evicted;
        let gone = buffer_text(&render(&model, 100, 8));
        assert!(gone.contains("gone"), "evicted baseline must be flagged");
    }

    #[test]
    fn frozen_baseline_column_renders_with_divider() {
        let mut model = model_with(
            vec![GridRow {
                label: "row".to_string(),
                cells: vec![GridCell::text("now")],
            }],
            vec![1099],
        );
        model.baseline_block = Some(1042);
        model.baseline_state = BaselineState::Live;
        // Frozen column: one cell per row, anchored left.
        model.baseline_column = Some(vec![GridCell::text("base")]);
        let text = buffer_text(&render(&model, 120, 8));
        assert!(text.contains("base"), "frozen baseline cell value must render");
        assert!(text.contains("║"), "purple divider must render");
        // The frozen header marks the baseline block.
        assert!(text.contains("★#1042"), "frozen column header must mark the baseline");
    }

    #[test]
    fn no_baseline_renders_no_badge_or_divider() {
        let model = model_with(
            vec![GridRow {
                label: "row".to_string(),
                cells: vec![GridCell::text("x")],
            }],
            vec![1],
        );
        let text = buffer_text(&render(&model, 100, 8));
        assert!(!text.contains("BASELINE"), "no baseline → no badge");
        assert!(!text.contains("║"), "no baseline → no divider");
    }

    #[test]
    fn baseline_state_default_is_off() {
        let model = model_with(
            vec![GridRow {
                label: "row".to_string(),
                cells: vec![GridCell::text("x")],
            }],
            vec![1],
        );
        // model_with builds a default (vs-previous) model: no baseline.
        assert_eq!(model.baseline_block, None);
        assert_eq!(model.baseline_state, BaselineState::Off);
        assert!(model.baseline_column.is_none());
    }

    #[test]
    fn respects_vertical_scroll_offset() {
        let rows = vec![
            GridRow {
                label: "ROW_ALPHA".to_string(),
                cells: vec![GridCell::text("a")],
            },
            GridRow {
                label: "ROW_BETA".to_string(),
                cells: vec![GridCell::text("b")],
            },
            GridRow {
                label: "ROW_GAMMA".to_string(),
                cells: vec![GridCell::text("c")],
            },
        ];
        let mut model = model_with(rows, vec![1]);

        // No scroll: first row visible.
        model.scroll = 0;
        let top = buffer_text(&render(&model, 80, 8));
        assert!(top.contains("ROW_ALPHA"), "alpha should be visible at scroll 0");

        // Scroll past the first two rows: alpha/beta gone, gamma visible.
        model.scroll = 2;
        let scrolled = buffer_text(&render(&model, 80, 8));
        assert!(
            !scrolled.contains("ROW_ALPHA"),
            "alpha should be scrolled out of view"
        );
        assert!(
            !scrolled.contains("ROW_BETA"),
            "beta should be scrolled out of view"
        );
        assert!(
            scrolled.contains("ROW_GAMMA"),
            "gamma should be visible after scrolling"
        );
    }
}
