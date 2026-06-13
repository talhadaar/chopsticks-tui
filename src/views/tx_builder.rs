//! Transaction builder overlay: account selection (dev or impersonate),
//! pallet → call → typed args, encoded-call preview — produces a `PreparedTx`
//! (ticket T11).
//!
//! This view is pure UI: it renders from its own selection state and emits
//! [`Command::SubmitTx`]. It never touches RPC and never submits — submission
//! lands in T12. The pallet/call/arg metadata is read through the injectable
//! [`CallCatalog`] seam so the view can be unit-tested against a mock catalog
//! without live runtime metadata.

use std::fmt::Write as _;

use crossterm::event::{KeyCode, KeyEvent};
use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, List, ListItem, Paragraph, Wrap};
use scale_value::Value;
use subxt::utils::AccountId32;

use crate::contracts::{Command, DevAccount, PreparedTx, TxOutcome, TxSigner};

// ---------------------------------------------------------------------------
// Call catalog seam
// ---------------------------------------------------------------------------

/// The scalar shape of a single call argument. Each variant knows how to parse a
/// user-typed string into a `scale_value::Value<()>` for the dynamic tx API.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ArgKind {
    /// Unsigned integer (`u8`…`u128`, balances, indices).
    U128,
    /// Boolean (`true`/`false`).
    Bool,
    /// SS58 account address, parsed to an `AccountId32` byte composite.
    AccountId,
    /// Free-form text / bytes.
    Text,
}

impl ArgKind {
    /// Parse a user-entered string into a `scale_value::Value<()>`, or return a
    /// human-readable error explaining why it did not parse.
    pub fn parse(self, raw: &str) -> std::result::Result<Value<()>, String> {
        let trimmed = raw.trim();
        match self {
            ArgKind::U128 => trimmed
                .parse::<u128>()
                .map(Value::u128)
                .map_err(|_| format!("`{trimmed}` is not a valid unsigned integer")),
            ArgKind::Bool => match trimmed {
                "true" | "yes" | "1" => Ok(Value::bool(true)),
                "false" | "no" | "0" => Ok(Value::bool(false)),
                _ => Err(format!("`{trimmed}` is not a boolean (true/false)")),
            },
            ArgKind::AccountId => trimmed
                .parse::<AccountId32>()
                .map(|id| Value::from_bytes(id.0))
                .map_err(|_| format!("`{trimmed}` is not a valid SS58 address")),
            ArgKind::Text => Ok(Value::string(trimmed.to_string())),
        }
    }
}

/// One named argument of a call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArgSpec {
    pub name: String,
    pub kind: ArgKind,
}

impl ArgSpec {
    pub fn new(name: impl Into<String>, kind: ArgKind) -> Self {
        Self {
            name: name.into(),
            kind,
        }
    }
}

/// A browsable view over the runtime's dispatchable calls. The real impl (T12 /
/// metadata) wraps subxt metadata; tests inject a hand-rolled catalog.
pub trait CallCatalog {
    /// Pallet names that expose at least one call, in display order.
    fn pallets(&self) -> Vec<String>;
    /// Call names within a pallet, in display order.
    fn calls(&self, pallet: &str) -> Vec<String>;
    /// Ordered argument specs for a `pallet.call`.
    fn args(&self, pallet: &str, call: &str) -> Vec<ArgSpec>;
}

// ---------------------------------------------------------------------------
// Signer selection
// ---------------------------------------------------------------------------

/// All dev accounts, in selector order.
const DEV_ACCOUNTS: [DevAccount; 6] = [
    DevAccount::Alice,
    DevAccount::Bob,
    DevAccount::Charlie,
    DevAccount::Dave,
    DevAccount::Eve,
    DevAccount::Ferdie,
];

fn dev_account_name(acc: DevAccount) -> &'static str {
    match acc {
        DevAccount::Alice => "Alice",
        DevAccount::Bob => "Bob",
        DevAccount::Charlie => "Charlie",
        DevAccount::Dave => "Dave",
        DevAccount::Eve => "Eve",
        DevAccount::Ferdie => "Ferdie",
    }
}

