//! Storage picker overlay: fuzzy pallet → entry selection, key-input form for
//! map entries, and nested field-path selection — produces a `PinnedItem`
//! (ticket T10).
//!
//! The overlay is a self-contained widget driven purely by key events. It reads
//! from a `&dyn StorageCatalog` and, once the user has fully specified an item,
//! emits `Command::Pin(PinnedItem)`. RPC is never touched here; the caller
//! (T14) assigns the real [`PinnedItemId`].

use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Text};
use ratatui::widgets::{Block, Clear, List, ListItem, Paragraph};

use crate::contracts::{
    Command, DevAccount, KeyArg, PalletInfo, PathSeg, PinnedItem, PinnedItemId, StorageCatalog,
    StorageEntryInfo, StorageKind,
};

/// Placeholder id stamped on emitted items; the caller re-assigns a stable id.
const PLACEHOLDER_ID: PinnedItemId = PinnedItemId(0);

/// The six well-known dev accounts, in menu order.
const DEV_ACCOUNTS: [DevAccount; 6] = [
    DevAccount::Alice,
    DevAccount::Bob,
    DevAccount::Charlie,
    DevAccount::Dave,
    DevAccount::Eve,
    DevAccount::Ferdie,
];

/// Resolve a [`DevAccount`] to its `sr25519` `AccountId32`.
fn dev_account_id(account: DevAccount) -> subxt::utils::AccountId32 {
    use subxt_signer::sr25519::dev;
    let keypair = match account {
        DevAccount::Alice => dev::alice(),
        DevAccount::Bob => dev::bob(),
        DevAccount::Charlie => dev::charlie(),
        DevAccount::Dave => dev::dave(),
        DevAccount::Eve => dev::eve(),
        DevAccount::Ferdie => dev::ferdie(),
    };
    keypair.public_key().to_account_id()
}

fn dev_account_label(account: DevAccount) -> &'static str {
    match account {
        DevAccount::Alice => "Alice",
        DevAccount::Bob => "Bob",
        DevAccount::Charlie => "Charlie",
        DevAccount::Dave => "Dave",
        DevAccount::Eve => "Eve",
        DevAccount::Ferdie => "Ferdie",
    }
}

/// Case-insensitive subsequence ("fuzzy") match: every char of `needle` must
/// appear in `haystack` in order. An empty needle matches everything.
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

/// Which pane / sub-form currently has focus.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Stage {
    /// Browsing pallets and entries with the fuzzy filter.
    Browse,
    /// Filling in map keys, one field per key type-id.
    KeyForm,
    /// Choosing an optional nested field path before pinning.
    PathSelect,
}

/// One field of the key-input form.
#[derive(Debug, Clone)]
struct KeyField {
    /// Metadata type-id of the key (informational / display only).
    type_id: u32,
    /// Whether this field is treated as an AccountId (offers the dev dropdown).
    is_account: bool,
    /// Raw text the user has typed for the SS58/hex/number input.
    raw: String,
    /// Selected dev account, if the dropdown is in use.
    dev: Option<DevAccount>,
}

impl KeyField {
    fn new(type_id: u32, is_account: bool) -> Self {
        Self {
            type_id,
            is_account,
            raw: String::new(),
            dev: None,
        }
    }

    /// Resolve this field to a [`KeyArg`], or `None` if the input is invalid /
    /// incomplete.
    fn resolve(&self) -> Option<KeyArg> {
        if self.is_account {
            if let Some(dev) = self.dev {
                return Some(KeyArg::AccountId(dev_account_id(dev)));
            }
            return parse_account(self.raw.trim());
        }
        parse_scalar(self.raw.trim())
    }
}

/// Parse a raw account field: SS58 first, then 0x-hex of 32 bytes.
fn parse_account(s: &str) -> Option<KeyArg> {
    if s.is_empty() {
        return None;
    }
    if let Ok(id) = s.parse::<subxt::utils::AccountId32>() {
        return Some(KeyArg::AccountId(id));
    }
    let bytes = parse_hex(s)?;
    let arr: [u8; 32] = bytes.try_into().ok()?;
    Some(KeyArg::AccountId(subxt::utils::AccountId32(arr)))
}