/// Which signing mode the account selector is in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SignerMode {
    /// One of the well-known `//Name` dev accounts.
    Dev,
    /// An arbitrary `AccountId32` (requires `--mock-signature-host`).
    Impersonate,
}

/// The note surfaced whenever impersonation is selected.
pub const MOCK_HOST_NOTE: &str =
    "Impersonation requires the fork spawned with --mock-signature-host.";

// ---------------------------------------------------------------------------
// Builder state machine
// ---------------------------------------------------------------------------

/// Which part of the overlay currently has focus / accepts keys.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Focus {
    /// Choosing dev-vs-impersonate and the concrete signer.
    Signer,
    /// Choosing the pallet.
    Pallet,
    /// Choosing the call.
    Call,
    /// Filling in the typed argument form.
    Args,
}

/// Whether the builder's terminal action submits the tx or stages it for a
/// caller (the MVP-2 build panel) to collect.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TxBuilderMode {
    /// Emit `Command::SubmitTx` (default — the `t` standalone flow).
    Submit,
    /// Park the `PreparedTx` for the build panel via `take_staged`.
    Stage,
}

/// The guided transaction builder overlay.
///
/// Generic over a [`CallCatalog`] so production code passes a metadata-backed
/// catalog while tests pass a mock. Drive it with [`TxBuilder::on_key`], feed
/// the asynchronously-delivered result back via [`TxBuilder::set_result`], and
/// draw it with [`TxBuilder::render`].
pub struct TxBuilder<C: CallCatalog> {
    catalog: C,

    focus: Focus,

    // Signer selection.
    signer_mode: SignerMode,
    dev_index: usize,
    impersonate_input: String,

    // Call selection.
    pallet_index: usize,
    call_index: usize,

    // Argument form: one input buffer per arg of the selected call, plus the
    // index of the field being edited.
    arg_inputs: Vec<String>,
    arg_field: usize,

    /// Last submission outcome, once T12 delivers it.
    result: Option<TxOutcome>,

    /// Terminal-action behaviour: submit vs stage (MVP-2 P3).
    mode: TxBuilderMode,
    /// In `Stage` mode, the most recently staged tx awaiting collection.
    staged: Option<PreparedTx>,
}

impl<C: CallCatalog> TxBuilder<C> {
    /// Build a fresh overlay over the given catalog. Defaults to dev `Alice` and
    /// the first pallet/call the catalog reports.
    pub fn new(catalog: C) -> Self {
        let mut me = Self {
            catalog,
            focus: Focus::Signer,
            signer_mode: SignerMode::Dev,
            dev_index: 0,
            impersonate_input: String::new(),
            pallet_index: 0,
            call_index: 0,
            arg_inputs: Vec::new(),
            arg_field: 0,
            result: None,
            mode: TxBuilderMode::Submit,
            staged: None,
        };
        me.sync_arg_inputs();
        me
    }

    /// Like [`TxBuilder::new`] but the terminal action stages the tx instead of
    /// submitting it (the build-panel `a`-flow). MVP-2 P3.
    pub fn new_staging(catalog: C) -> Self {
        let mut me = Self::new(catalog);
        me.mode = TxBuilderMode::Stage;
        me
    }

    /// Take the staged `PreparedTx`, if the user completed one in `Stage` mode.
    pub fn take_staged(&mut self) -> Option<PreparedTx> {
        self.staged.take()
    }

    // -- derived selection -------------------------------------------------

    fn current_pallet(&self) -> Option<String> {
        self.catalog.pallets().into_iter().nth(self.pallet_index)
    }

    fn current_call(&self) -> Option<String> {
        let pallet = self.current_pallet()?;
        self.catalog.calls(&pallet).into_iter().nth(self.call_index)
    }

    fn current_arg_specs(&self) -> Vec<ArgSpec> {
        match (self.current_pallet(), self.current_call()) {
            (Some(p), Some(c)) => self.catalog.args(&p, &c),
            _ => Vec::new(),
        }
    }