/// Parse a raw non-account key field: number → `U`, 0x-hex → `Bytes`.
fn parse_scalar(s: &str) -> Option<KeyArg> {
    if s.is_empty() {
        return None;
    }
    if let Some(hex) = s.strip_prefix("0x") {
        let bytes = parse_hex_body(hex)?;
        return Some(KeyArg::Bytes(bytes));
    }
    if let Ok(n) = s.parse::<u128>() {
        return Some(KeyArg::U(n));
    }
    None
}

/// Parse `0x`-prefixed hex into bytes (the `0x` is required here).
fn parse_hex(s: &str) -> Option<Vec<u8>> {
    let body = s.strip_prefix("0x")?;
    parse_hex_body(body)
}

fn parse_hex_body(body: &str) -> Option<Vec<u8>> {
    if body.is_empty() || !body.len().is_multiple_of(2) {
        return None;
    }
    (0..body.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&body[i..i + 2], 16).ok())
        .collect()
}

/// Parse a dotted path expression (`data.free`, `votes.0`) into [`PathSeg`]s.
/// Numeric segments become [`PathSeg::Index`]; everything else is a field.
fn parse_path(s: &str) -> Vec<PathSeg> {
    s.split('.')
        .filter(|seg| !seg.is_empty())
        .map(|seg| match seg.parse::<u32>() {
            Ok(i) => PathSeg::Index(i),
            Err(_) => PathSeg::Field(seg.to_string()),
        })
        .collect()
}

/// Build the human-readable label, e.g. `System.Account(Alice).data.free`.
fn build_label(
    pallet: &str,
    entry: &str,
    fields: &[KeyField],
    keys: &[KeyArg],
    path: &[PathSeg],
) -> String {
    let mut label = format!("{pallet}.{entry}");
    if !keys.is_empty() {
        let parts: Vec<String> = keys
            .iter()
            .enumerate()
            .map(|(i, key)| key_label(key, fields.get(i)))
            .collect();
        label.push('(');
        label.push_str(&parts.join(", "));
        label.push(')');
    }
    for seg in path {
        match seg {
            PathSeg::Field(name) => {
                label.push('.');
                label.push_str(name);
            }
            PathSeg::Index(i) => {
                label.push('.');
                label.push_str(&i.to_string());
            }
        }
    }
    label
}

/// Pretty label for a single key in the display string. Dev accounts show their
/// name; other args show a compact rendering.
fn key_label(key: &KeyArg, field: Option<&KeyField>) -> String {
    if let Some(dev) = field.and_then(|f| f.dev) {
        return dev_account_label(dev).to_string();
    }
    match key {
        KeyArg::AccountId(id) => id.to_string(),
        KeyArg::U(n) => n.to_string(),
        KeyArg::Bytes(b) => format!("0x{}", hex_string(b)),
        KeyArg::Text(t) => t.clone(),
    }
}

fn hex_string(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push_str(&format!("{b:02x}"));
    }
    s
}

/// The storage picker overlay state machine.
pub struct StoragePicker<'a> {
    catalog: &'a dyn StorageCatalog,
    stage: Stage,

    /// Current fuzzy query (shared text box driving both panes).
    query: String,
    /// All pallets (cached at construction).
    pallets: Vec<PalletInfo>,
    /// Index into the *filtered* pallet list.
    pallet_cursor: usize,
    /// Index into the *filtered* entry list of the selected pallet.
    entry_cursor: usize,

    /// The entry the user has drilled into (set in `KeyForm`/`PathSelect`).
    selected: Option<StorageEntryInfo>,

    /// Key-form fields (one per key type-id), and which field is focused.
    fields: Vec<KeyField>,
    field_cursor: usize,
    /// Whether the dev-account dropdown is currently open for the focused field.
    dropdown_open: bool,
    dropdown_cursor: usize,

    /// Path expression typed in the `PathSelect` stage.
    path_input: String,
}