    /// Resize the per-arg input buffers to match the selected call, preserving
    /// nothing (selection just changed, so old inputs no longer apply).
    fn sync_arg_inputs(&mut self) {
        let n = self.current_arg_specs().len();
        self.arg_inputs = vec![String::new(); n];
        self.arg_field = 0;
    }

    /// The selected signer, if it is currently valid.
    fn signer(&self) -> Option<TxSigner> {
        match self.signer_mode {
            SignerMode::Dev => Some(TxSigner::Dev(DEV_ACCOUNTS[self.dev_index])),
            SignerMode::Impersonate => self
                .impersonate_input
                .trim()
                .parse::<AccountId32>()
                .ok()
                .map(TxSigner::Impersonate),
        }
    }

    /// Parse every arg field; `Ok` only if all parse.
    fn parse_args(&self) -> std::result::Result<Vec<Value<()>>, String> {
        self.current_arg_specs()
            .iter()
            .zip(&self.arg_inputs)
            .map(|(spec, raw)| {
                spec.kind
                    .parse(raw)
                    .map_err(|e| format!("{}: {e}", spec.name))
            })
            .collect()
    }

    /// Build a one-line decoded summary like `Balances.transfer(1000000, …)`.
    fn decoded_summary(&self, args: &[Value<()>]) -> String {
        let pallet = self.current_pallet().unwrap_or_default();
        let call = self.current_call().unwrap_or_default();
        let mut out = format!("{pallet}.{call}(");
        for (i, v) in args.iter().enumerate() {
            if i > 0 {
                out.push_str(", ");
            }
            let _ = write!(out, "{v}");
        }
        out.push(')');
        out
    }

    /// A deterministic hex preview of the call. Real SCALE encoding needs the
    /// runtime type registry (only available in T12); for the builder preview we
    /// derive a stable hex fingerprint from the call identity and rendered args
    /// so the user sees a concrete, changing preview as they edit.
    fn encoded_hex(&self, args: &[Value<()>]) -> String {
        let summary = self.decoded_summary(args);
        let mut out = String::from("0x");
        for b in summary.as_bytes() {
            let _ = write!(out, "{b:02x}");
        }
        out
    }

    /// Assemble a [`PreparedTx`] from the current selection, if it is complete.
    fn build_prepared(&self) -> std::result::Result<PreparedTx, String> {
        let signer = self
            .signer()
            .ok_or_else(|| "no valid signer selected".to_string())?;
        let pallet = self
            .current_pallet()
            .ok_or_else(|| "no pallet selected".to_string())?;
        let call = self
            .current_call()
            .ok_or_else(|| "no call selected".to_string())?;
        let args = self.parse_args()?;
        let encoded_preview = format!("{}  {}", self.encoded_hex(&args), self.decoded_summary(&args));
        Ok(PreparedTx {
            pallet,
            call,
            args,
            signer,
            encoded_preview,
        })
    }

    // -- input handling ----------------------------------------------------

    /// Handle one key press. Returns `Some(Command)` only when the user submits
    /// a complete transaction.
    pub fn on_key(&mut self, key: KeyEvent) -> Option<Command> {
        // A delivered result acts as a dismissable panel; any key clears it and
        // returns focus to editing.
        if self.result.is_some() {
            self.result = None;
            return None;
        }

        match self.focus {
            Focus::Signer => self.on_key_signer(key),
            Focus::Pallet => self.on_key_pallet(key),
            Focus::Call => self.on_key_call(key),
            Focus::Args => return self.on_key_args(key),
        }
        None
    }