impl<'a> StoragePicker<'a> {
    /// Create a picker over the given catalog, opening on the pallet list.
    pub fn new(catalog: &'a dyn StorageCatalog) -> Self {
        let pallets = catalog.pallets();
        Self {
            catalog,
            stage: Stage::Browse,
            query: String::new(),
            pallets,
            pallet_cursor: 0,
            entry_cursor: 0,
            selected: None,
            fields: Vec::new(),
            field_cursor: 0,
            dropdown_open: false,
            dropdown_cursor: 0,
            path_input: String::new(),
        }
    }

    // --- Browse-stage derived views -------------------------------------

    /// Pallets surviving the current fuzzy query. A pallet survives if its own
    /// name matches the query *or* any of its entries match — so typing an
    /// entry name narrows the pallet pane down to the pallet(s) that contain a
    /// matching entry.
    fn filtered_pallets(&self) -> Vec<&PalletInfo> {
        self.pallets
            .iter()
            .filter(|p| self.pallet_matches(&p.name))
            .collect()
    }

    fn pallet_matches(&self, name: &str) -> bool {
        if fuzzy_match(name, &self.query) {
            return true;
        }
        self.catalog
            .entries(name)
            .iter()
            .any(|e| fuzzy_match(&e.name, &self.query))
    }

    /// The currently highlighted pallet, if any.
    fn current_pallet(&self) -> Option<&PalletInfo> {
        self.filtered_pallets().get(self.pallet_cursor).copied()
    }

    /// Entries of the highlighted pallet matching the current query. If the
    /// query already matches the pallet name we show all its entries; otherwise
    /// we narrow to entries whose name matches the query.
    fn filtered_entries(&self) -> Vec<StorageEntryInfo> {
        let Some(pallet) = self.current_pallet() else {
            return Vec::new();
        };
        let entries = self.catalog.entries(&pallet.name);
        if fuzzy_match(&pallet.name, &self.query) {
            return entries;
        }
        entries
            .into_iter()
            .filter(|e| fuzzy_match(&e.name, &self.query))
            .collect()
    }

    fn current_entry(&self) -> Option<StorageEntryInfo> {
        self.filtered_entries().into_iter().nth(self.entry_cursor)
    }

    // --- Key event handling ---------------------------------------------

    /// Feed a key event; returns `Some(Command::Pin(..))` once an item is fully
    /// specified, otherwise `None`.
    pub fn on_key(&mut self, key: crossterm::event::KeyEvent) -> Option<Command> {
        if key.kind != crossterm::event::KeyEventKind::Press {
            return None;
        }
        match self.stage {
            Stage::Browse => self.on_key_browse(key.code),
            Stage::KeyForm => self.on_key_form(key.code),
            Stage::PathSelect => self.on_key_path(key.code),
        }
    }

    fn on_key_browse(&mut self, code: crossterm::event::KeyCode) -> Option<Command> {
        use crossterm::event::KeyCode;
        match code {
            KeyCode::Char(c) => {
                self.query.push(c);
                self.pallet_cursor = 0;
                self.entry_cursor = 0;
            }
            KeyCode::Backspace => {
                self.query.pop();
                self.pallet_cursor = 0;
                self.entry_cursor = 0;
            }
            KeyCode::Up => {
                self.pallet_cursor = self.pallet_cursor.saturating_sub(1);
                self.entry_cursor = 0;
            }
            KeyCode::Down => {
                let max = self.filtered_pallets().len().saturating_sub(1);
                self.pallet_cursor = (self.pallet_cursor + 1).min(max);
                self.entry_cursor = 0;
            }
            KeyCode::Tab => {
                let max = self.filtered_entries().len().saturating_sub(1);
                self.entry_cursor = (self.entry_cursor + 1).min(max);
            }
            KeyCode::BackTab => {
                self.entry_cursor = self.entry_cursor.saturating_sub(1);
            }
            KeyCode::Enter => return self.confirm_entry(),
            _ => {}
        }
        None
    }

    /// Drill into the highlighted entry: plain → pin now; map → open key form.
    fn confirm_entry(&mut self) -> Option<Command> {
        let entry = self.current_entry()?;
        match &entry.kind {
            StorageKind::Plain => {
                let label = build_label(&entry.pallet, &entry.name, &[], &[], &[]);
                Some(Command::Pin(PinnedItem {
                    id: PLACEHOLDER_ID,
                    pallet: entry.pallet,
                    entry: entry.name,
                    keys: Vec::new(),
                    path: Vec::new(),
                    label,
                }))
            }
            StorageKind::Map { key_type_ids } => {
                self.fields = key_type_ids
                    .iter()
                    .map(|&tid| KeyField::new(tid, is_account_type(tid)))
                    .collect();
                self.field_cursor = 0;
                self.dropdown_open = false;
                self.dropdown_cursor = 0;
                self.selected = Some(entry);
                self.stage = Stage::KeyForm;
                None
            }
        }
    }

    fn on_key_form(&mut self, code: crossterm::event::KeyCode) -> Option<Command> {
        use crossterm::event::KeyCode;
        if self.dropdown_open {
            return self.on_key_dropdown(code);
        }
        match code {
            KeyCode::Char(c) => {
                if let Some(field) = self.fields.get_mut(self.field_cursor) {
                    field.raw.push(c);
                    field.dev = None;
                }
            }
            KeyCode::Backspace => {
                if let Some(field) = self.fields.get_mut(self.field_cursor) {
                    field.raw.pop();
                    field.dev = None;
                }
            }
            KeyCode::Up => self.field_cursor = self.field_cursor.saturating_sub(1),
            KeyCode::Down | KeyCode::Tab => {
                let max = self.fields.len().saturating_sub(1);
                self.field_cursor = (self.field_cursor + 1).min(max);
            }
            // Open the dev-account dropdown for an AccountId field.
            KeyCode::F(2) => {
                if self
                    .fields
                    .get(self.field_cursor)
                    .is_some_and(|f| f.is_account)
                {
                    self.dropdown_open = true;
                    self.dropdown_cursor = 0;
                }
            }
            KeyCode::Esc => {
                self.stage = Stage::Browse;
                self.selected = None;
            }
            KeyCode::Enter => return self.confirm_keys(),
            _ => {}
        }
        None
    }

    fn on_key_dropdown(&mut self, code: crossterm::event::KeyCode) -> Option<Command> {
        use crossterm::event::KeyCode;
        match code {
            KeyCode::Up => self.dropdown_cursor = self.dropdown_cursor.saturating_sub(1),
            KeyCode::Down => {
                self.dropdown_cursor = (self.dropdown_cursor + 1).min(DEV_ACCOUNTS.len() - 1)
            }
            KeyCode::Enter => {
                let dev = DEV_ACCOUNTS[self.dropdown_cursor];
                if let Some(field) = self.fields.get_mut(self.field_cursor) {
                    field.dev = Some(dev);
                    field.raw.clear();
                }
                self.dropdown_open = false;
            }
            KeyCode::Esc => self.dropdown_open = false,
            _ => {}
        }
        None
    }

    /// Validate all key fields; on success move to path selection.
    fn confirm_keys(&mut self) -> Option<Command> {
        let resolved: Option<Vec<KeyArg>> = self.fields.iter().map(KeyField::resolve).collect();
        // If any field is invalid, stay in the form (caller can show an error).
        let _keys = resolved?;
        self.path_input.clear();
        self.stage = Stage::PathSelect;
        None
    }

    fn on_key_path(&mut self, code: crossterm::event::KeyCode) -> Option<Command> {
        use crossterm::event::KeyCode;
        match code {
            KeyCode::Char(c) => self.path_input.push(c),
            KeyCode::Backspace => {
                self.path_input.pop();
            }
            KeyCode::Esc => {
                // Back to the key form.
                self.stage = Stage::KeyForm;
            }
            KeyCode::Enter => return self.finish(),
            _ => {}
        }
        None
    }