    fn on_key_signer(&mut self, key: KeyEvent) {
        match key.code {
            // Toggle dev / impersonate.
            KeyCode::Tab | KeyCode::Char('i') if self.impersonate_input.is_empty() => {
                self.signer_mode = match self.signer_mode {
                    SignerMode::Dev => SignerMode::Impersonate,
                    SignerMode::Impersonate => SignerMode::Dev,
                };
            }
            KeyCode::Up if self.signer_mode == SignerMode::Dev => {
                if self.dev_index == 0 {
                    self.dev_index = DEV_ACCOUNTS.len() - 1;
                } else {
                    self.dev_index -= 1;
                }
            }
            KeyCode::Down if self.signer_mode == SignerMode::Dev => {
                self.dev_index = (self.dev_index + 1) % DEV_ACCOUNTS.len();
            }
            KeyCode::Char(c) if self.signer_mode == SignerMode::Impersonate => {
                self.impersonate_input.push(c);
            }
            KeyCode::Backspace if self.signer_mode == SignerMode::Impersonate => {
                self.impersonate_input.pop();
            }
            KeyCode::Enter => self.focus = Focus::Pallet,
            _ => {}
        }
    }

    fn on_key_pallet(&mut self, key: KeyEvent) {
        let len = self.catalog.pallets().len().max(1);
        match key.code {
            KeyCode::Up => {
                self.pallet_index = (self.pallet_index + len - 1) % len;
                self.call_index = 0;
                self.sync_arg_inputs();
            }
            KeyCode::Down => {
                self.pallet_index = (self.pallet_index + 1) % len;
                self.call_index = 0;
                self.sync_arg_inputs();
            }
            KeyCode::Enter => self.focus = Focus::Call,
            KeyCode::Esc => self.focus = Focus::Signer,
            _ => {}
        }
    }

    fn on_key_call(&mut self, key: KeyEvent) {
        let len = self
            .current_pallet()
            .map(|p| self.catalog.calls(&p).len())
            .unwrap_or(0)
            .max(1);
        match key.code {
            KeyCode::Up => {
                self.call_index = (self.call_index + len - 1) % len;
                self.sync_arg_inputs();
            }
            KeyCode::Down => {
                self.call_index = (self.call_index + 1) % len;
                self.sync_arg_inputs();
            }
            KeyCode::Enter => self.focus = Focus::Args,
            KeyCode::Esc => self.focus = Focus::Pallet,
            _ => {}
        }
    }

    fn on_key_args(&mut self, key: KeyEvent) -> Option<Command> {
        let n = self.arg_inputs.len();
        match key.code {
            KeyCode::Esc => {
                self.focus = Focus::Call;
                None
            }
            KeyCode::Up if n > 0 => {
                self.arg_field = (self.arg_field + n - 1) % n;
                None
            }
            KeyCode::Down | KeyCode::Tab if n > 0 => {
                self.arg_field = (self.arg_field + 1) % n;
                None
            }
            KeyCode::Char(c) if n > 0 => {
                self.arg_inputs[self.arg_field].push(c);
                None
            }
            KeyCode::Backspace if n > 0 => {
                self.arg_inputs[self.arg_field].pop();
                None
            }
            KeyCode::Enter => self.submit(),
            _ => None,
        }
    }

    /// Try to assemble the transaction. In `Submit` mode, returns
    /// `Some(Command::SubmitTx)`; in `Stage` mode, parks the tx in `self.staged`
    /// and returns `None`. Returns `None` (overlay stays open) if incomplete or
    /// an arg fails to parse.
    fn submit(&mut self) -> Option<Command> {
        match self.build_prepared() {
            Ok(tx) => match self.mode {
                TxBuilderMode::Submit => Some(Command::SubmitTx(tx)),
                TxBuilderMode::Stage => {
                    self.staged = Some(tx);
                    None
                }
            },
            Err(_) => None,
        }
    }

    // -- result panel ------------------------------------------------------

    /// Record the outcome of a submitted transaction (delivered by T12). The
    /// next key press dismisses the result panel.
    pub fn set_result(&mut self, outcome: TxOutcome) {
        self.result = Some(outcome);
    }

    // -- rendering ---------------------------------------------------------