    /// Assemble the final `PinnedItem` and emit the `Pin` command.
    fn finish(&mut self) -> Option<Command> {
        let entry = self.selected.clone()?;
        let keys: Vec<KeyArg> = self.fields.iter().filter_map(KeyField::resolve).collect();
        // Defensive: all fields must resolve (confirm_keys already checked).
        if keys.len() != self.fields.len() {
            return None;
        }
        let path = parse_path(&self.path_input);
        let label = build_label(&entry.pallet, &entry.name, &self.fields, &keys, &path);
        Some(Command::Pin(PinnedItem {
            id: PLACEHOLDER_ID,
            pallet: entry.pallet,
            entry: entry.name,
            keys,
            path,
            label,
        }))
    }

    // --- Rendering -------------------------------------------------------

    /// Draw the overlay into `area` (typically a centered popup rect).
    pub fn render(&self, frame: &mut Frame, area: Rect) {
        frame.render_widget(Clear, area);
        match self.stage {
            Stage::Browse => self.render_browse(frame, area),
            Stage::KeyForm => self.render_key_form(frame, area),
            Stage::PathSelect => self.render_path(frame, area),
        }
    }

    fn render_browse(&self, frame: &mut Frame, area: Rect) {
        let rows = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Length(3), Constraint::Min(1)])
            .split(area);

        let search = Paragraph::new(Line::from(format!("/{}", self.query)))
            .block(Block::bordered().title("Search storage"));
        frame.render_widget(search, rows[0]);

        let panes = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Percentage(40), Constraint::Percentage(60)])
            .split(rows[1]);

        let pallets = self.filtered_pallets();
        let pallet_items: Vec<ListItem> = pallets
            .iter()
            .enumerate()
            .map(|(i, p)| {
                let line = format!("{} ({})", p.name, p.entry_count);
                styled_item(line, i == self.pallet_cursor)
            })
            .collect();
        frame.render_widget(
            List::new(pallet_items).block(Block::bordered().title("Pallets")),
            panes[0],
        );

        let entries = self.filtered_entries();
        let entry_items: Vec<ListItem> = entries
            .iter()
            .enumerate()
            .map(|(i, e)| {
                let kind = match e.kind {
                    StorageKind::Plain => "plain",
                    StorageKind::Map { .. } => "map",
                };
                styled_item(format!("{} [{}]", e.name, kind), i == self.entry_cursor)
            })
            .collect();
        frame.render_widget(
            List::new(entry_items).block(Block::bordered().title("Entries")),
            panes[1],
        );
    }

    fn render_key_form(&self, frame: &mut Frame, area: Rect) {
        let title = self
            .selected
            .as_ref()
            .map(|e| format!("Keys for {}.{}", e.pallet, e.name))
            .unwrap_or_else(|| "Keys".to_string());

        let mut lines: Vec<Line> = Vec::new();
        for (i, field) in self.fields.iter().enumerate() {
            let marker = if i == self.field_cursor { ">" } else { " " };
            let kind = if field.is_account {
                "AccountId"
            } else {
                "raw"
            };
            let value = if let Some(dev) = field.dev {
                dev_account_label(dev).to_string()
            } else {
                field.raw.clone()
            };
            let status = match field.resolve() {
                Some(_) => "ok",
                None => "invalid",
            };
            lines.push(Line::from(format!(
                "{marker} key{i} ({kind}, type {}): {value}  [{status}]",
                field.type_id
            )));
        }
        if self
            .fields
            .get(self.field_cursor)
            .is_some_and(|f| f.is_account)
        {
            lines.push(Line::from("F2: pick dev account · Enter: confirm"));
        }

        let para = Paragraph::new(Text::from(lines)).block(Block::bordered().title(title));
        frame.render_widget(para, area);

        if self.dropdown_open {
            self.render_dropdown(frame, area);
        }
    }

    fn render_dropdown(&self, frame: &mut Frame, area: Rect) {
        let height = (DEV_ACCOUNTS.len() as u16 + 2).min(area.height);
        let rect = Rect {
            x: area.x + 2,
            y: area.y + 2,
            width: area.width.saturating_sub(4).min(24),
            height,
        };
        frame.render_widget(Clear, rect);
        let items: Vec<ListItem> = DEV_ACCOUNTS
            .iter()
            .enumerate()
            .map(|(i, a)| styled_item(dev_account_label(*a).to_string(), i == self.dropdown_cursor))
            .collect();
        frame.render_widget(
            List::new(items).block(Block::bordered().title("Dev account")),
            rect,
        );
    }

    fn render_path(&self, frame: &mut Frame, area: Rect) {
        let title = self
            .selected
            .as_ref()
            .map(|e| format!("Field path for {}.{}", e.pallet, e.name))
            .unwrap_or_else(|| "Field path".to_string());
        let body = Text::from(vec![
            Line::from(format!("path: {}", self.path_input)),
            Line::from("e.g. data.free · empty = whole value · Enter to pin"),
        ]);
        frame.render_widget(
            Paragraph::new(body).block(Block::bordered().title(title)),
            area,
        );
    }
}

fn styled_item(text: String, selected: bool) -> ListItem<'static> {
    let style = if selected {
        Style::default().add_modifier(Modifier::REVERSED)
    } else {
        Style::default()
    };
    ListItem::new(Line::from(text)).style(style)
}

/// Heuristic: is a key type-id an AccountId? Without the metadata registry here
/// we treat the well-known AccountId32 type-id (0) specially via a const, but in
/// MVP-1 the caller can pre-flag this. For now, type-id 0 is the AccountId
/// convention used by `MockCatalog` and tests.
fn is_account_type(type_id: u32) -> bool {
    type_id == ACCOUNT_TYPE_ID
}

/// Conventional metadata type-id used for `AccountId32` keys in MVP-1 fixtures.
pub const ACCOUNT_TYPE_ID: u32 = 0;

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};

    /// A hand-built catalog for tests.
    struct MockCatalog {
        pallets: Vec<PalletInfo>,
        entries: std::collections::HashMap<String, Vec<StorageEntryInfo>>,
    }

    impl MockCatalog {
        fn new() -> Self {
            let mut entries = std::collections::HashMap::new();
            entries.insert(
                "System".to_string(),
                vec![
                    StorageEntryInfo {
                        pallet: "System".to_string(),
                        name: "Account".to_string(),
                        kind: StorageKind::Map {
                            key_type_ids: vec![ACCOUNT_TYPE_ID],
                        },
                        value_type_id: 10,
                        docs: "account info".to_string(),
                    },
                    StorageEntryInfo {
                        pallet: "System".to_string(),
                        name: "Number".to_string(),
                        kind: StorageKind::Plain,
                        value_type_id: 4,
                        docs: "block number".to_string(),
                    },
                ],
            );
            entries.insert(
                "Balances".to_string(),
                vec![StorageEntryInfo {
                    pallet: "Balances".to_string(),
                    name: "TotalIssuance".to_string(),
                    kind: StorageKind::Plain,
                    value_type_id: 6,
                    docs: "total issuance".to_string(),
                }],
            );
            entries.insert(
                "Timestamp".to_string(),
                vec![StorageEntryInfo {
                    pallet: "Timestamp".to_string(),
                    name: "Now".to_string(),
                    kind: StorageKind::Map {
                        // a non-account numeric key
                        key_type_ids: vec![99],
                    },
                    value_type_id: 8,
                    docs: "now".to_string(),
                }],
            );
            Self {
                pallets: vec![
                    PalletInfo {
                        name: "System".to_string(),
                        entry_count: 2,
                    },
                    PalletInfo {
                        name: "Balances".to_string(),
                        entry_count: 1,
                    },
                    PalletInfo {
                        name: "Timestamp".to_string(),
                        entry_count: 1,
                    },
                ],
                entries,
            }
        }
    }

    impl StorageCatalog for MockCatalog {
        fn pallets(&self) -> Vec<PalletInfo> {
            self.pallets.clone()
        }
        fn entries(&self, pallet: &str) -> Vec<StorageEntryInfo> {
            self.entries.get(pallet).cloned().unwrap_or_default()
        }
        fn entry(&self, pallet: &str, entry: &str) -> Option<StorageEntryInfo> {
            self.entries
                .get(pallet)?
                .iter()
                .find(|e| e.name == entry)
                .cloned()
        }
    }

    fn press(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn typ(picker: &mut StoragePicker, s: &str) {
        for c in s.chars() {
            picker.on_key(press(KeyCode::Char(c)));
        }
    }

    #[test]
    fn key_event_kind_release_is_ignored() {
        let cat = MockCatalog::new();
        let mut p = StoragePicker::new(&cat);
        let mut ev = press(KeyCode::Char('x'));
        ev.kind = KeyEventKind::Release;
        p.on_key(ev);
        assert_eq!(p.query, "");
    }

    #[test]
    fn fuzzy_filter_narrows_pallets_then_entries() {
        let cat = MockCatalog::new();
        let mut p = StoragePicker::new(&cat);
        assert_eq!(p.filtered_pallets().len(), 3);

        // "sys" narrows to System (subsequence match).
        typ(&mut p, "sys");
        let pallets = p.filtered_pallets();
        assert_eq!(pallets.len(), 1);
        assert_eq!(pallets[0].name, "System");

        // Clear and use a subsequence that also narrows entries: "Num" should
        // keep System (Account/Number) and match the Number entry.
        for _ in 0.."sys".len() {
            p.on_key(press(KeyCode::Backspace));
        }
        typ(&mut p, "Num");
        // System has a "Number" entry; only it should survive the entry filter.
        assert_eq!(p.current_pallet().map(|x| x.name.as_str()), Some("System"));
        let entries = p.filtered_entries();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].name, "Number");
    }

    #[test]
    fn plain_entry_emits_pin_with_empty_keys() {
        let cat = MockCatalog::new();
        let mut p = StoragePicker::new(&cat);
        // Filter to System.Number (a plain entry) and confirm.
        typ(&mut p, "Number");
        let cmd = p.on_key(press(KeyCode::Enter)).expect("should pin");
        match cmd {
            Command::Pin(item) => {
                assert_eq!(item.pallet, "System");
                assert_eq!(item.entry, "Number");
                assert!(item.keys.is_empty());
                assert!(item.path.is_empty());
                assert_eq!(item.label, "System.Number");
            }
            other => panic!("expected Pin, got {other:?}"),
        }
    }

    #[test]
    fn map_entry_opens_key_form_with_one_field_per_key() {
        let cat = MockCatalog::new();
        let mut p = StoragePicker::new(&cat);
        // Select System.Account (a map with one AccountId key).
        typ(&mut p, "Account");
        let cmd = p.on_key(press(KeyCode::Enter));
        assert!(cmd.is_none(), "map entry should not pin immediately");
        assert_eq!(p.stage, Stage::KeyForm);
        assert_eq!(p.fields.len(), 1);
        assert!(p.fields[0].is_account);
    }

    #[test]
    fn dev_account_dropdown_yields_accountid_keyarg() {
        let cat = MockCatalog::new();
        let mut p = StoragePicker::new(&cat);
        typ(&mut p, "Account");
        p.on_key(press(KeyCode::Enter)); // into key form

        // Open dropdown, move to Bob (index 1), select.
        p.on_key(press(KeyCode::F(2)));
        assert!(p.dropdown_open);
        p.on_key(press(KeyCode::Down));
        p.on_key(press(KeyCode::Enter));
        assert!(!p.dropdown_open);

        let resolved = p.fields[0].resolve().expect("field resolves");
        match resolved {
            KeyArg::AccountId(id) => {
                assert_eq!(id, dev_account_id(DevAccount::Bob));
            }
            other => panic!("expected AccountId, got {other:?}"),
        }

        // Confirm keys → path stage → pin with the dev label.
        p.on_key(press(KeyCode::Enter)); // confirm keys
        assert_eq!(p.stage, Stage::PathSelect);
        let cmd = p.on_key(press(KeyCode::Enter)).expect("pin");
        match cmd {
            Command::Pin(item) => {
                assert_eq!(item.keys.len(), 1);
                assert_eq!(item.keys[0], KeyArg::AccountId(dev_account_id(DevAccount::Bob)));
                assert_eq!(item.label, "System.Account(Bob)");
            }
            other => panic!("expected Pin, got {other:?}"),
        }
    }

    #[test]
    fn raw_accountid_ss58_yields_accountid_keyarg() {
        let alice = dev_account_id(DevAccount::Alice);
        let ss58 = alice.to_string();
        let parsed = parse_account(&ss58).expect("ss58 parses");
        assert_eq!(parsed, KeyArg::AccountId(alice));
    }

    #[test]
    fn raw_numeric_key_parses_to_u_keyarg() {
        let cat = MockCatalog::new();
        let mut p = StoragePicker::new(&cat);
        // Timestamp.Now is a map with one non-account numeric key.
        typ(&mut p, "Now");
        p.on_key(press(KeyCode::Enter));
        assert_eq!(p.stage, Stage::KeyForm);
        assert!(!p.fields[0].is_account);

        // Valid number.
        typ(&mut p, "42");
        assert_eq!(p.fields[0].resolve(), Some(KeyArg::U(42)));

        let cmd = p.on_key(press(KeyCode::Enter)); // confirm keys
        assert!(cmd.is_none());
        assert_eq!(p.stage, Stage::PathSelect);
    }

    #[test]
    fn invalid_numeric_key_is_rejected() {
        let cat = MockCatalog::new();
        let mut p = StoragePicker::new(&cat);
        typ(&mut p, "Now");
        p.on_key(press(KeyCode::Enter));

        // "abc" is neither a number nor 0x-hex → invalid → stays in form.
        typ(&mut p, "abc");
        assert_eq!(p.fields[0].resolve(), None);
        let cmd = p.on_key(press(KeyCode::Enter));
        assert!(cmd.is_none());
        assert_eq!(p.stage, Stage::KeyForm, "invalid key must not advance");
    }

    #[test]
    fn raw_hex_key_parses_to_bytes_keyarg() {
        assert_eq!(parse_scalar("0xdeadbeef"), Some(KeyArg::Bytes(vec![0xde, 0xad, 0xbe, 0xef])));
        assert_eq!(parse_scalar("0xabc"), None, "odd-length hex rejected");
    }

    #[test]
    fn nested_path_selection_sets_path_and_label() {
        let cat = MockCatalog::new();
        let mut p = StoragePicker::new(&cat);
        typ(&mut p, "Account");
        p.on_key(press(KeyCode::Enter)); // key form
        p.on_key(press(KeyCode::F(2))); // dropdown
        p.on_key(press(KeyCode::Enter)); // pick Alice (index 0)
        p.on_key(press(KeyCode::Enter)); // confirm keys → path stage
        assert_eq!(p.stage, Stage::PathSelect);

        typ(&mut p, "data.free");
        let cmd = p.on_key(press(KeyCode::Enter)).expect("pin");
        match cmd {
            Command::Pin(item) => {
                assert_eq!(
                    item.path,
                    vec![PathSeg::Field("data".to_string()), PathSeg::Field("free".to_string())]
                );
                assert_eq!(item.label, "System.Account(Alice).data.free");
            }
            other => panic!("expected Pin, got {other:?}"),
        }
    }

    #[test]
    fn nested_path_supports_index_segments() {
        assert_eq!(
            parse_path("votes.0.weight"),
            vec![
                PathSeg::Field("votes".to_string()),
                PathSeg::Index(0),
                PathSeg::Field("weight".to_string()),
            ]
        );
    }
}