    /// Draw the overlay into `area`. Pure: reads state only.
    pub fn render(&self, frame: &mut Frame, area: Rect) {
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(9), // signer
                Constraint::Min(6),    // call + args
                Constraint::Length(4), // preview
                Constraint::Length(6), // result / help
            ])
            .split(area);

        self.render_signer(frame, chunks[0]);
        self.render_call(frame, chunks[1]);
        self.render_preview(frame, chunks[2]);
        self.render_result(frame, chunks[3]);
    }

    fn render_signer(&self, frame: &mut Frame, area: Rect) {
        let mut lines: Vec<Line> = Vec::new();
        let dev_marker = if self.signer_mode == SignerMode::Dev {
            "[x]"
        } else {
            "[ ]"
        };
        let imp_marker = if self.signer_mode == SignerMode::Impersonate {
            "[x]"
        } else {
            "[ ]"
        };
        lines.push(Line::from(format!("{dev_marker} Dev account")));
        if self.signer_mode == SignerMode::Dev {
            for (i, acc) in DEV_ACCOUNTS.iter().enumerate() {
                let sel = if i == self.dev_index { ">" } else { " " };
                lines.push(Line::from(format!("   {sel} //{}", dev_account_name(*acc))));
            }
        }
        lines.push(Line::from(format!("{imp_marker} Impersonate (AccountId32)")));
        if self.signer_mode == SignerMode::Impersonate {
            lines.push(Line::from(format!("   addr: {}", self.impersonate_input)));
            lines.push(Line::from(Span::styled(
                MOCK_HOST_NOTE,
                Style::default().fg(Color::Yellow),
            )));
        }

        let block = Block::default()
            .borders(Borders::ALL)
            .title("Signer")
            .border_style(self.border_style(Focus::Signer));
        frame.render_widget(Paragraph::new(lines).block(block), area);
    }

    fn render_call(&self, frame: &mut Frame, area: Rect) {
        let cols = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([
                Constraint::Percentage(30),
                Constraint::Percentage(30),
                Constraint::Percentage(40),
            ])
            .split(area);

        // Pallets.
        let pallets = self.catalog.pallets();
        let pallet_items: Vec<ListItem> = pallets
            .iter()
            .enumerate()
            .map(|(i, p)| self.list_item(p, i == self.pallet_index, self.focus == Focus::Pallet))
            .collect();
        frame.render_widget(
            List::new(pallet_items).block(
                Block::default()
                    .borders(Borders::ALL)
                    .title("Pallet")
                    .border_style(self.border_style(Focus::Pallet)),
            ),
            cols[0],
        );

        // Calls.
        let calls = self
            .current_pallet()
            .map(|p| self.catalog.calls(&p))
            .unwrap_or_default();
        let call_items: Vec<ListItem> = calls
            .iter()
            .enumerate()
            .map(|(i, c)| self.list_item(c, i == self.call_index, self.focus == Focus::Call))
            .collect();
        frame.render_widget(
            List::new(call_items).block(
                Block::default()
                    .borders(Borders::ALL)
                    .title("Call")
                    .border_style(self.border_style(Focus::Call)),
            ),
            cols[1],
        );

        // Arg form.
        let specs = self.current_arg_specs();
        let mut arg_lines: Vec<Line> = Vec::new();
        for (i, spec) in specs.iter().enumerate() {
            let sel = if self.focus == Focus::Args && i == self.arg_field {
                ">"
            } else {
                " "
            };
            let raw = self.arg_inputs.get(i).map(String::as_str).unwrap_or("");
            let parse_note = match spec.kind.parse(raw) {
                Ok(_) if !raw.trim().is_empty() => Span::styled(" ok", Style::default().fg(Color::Green)),
                Ok(_) => Span::raw(""),
                Err(_) => Span::styled(" !", Style::default().fg(Color::Red)),
            };
            arg_lines.push(Line::from(vec![
                Span::raw(format!("{sel} {} ({:?}): ", spec.name, spec.kind)),
                Span::raw(raw.to_string()),
                parse_note,
            ]));
        }
        if specs.is_empty() {
            arg_lines.push(Line::from("(no arguments)"));
        }
        frame.render_widget(
            Paragraph::new(arg_lines).wrap(Wrap { trim: false }).block(
                Block::default()
                    .borders(Borders::ALL)
                    .title("Arguments")
                    .border_style(self.border_style(Focus::Args)),
            ),
            cols[2],
        );
    }

    fn render_preview(&self, frame: &mut Frame, area: Rect) {
        let text = match self.build_prepared() {
            Ok(tx) => tx.encoded_preview,
            Err(e) => format!("(incomplete) {e}"),
        };
        frame.render_widget(
            Paragraph::new(text)
                .wrap(Wrap { trim: false })
                .block(Block::default().borders(Borders::ALL).title("Encoded preview")),
            area,
        );
    }

    fn render_result(&self, frame: &mut Frame, area: Rect) {
        let block = Block::default().borders(Borders::ALL).title("Result");
        let lines: Vec<Line> = match &self.result {
            None => vec![Line::from(
                "Tab: toggle signer · ↑↓: select · Enter: next/submit · Esc: back",
            )],
            Some(outcome) => {
                let mut ls = Vec::new();
                if outcome.success {
                    ls.push(Line::from(Span::styled(
                        "SUCCESS",
                        Style::default().fg(Color::Green).add_modifier(Modifier::BOLD),
                    )));
                } else {
                    ls.push(Line::from(Span::styled(
                        "FAILED",
                        Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
                    )));
                    if let Some(err) = &outcome.error {
                        ls.push(Line::from(Span::styled(
                            format!("error: {err}"),
                            Style::default().fg(Color::Red),
                        )));
                    }
                }
                for ev in &outcome.events {
                    ls.push(Line::from(format!(
                        "{}.{} {}",
                        ev.pallet, ev.variant, ev.fields
                    )));
                }
                ls
            }
        };
        frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }).block(block), area);
    }

    fn border_style(&self, focus: Focus) -> Style {
        if self.focus == focus {
            Style::default().fg(Color::Cyan)
        } else {
            Style::default()
        }
    }

    fn list_item<'a>(&self, label: &'a str, selected: bool, focused: bool) -> ListItem<'a> {
        let marker = if selected { "> " } else { "  " };
        let mut style = Style::default();
        if selected && focused {
            style = style.fg(Color::Cyan).add_modifier(Modifier::BOLD);
        } else if selected {
            style = style.add_modifier(Modifier::BOLD);
        }
        ListItem::new(Line::from(Span::styled(format!("{marker}{label}"), style)))
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::{KeyEvent, KeyEventKind, KeyModifiers};
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    /// A hand-rolled catalog: two pallets, with a `Balances.transfer_keep_alive`
    /// call taking `(dest: AccountId, value: u128)`.
    struct MockCatalog;

    impl CallCatalog for MockCatalog {
        fn pallets(&self) -> Vec<String> {
            vec!["Balances".into(), "System".into()]
        }
        fn calls(&self, pallet: &str) -> Vec<String> {
            match pallet {
                "Balances" => vec!["transfer_keep_alive".into(), "force_transfer".into()],
                "System" => vec!["remark".into()],
                _ => vec![],
            }
        }
        fn args(&self, pallet: &str, call: &str) -> Vec<ArgSpec> {
            match (pallet, call) {
                ("Balances", "transfer_keep_alive") => vec![
                    ArgSpec::new("dest", ArgKind::AccountId),
                    ArgSpec::new("value", ArgKind::U128),
                ],
                ("System", "remark") => vec![ArgSpec::new("remark", ArgKind::Text)],
                _ => vec![],
            }
        }
    }

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent {
            code,
            modifiers: KeyModifiers::NONE,
            kind: KeyEventKind::Press,
            state: crossterm::event::KeyEventState::NONE,
        }
    }

    fn typed(b: &mut TxBuilder<MockCatalog>, s: &str) {
        for c in s.chars() {
            b.on_key(key(KeyCode::Char(c)));
        }
    }

    // A valid sr25519 dev address (Alice) for impersonation parsing.
    const ALICE_SS58: &str = "5GrwvaEF5zXb26Fz9rcQpDWS57CtERHpNehXCPcNoHGKutQY";

    /// Navigate to `Balances.transfer_keep_alive` with valid args, ready to submit.
    fn ready_to_submit(b: &mut TxBuilder<MockCatalog>) {
        // Signer: leave as Dev/Alice. Move to pallet.
        b.on_key(key(KeyCode::Enter)); // Signer -> Pallet
        b.on_key(key(KeyCode::Enter)); // Pallet (Balances) -> Call
        b.on_key(key(KeyCode::Enter)); // Call (transfer_keep_alive) -> Args
        // Fill dest then value.
        typed(b, ALICE_SS58);
        b.on_key(key(KeyCode::Down)); // next field
        typed(b, "1000000");
    }

    #[test]
    fn dev_account_selection_sets_dev_signer() {
        let mut b = TxBuilder::new(MockCatalog);
        // Move down twice: Alice -> Bob -> Charlie.
        b.on_key(key(KeyCode::Down));
        b.on_key(key(KeyCode::Down));
        ready_to_submit_after_signer(&mut b);

        let cmd = b.on_key(key(KeyCode::Enter)).expect("submit emits command");
        let Command::SubmitTx(tx) = cmd else {
            panic!("expected SubmitTx");
        };
        assert_eq!(tx.signer, TxSigner::Dev(DevAccount::Charlie));
    }

    /// Like `ready_to_submit` but assumes signer already configured (caller moved
    /// within the signer step). Advances from Signer through to filled args.
    fn ready_to_submit_after_signer(b: &mut TxBuilder<MockCatalog>) {
        b.on_key(key(KeyCode::Enter)); // Signer -> Pallet
        b.on_key(key(KeyCode::Enter)); // Pallet -> Call
        b.on_key(key(KeyCode::Enter)); // Call -> Args
        typed(b, ALICE_SS58);
        b.on_key(key(KeyCode::Down));
        typed(b, "1000000");
    }

    #[test]
    fn impersonate_sets_impersonate_signer_and_warns_about_mock_host() {
        let mut b = TxBuilder::new(MockCatalog);
        // Toggle to impersonate, type the address.
        b.on_key(key(KeyCode::Tab));
        typed(&mut b, ALICE_SS58);
        ready_to_submit_after_signer(&mut b);

        let cmd = b.on_key(key(KeyCode::Enter)).expect("submit emits command");
        let Command::SubmitTx(tx) = cmd else {
            panic!("expected SubmitTx");
        };
        let expected: AccountId32 = ALICE_SS58.parse().unwrap();
        assert_eq!(tx.signer, TxSigner::Impersonate(expected));

        // The mock-host requirement is surfaced in the rendered UI.
        let backend = TestBackend::new(80, 40);
        let mut term = Terminal::new(backend).unwrap();
        // Re-enter impersonate mode for the render (submit cleared nothing here).
        let mut b2 = TxBuilder::new(MockCatalog);
        b2.on_key(key(KeyCode::Tab));
        term.draw(|f| b2.render(f, f.area())).unwrap();
        let buf = term.backend().buffer().clone();
        let dump: String = buf.content().iter().map(|c| c.symbol()).collect();
        assert!(
            dump.contains("--mock-signature-host"),
            "mock host note must be visible, got: {dump}"
        );
    }

    #[test]
    fn arg_form_parses_fields_into_scale_values() {
        let mut b = TxBuilder::new(MockCatalog);
        ready_to_submit(&mut b);
        let args = b.parse_args().expect("args parse");
        assert_eq!(args.len(), 2);
        // value field is u128(1000000)
        assert_eq!(args[1], Value::u128(1_000_000));
        // dest field is a 32-byte unnamed composite (AccountId32 bytes)
        let expected: AccountId32 = ALICE_SS58.parse().unwrap();
        assert_eq!(args[0], Value::from_bytes(expected.0));
    }

    #[test]
    fn arg_form_rejects_unparseable_field() {
        let mut b = TxBuilder::new(MockCatalog);
        b.on_key(key(KeyCode::Enter)); // -> Pallet
        b.on_key(key(KeyCode::Enter)); // -> Call (transfer_keep_alive)
        b.on_key(key(KeyCode::Enter)); // -> Args
        typed(&mut b, "not-an-address");
        b.on_key(key(KeyCode::Down));
        typed(&mut b, "not-a-number");
        assert!(b.parse_args().is_err());
        // Submit must NOT emit when args are invalid.
        assert!(b.on_key(key(KeyCode::Enter)).is_none());
    }

    #[test]
    fn preview_reflects_pallet_call_and_args() {
        let mut b = TxBuilder::new(MockCatalog);
        ready_to_submit(&mut b);
        let tx = b.build_prepared().expect("prepared");
        assert_eq!(tx.pallet, "Balances");
        assert_eq!(tx.call, "transfer_keep_alive");
        assert!(tx.encoded_preview.starts_with("0x"));
        assert!(
            tx.encoded_preview.contains("Balances.transfer_keep_alive"),
            "preview summary must name the call: {}",
            tx.encoded_preview
        );
        assert!(
            tx.encoded_preview.contains("1000000"),
            "preview summary must reflect args: {}",
            tx.encoded_preview
        );
    }

    #[test]
    fn submit_emits_command_with_prepared_tx() {
        let mut b = TxBuilder::new(MockCatalog);
        ready_to_submit(&mut b);
        let cmd = b.on_key(key(KeyCode::Enter)).expect("submit emits command");
        match cmd {
            Command::SubmitTx(tx) => {
                assert_eq!(tx.pallet, "Balances");
                assert_eq!(tx.call, "transfer_keep_alive");
                assert_eq!(tx.args.len(), 2);
                assert_eq!(tx.signer, TxSigner::Dev(DevAccount::Alice));
            }
            other => panic!("expected SubmitTx, got {other:?}"),
        }
    }

    #[test]
    fn stage_mode_returns_prepared_tx_without_emitting_submit() {
        let mut b = TxBuilder::new_staging(MockCatalog);
        ready_to_submit(&mut b);
        // In Stage mode the terminal action does NOT emit a Command...
        assert!(b.on_key(key(KeyCode::Enter)).is_none());
        // ...it parks the PreparedTx for the caller to take.
        let staged = b.take_staged().expect("staged tx available");
        assert_eq!(staged.pallet, "Balances");
        assert_eq!(staged.call, "transfer_keep_alive");
        assert_eq!(staged.args.len(), 2);
        assert_eq!(staged.signer, TxSigner::Dev(DevAccount::Alice));
        // Taken once, gone.
        assert!(b.take_staged().is_none());
    }

    #[test]
    fn failure_outcome_renders_error_inline() {
        use crate::contracts::EventSummary;
        let mut b = TxBuilder::new(MockCatalog);
        b.set_result(TxOutcome {
            success: false,
            events: vec![EventSummary {
                pallet: "System".into(),
                variant: "ExtrinsicFailed".into(),
                fields: "{}".into(),
            }],
            error: Some("BadOrigin".into()),
        });

        let backend = TestBackend::new(80, 40);
        let mut term = Terminal::new(backend).unwrap();
        term.draw(|f| b.render(f, f.area())).unwrap();
        let buf = term.backend().buffer().clone();
        let dump: String = buf.content().iter().map(|c| c.symbol()).collect();
        assert!(dump.contains("FAILED"), "must show failure: {dump}");
        assert!(dump.contains("BadOrigin"), "must show error inline: {dump}");
    }

    #[test]
    fn result_panel_is_dismissed_on_next_key() {
        let mut b = TxBuilder::new(MockCatalog);
        b.set_result(TxOutcome {
            success: true,
            events: vec![],
            error: None,
        });
        assert!(b.result.is_some());
        // Any key dismisses and does not emit a command.
        assert!(b.on_key(key(KeyCode::Enter)).is_none());
        assert!(b.result.is_none());
    }
}
