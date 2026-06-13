//! Set-storage editor overlay (MVP-2, plan P2).
//!
//! Composes the MVP-1 [`StoragePicker`](crate::views::picker::StoragePicker) for
//! target selection (pallet → entry → keys) with a value editor that produces a
//! [`SetStorageReq`](crate::contracts::SetStorageReq). Two value-editing modes:
//!
//! * **Free-text** (always available): a scale-value text expression (parsed by
//!   `scale_value::stringify::from_str`) or a raw `0x`-prefixed SCALE hex string.
//! * **Typed field-tree**: decode the current value into editable leaves.
//!
//! The editor owns *all* encoding: it turns the chosen value into a
//! `0x`-prefixed `value_hex` and pairs it with the storage `key_hex` the caller
//! supplies, so the RPC layer (`dev_rpc::set_storage`) is a thin passthrough.

use crossterm::event::{KeyCode, KeyEvent, KeyEventKind};
use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, Borders, Clear, Paragraph, Wrap};
use scale_value::Value;
use scale_value::scale::encode_as_type;

use crate::contracts::{Command, PinnedItem, SetStorageReq, StorageCatalog};
use crate::views::picker::StoragePicker;

/// Which value-editing mode the value stage is in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ValueMode {
    /// Typed field-tree: edit decoded leaves (default when a current value is
    /// seeded). Cycles to `ScaleText` on `x`.
    Tree,
    /// scale-value text expression (free-text).
    ScaleText,
    /// raw `0x` SCALE hex.
    RawHex,
}

/// Which stage of the set-storage flow has focus.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Stage {
    /// Choosing the target via the embedded `StoragePicker`.
    Target,
    /// Editing the value for the chosen target.
    Value,
}

/// Resolves a chosen target [`PinnedItem`] to its hashed storage key hex and the
/// value type-id (so the editor can encode scale-value text against it). Returns
/// an error string the editor surfaces inline. The app supplies a real
/// implementation backed by subxt; tests pass a closure.
type KeyResolver<'a> =
    dyn Fn(&PinnedItem) -> std::result::Result<(String, u32), String> + 'a;

/// Encodes the user's value input (per mode) against the target's value type-id
/// into `0x`-hex. The app supplies a scale-value-aware encoder; the default only
/// handles raw hex.
type TextEncoder<'a> =
    dyn Fn(ValueMode, &str, u32) -> std::result::Result<String, String> + 'a;

/// Encodes a fully-edited decoded value (typed-tree mode) against its type-id
/// into `0x`-hex. The app wires this with the live metadata registry; the
/// default errors (tree mode needs metadata).
type TreeEncoder<'a> =
    dyn Fn(&Value<u32>, u32) -> std::result::Result<String, String> + 'a;

/// The set-storage editor overlay: a target stage (embedded [`StoragePicker`])
/// followed by a value stage that produces a [`Command::SetStorage`].
pub struct SetStorageEditor<'a> {
    stage: Stage,
    picker: StoragePicker<'a>,
    /// The chosen target, set when leaving the Target stage.
    target: Option<PinnedItem>,
    /// Resolved hashed storage key for the target (filled on target-confirm).
    key_hex: Option<String>,
    /// Resolved value type-id for the target (filled on target-confirm).
    value_type_id: Option<u32>,

    mode: ValueMode,
    /// Free-text buffer (scale-value text or raw hex).
    input: String,
    /// Last error to surface inline (parse/encode/key-resolve).
    error: Option<String>,

    /// Typed-tree state: the working decoded value, its flattened leaves, and the
    /// focused leaf. Empty/`None` until `with_current_value` seeds them.
    tree_value: Option<Value<u32>>,
    leaves: Vec<Leaf>,
    leaf_cursor: usize,
    /// Edit buffer for the focused leaf (Tree mode).
    leaf_input: String,
    /// Whether the focused leaf is being edited (vs. just navigated).
    editing_leaf: bool,

    resolve_key: Box<KeyResolver<'a>>,
    encode_text: Box<TextEncoder<'a>>,
    encode_tree: Box<TreeEncoder<'a>>,
}

impl<'a> SetStorageEditor<'a> {
    /// Create an editor over `catalog`, using `resolve_key` to turn the chosen
    /// target into its hashed storage key hex + value type-id. Uses a default
    /// encoder that only supports raw hex (the app overrides via
    /// [`with_encoder`](Self::with_encoder) to add scale-value text encoding
    /// against live metadata).
    pub fn new(
        catalog: &'a dyn StorageCatalog,
        resolve_key: impl Fn(&PinnedItem) -> std::result::Result<(String, u32), String> + 'a,
    ) -> Self {
        Self {
            stage: Stage::Target,
            picker: StoragePicker::new(catalog),
            target: None,
            key_hex: None,
            value_type_id: None,
            mode: ValueMode::ScaleText,
            input: String::new(),
            error: None,
            tree_value: None,
            leaves: Vec::new(),
            leaf_cursor: 0,
            leaf_input: String::new(),
            editing_leaf: false,
            resolve_key: Box::new(resolve_key),
            encode_text: Box::new(|mode, raw, _ty| match mode {
                ValueMode::RawHex => encode_raw_hex(raw),
                _ => Err("scale-value text needs metadata (press `x` for raw hex)".to_string()),
            }),
            encode_tree: Box::new(|_v, _ty| {
                Err("typed-tree encoding needs metadata".to_string())
            }),
        }
    }

    /// Override the text encoder with one that has access to the metadata
    /// registry (the app supplies this). The closure receives the mode, the raw
    /// input, and the chosen entry's value type-id, and returns `0x`-hex or an
    /// error string.
    pub fn with_encoder(
        mut self,
        encode_text: impl Fn(ValueMode, &str, u32) -> std::result::Result<String, String> + 'a,
    ) -> Self {
        self.encode_text = Box::new(encode_text);
        self
    }

    /// Override the typed-tree encoder with one that has the live metadata
    /// registry (the app supplies this). Receives the edited decoded value + the
    /// value type-id and returns `0x`-hex.
    pub fn with_tree_encoder(
        mut self,
        encode_tree: impl Fn(&Value<u32>, u32) -> std::result::Result<String, String> + 'a,
    ) -> Self {
        self.encode_tree = Box::new(encode_tree);
        self
    }

    /// Seed the typed-tree mode with the target's current decoded value. When
    /// `None`, the editor stays in free-text mode (e.g. the key doesn't exist
    /// yet — spec §5 "keys that don't exist yet").
    pub fn with_current_value(mut self, value: Option<Value<u32>>) -> Self {
        if let Some(v) = value {
            self.leaves = flatten_leaves(&v);
            self.tree_value = Some(v);
            self.mode = ValueMode::Tree;
        }
        self
    }

    /// True once a target is chosen and the value stage is active (test hook).
    pub fn in_value_stage(&self) -> bool {
        self.stage == Stage::Value
    }

    /// Feed a key event; returns `Some(Command::SetStorage(..))` once the user
    /// confirms a value.
    pub fn on_key(&mut self, key: KeyEvent) -> Option<Command> {
        if key.kind != KeyEventKind::Press {
            return None;
        }
        match self.stage {
            Stage::Target => self.on_key_target(key),
            Stage::Value => self.on_key_value(key),
        }
    }

    fn on_key_target(&mut self, key: KeyEvent) -> Option<Command> {
        // The picker emits `Command::Pin(item)` when the target is fully
        // specified; we intercept it as the target (we do NOT pin).
        if let Some(Command::Pin(item)) = self.picker.on_key(key) {
            match (self.resolve_key)(&item) {
                Ok((key_hex, type_id)) => {
                    self.key_hex = Some(key_hex);
                    self.value_type_id = Some(type_id);
                    self.target = Some(item);
                    self.stage = Stage::Value;
                    self.error = None;
                }
                Err(e) => self.error = Some(format!("resolve key: {e}")),
            }
        }
        None
    }

    fn on_key_value(&mut self, key: KeyEvent) -> Option<Command> {
        if self.mode == ValueMode::Tree {
            return self.on_key_tree(key);
        }
        match key.code {
            // `x` cycles raw-hex / scale-text while the buffer is empty. If a
            // tree value was seeded, `x` can also return to Tree mode.
            KeyCode::Char('x') if self.input.is_empty() => {
                self.cycle_mode();
                self.error = None;
                None
            }
            KeyCode::Char(c) => {
                self.input.push(c);
                None
            }
            KeyCode::Backspace => {
                self.input.pop();
                None
            }
            KeyCode::Enter => self.confirm_freetext(),
            _ => None,
        }
    }

    /// Key handling in typed-tree mode: `j`/`k` move the leaf cursor, `Enter`
    /// begins/commits an edit of the focused leaf, `x` drops to free-text.
    fn on_key_tree(&mut self, key: KeyEvent) -> Option<Command> {
        if self.editing_leaf {
            match key.code {
                KeyCode::Char(c) => self.leaf_input.push(c),
                KeyCode::Backspace => {
                    self.leaf_input.pop();
                }
                KeyCode::Esc => {
                    self.editing_leaf = false;
                    self.leaf_input.clear();
                }
                KeyCode::Enter => self.commit_leaf_edit(),
                _ => {}
            }
            return None;
        }
        match key.code {
            KeyCode::Char('x') => {
                self.mode = ValueMode::ScaleText;
                self.error = None;
                None
            }
            KeyCode::Char('j') | KeyCode::Down => {
                if !self.leaves.is_empty() {
                    self.leaf_cursor = (self.leaf_cursor + 1).min(self.leaves.len() - 1);
                }
                None
            }
            KeyCode::Char('k') | KeyCode::Up => {
                self.leaf_cursor = self.leaf_cursor.saturating_sub(1);
                None
            }
            // Begin editing the focused leaf.
            KeyCode::Char('i') => {
                if let Some(leaf) = self.leaves.get(self.leaf_cursor) {
                    self.leaf_input = leaf.text.clone();
                    self.editing_leaf = true;
                }
                None
            }
            // Confirm the whole edited tree → SetStorage.
            KeyCode::Enter => self.confirm_tree(),
            _ => None,
        }
    }

    /// Parse the leaf edit buffer and write it back into the working tree.
    fn commit_leaf_edit(&mut self) {
        let Some(leaf) = self.leaves.get(self.leaf_cursor).cloned() else {
            self.editing_leaf = false;
            return;
        };
        let (parsed, rest) = scale_value::stringify::from_str(&self.leaf_input);
        let new = match parsed {
            Ok(v) if rest.trim().is_empty() => v.map_context(|_| 0u32),
            Ok(_) => {
                self.error = Some(format!("trailing input: {rest:?}"));
                return;
            }
            Err(e) => {
                self.error = Some(format!("parse leaf: {e:?}"));
                return;
            }
        };
        if let Some(tree) = self.tree_value.as_mut() {
            if let Err(e) = set_leaf(tree, &leaf.path, new) {
                self.error = Some(e);
                return;
            }
            // Refresh leaves so the display reflects the edit.
            self.leaves = flatten_leaves(tree);
            self.error = None;
        }
        self.editing_leaf = false;
        self.leaf_input.clear();
    }

    /// Cycle free-text modes; if a tree value is seeded, include Tree in the cycle.
    fn cycle_mode(&mut self) {
        self.mode = match self.mode {
            ValueMode::ScaleText => ValueMode::RawHex,
            ValueMode::RawHex if self.tree_value.is_some() => ValueMode::Tree,
            ValueMode::RawHex => ValueMode::ScaleText,
            ValueMode::Tree => ValueMode::ScaleText,
        };
    }

    /// Encode the free-text input and emit `SetStorage`, or stash an error.
    fn confirm_freetext(&mut self) -> Option<Command> {
        let key_hex = self.key_hex.clone()?;
        let target = self.target.clone()?;
        let type_id = self.value_type_id.unwrap_or(0);
        match (self.encode_text)(self.mode, &self.input, type_id) {
            Ok(value_hex) => {
                let label = format!("{} = {}", target.label, self.input.trim());
                self.error = None;
                Some(Command::SetStorage(SetStorageReq {
                    key_hex,
                    value_hex: Some(value_hex),
                    label,
                }))
            }
            Err(e) => {
                self.error = Some(e);
                None
            }
        }
    }

    /// Re-encode the edited tree value and emit `SetStorage`, or stash an error.
    fn confirm_tree(&mut self) -> Option<Command> {
        let key_hex = self.key_hex.clone()?;
        let target = self.target.clone()?;
        let type_id = self.value_type_id.unwrap_or(0);
        let tree = self.tree_value.as_ref()?;
        match (self.encode_tree)(tree, type_id) {
            Ok(value_hex) => {
                let label = format!("{} (edited)", target.label);
                self.error = None;
                Some(Command::SetStorage(SetStorageReq {
                    key_hex,
                    value_hex: Some(value_hex),
                    label,
                }))
            }
            Err(e) => {
                self.error = Some(e);
                None
            }
        }
    }

    /// Draw the overlay (`Clear` then the active stage).
    pub fn render(&self, frame: &mut Frame, area: Rect) {
        frame.render_widget(Clear, area);
        match self.stage {
            Stage::Target => self.picker.render(frame, area),
            Stage::Value => self.render_value(frame, area),
        }
    }

    fn render_value(&self, frame: &mut Frame, area: Rect) {
        let rows = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(2), // target line
                Constraint::Min(3),    // editor
                Constraint::Length(2), // hint / error
            ])
            .split(area);

        let target = self
            .target
            .as_ref()
            .map(|t| t.label.as_str())
            .unwrap_or("(no target)");
        frame.render_widget(
            Paragraph::new(Line::from(format!("target  {target}  · writes to head"))),
            rows[0],
        );

        let mode_label = match self.mode {
            ValueMode::Tree => "typed field-tree",
            ValueMode::ScaleText => "scale-value text",
            ValueMode::RawHex => "raw SCALE hex",
        };

        let body = if self.mode == ValueMode::Tree {
            let mut lines = vec![Line::from(format!("[{mode_label}]"))];
            for (i, leaf) in self.leaves.iter().enumerate() {
                let focused = i == self.leaf_cursor;
                let value = if focused && self.editing_leaf {
                    format!("{} → {}", leaf.text, self.leaf_input)
                } else {
                    leaf.text.clone()
                };
                let marker = if focused { "›" } else { " " };
                let line = format!("{marker} {} = {}", leaf.display_path, value);
                let style = if focused {
                    Style::default().add_modifier(Modifier::BOLD)
                } else {
                    Style::default()
                };
                lines.push(Line::styled(line, style));
            }
            Text::from(lines)
        } else {
            Text::from(vec![
                Line::from(format!("[{mode_label}]")),
                Line::from(self.input.as_str()),
            ])
        };

        let editor = Paragraph::new(body)
            .wrap(Wrap { trim: false })
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .title("Value")
                    // purple = dev/command surface (spec §2.3).
                    .border_style(Style::default().fg(Color::Magenta)),
            );
        frame.render_widget(editor, rows[1]);

        let hint_text = match self.mode {
            ValueMode::Tree if self.editing_leaf => "type value · ↵ commit · esc cancel edit",
            ValueMode::Tree => "j/k move · i edit · ↵ write at tip · x free-text · esc cancel",
            _ => "x toggle mode · ↵ write at tip · esc cancel",
        };
        let hint = match &self.error {
            Some(e) => Span::styled(format!("error: {e}"), Style::default().fg(Color::Red)),
            None => Span::styled(hint_text, Style::default().add_modifier(Modifier::DIM)),
        };
        frame.render_widget(Paragraph::new(Line::from(hint)), rows[2]);
    }
}

/// Encode a raw `0x`-prefixed hex string into validated value bytes (the bytes
/// are passed through verbatim; this only validates hex-ness). Returns the
/// `0x`-prefixed normalized hex on success.
pub fn encode_raw_hex(input: &str) -> std::result::Result<String, String> {
    let body = input
        .trim()
        .strip_prefix("0x")
        .ok_or_else(|| "raw value must start with 0x".to_string())?;
    if body.is_empty() || !body.len().is_multiple_of(2) {
        return Err("hex must be non-empty and even-length".to_string());
    }
    for (i, _) in body.char_indices().step_by(2) {
        u8::from_str_radix(&body[i..i + 2], 16)
            .map_err(|_| format!("invalid hex at offset {i}"))?;
    }
    Ok(format!("0x{}", body.to_lowercase()))
}

/// Parse a scale-value text expression and SCALE-encode it against `type_id`
/// using the metadata `registry`. Returns the `0x`-prefixed encoded hex.
///
/// `R` is the metadata type resolver — at the call site this is
/// `metadata.types()` (a `scale_info::PortableRegistry`), the same registry
/// `storage_fetch` decodes against.
pub fn encode_scale_text<R>(
    input: &str,
    type_id: R::TypeId,
    registry: &R,
) -> std::result::Result<String, String>
where
    R: scale_value::scale::TypeResolver,
{
    let (parsed, rest) = scale_value::stringify::from_str(input);
    let value: Value<()> = parsed.map_err(|e| format!("parse error: {e:?}"))?;
    if !rest.trim().is_empty() {
        return Err(format!("trailing input not parsed: {rest:?}"));
    }
    let mut buf = Vec::new();
    encode_as_type(&value, type_id, registry, &mut buf)
        .map_err(|e| format!("encode error: {e}"))?;
    Ok(to_hex(&buf))
}

/// Re-encode an already-decoded `Value<u32>` against `type_id` (used by the
/// typed-tree mode). The value carries `u32` type-id context but
/// `encode_as_type` ignores it and encodes against the supplied `type_id`.
pub fn encode_value<R>(
    value: &Value<u32>,
    type_id: R::TypeId,
    registry: &R,
) -> std::result::Result<String, String>
where
    R: scale_value::scale::TypeResolver,
{
    let mut buf = Vec::new();
    encode_as_type(value, type_id, registry, &mut buf)
        .map_err(|e| format!("encode error: {e}"))?;
    Ok(to_hex(&buf))
}

// ---------------------------------------------------------------------------
// Typed field-tree core (Task 7): decode → flat editable leaves → re-encode.
// ---------------------------------------------------------------------------

use scale_value::{Composite, Primitive, ValueDef};

/// A path segment into a decoded value (local to the tree editor; mirrors
/// `contracts::PathSeg` but kept local to avoid a cross-module dep).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Seg {
    Field(String),
    Index(usize),
}

/// One editable leaf of the decoded value.
#[derive(Debug, Clone)]
pub struct Leaf {
    /// Path from the root to this leaf.
    pub path: Vec<Seg>,
    /// Dotted display path, e.g. `data.free`.
    pub display_path: String,
    /// Current rendered text of the leaf (what the user edits).
    pub text: String,
}

/// Flatten a decoded value into its primitive leaves, depth-first. Composites
/// recurse; primitives/variants/bit-sequences are leaves (a variant is edited
/// via free-text, per the fallback).
pub fn flatten_leaves(value: &Value<u32>) -> Vec<Leaf> {
    let mut out = Vec::new();
    walk(value, &mut Vec::new(), &mut out);
    out
}

fn walk(value: &Value<u32>, path: &mut Vec<Seg>, out: &mut Vec<Leaf>) {
    match &value.value {
        ValueDef::Composite(Composite::Named(fields)) => {
            for (name, v) in fields {
                path.push(Seg::Field(name.clone()));
                walk(v, path, out);
                path.pop();
            }
        }
        ValueDef::Composite(Composite::Unnamed(vals)) => {
            for (i, v) in vals.iter().enumerate() {
                path.push(Seg::Index(i));
                walk(v, path, out);
                path.pop();
            }
        }
        // Primitives, variants, bit-sequences are leaves.
        _ => out.push(Leaf {
            path: path.clone(),
            display_path: display_path(path),
            text: leaf_text(value),
        }),
    }
}

fn display_path(path: &[Seg]) -> String {
    if path.is_empty() {
        return "(value)".to_string();
    }
    path.iter()
        .map(|s| match s {
            Seg::Field(n) => n.clone(),
            Seg::Index(i) => i.to_string(),
        })
        .collect::<Vec<_>>()
        .join(".")
}

/// One-line text for a leaf value (numbers, bools, strings; everything else hex).
fn leaf_text(value: &Value<u32>) -> String {
    match &value.value {
        ValueDef::Primitive(Primitive::U128(n)) => n.to_string(),
        ValueDef::Primitive(Primitive::I128(n)) => n.to_string(),
        ValueDef::Primitive(Primitive::Bool(b)) => b.to_string(),
        ValueDef::Primitive(Primitive::String(s)) => s.clone(),
        ValueDef::Primitive(Primitive::Char(c)) => c.to_string(),
        other => format!("{other:?}"),
    }
}

/// Replace the value at `path` with `new`. Errors if the path does not resolve.
pub fn set_leaf(
    value: &mut Value<u32>,
    path: &[Seg],
    new: Value<u32>,
) -> std::result::Result<(), String> {
    let Some((head, rest)) = path.split_first() else {
        *value = new;
        return Ok(());
    };
    match (&mut value.value, head) {
        (ValueDef::Composite(Composite::Named(fields)), Seg::Field(name)) => {
            let slot = fields
                .iter_mut()
                .find(|(k, _)| k == name)
                .map(|(_, v)| v)
                .ok_or_else(|| format!("no field {name}"))?;
            set_leaf(slot, rest, new)
        }
        (ValueDef::Composite(Composite::Unnamed(vals)), Seg::Index(i)) => {
            let slot = vals.get_mut(*i).ok_or_else(|| format!("no index {i}"))?;
            set_leaf(slot, rest, new)
        }
        _ => Err("path does not resolve to a composite".to_string()),
    }
}

/// Lowercase `0x`-prefixed hex (mirrors `dev_rpc::to_hex` / `storage_fetch::hex_of`).
fn to_hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    let mut s = String::with_capacity(2 + bytes.len() * 2);
    s.push_str("0x");
    for b in bytes {
        let _ = write!(s, "{b:02x}");
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use scale_info::{PortableRegistry, TypeInfo};

    /// Build a single-type `PortableRegistry` for `T` and return it with the
    /// registered type-id, so encode/decode tests run fully offline.
    fn registry_for<T: TypeInfo + 'static>() -> (PortableRegistry, u32) {
        let mut registry = scale_info::Registry::new();
        let sym = registry.register_type(&scale_info::meta_type::<T>());
        let portable: PortableRegistry = registry.into();
        (portable, sym.id)
    }

    #[test]
    fn raw_hex_validates_and_normalizes() {
        assert_eq!(encode_raw_hex("0xDEAD").unwrap(), "0xdead");
        assert!(encode_raw_hex("dead").is_err(), "missing 0x");
        assert!(encode_raw_hex("0xabc").is_err(), "odd length");
        assert!(encode_raw_hex("0x").is_err(), "empty body");
        assert!(encode_raw_hex("0xzz").is_err(), "non-hex");
    }

    #[test]
    fn scale_text_encodes_u128_against_type() {
        let (registry, ty) = registry_for::<u128>();
        // 1000 as u128 little-endian = 0xe803...00 (16 bytes).
        let hex = encode_scale_text("1000", ty, &registry).expect("encode");
        let mut expected = vec![0xe8, 0x03];
        expected.resize(16, 0);
        assert_eq!(
            hex,
            format!(
                "0x{}",
                expected.iter().map(|b| format!("{b:02x}")).collect::<String>()
            )
        );
    }

    #[test]
    fn scale_text_round_trips_through_decode() {
        // Encode "1000" as u128, then decode it back and confirm re-encoding the
        // decoded value yields the same hex.
        let (registry, ty) = registry_for::<u128>();
        let hex = encode_scale_text("1000", ty, &registry).unwrap();
        let bytes: Vec<u8> = (2..hex.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&hex[i..i + 2], 16).unwrap())
            .collect();
        let mut cursor = bytes.as_slice();
        let decoded: Value<u32> =
            scale_value::scale::decode_as_type(&mut cursor, ty, &registry).unwrap();
        assert_eq!(encode_value(&decoded, ty, &registry).unwrap(), hex);
    }

    #[test]
    fn scale_text_rejects_garbage() {
        let (registry, ty) = registry_for::<u128>();
        assert!(encode_scale_text("not a value {", ty, &registry).is_err());
    }

    use crate::contracts::{
        Command, PalletInfo, StorageCatalog, StorageEntryInfo, StorageKind,
    };
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    struct OneEntryCatalog;
    impl StorageCatalog for OneEntryCatalog {
        fn pallets(&self) -> Vec<PalletInfo> {
            vec![PalletInfo { name: "Balances".into(), entry_count: 1 }]
        }
        fn entries(&self, pallet: &str) -> Vec<StorageEntryInfo> {
            if pallet == "Balances" {
                vec![StorageEntryInfo {
                    pallet: "Balances".into(),
                    name: "TotalIssuance".into(),
                    kind: StorageKind::Plain,
                    value_type_id: 0,
                    docs: String::new(),
                }]
            } else {
                vec![]
            }
        }
        fn entry(&self, pallet: &str, entry: &str) -> Option<StorageEntryInfo> {
            self.entries(pallet).into_iter().find(|e| e.name == entry)
        }
    }

    fn press(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }
    fn typ(ed: &mut SetStorageEditor, s: &str) {
        for c in s.chars() {
            ed.on_key(press(KeyCode::Char(c)));
        }
    }

    #[test]
    fn picks_target_then_emits_raw_hex_set_storage() {
        let cat = OneEntryCatalog;
        // `resolve_key` is supplied by the caller (app); in the test we stub it
        // to a fixed key + type-id so the editor needs no live node.
        let mut ed = SetStorageEditor::new(&cat, |_item| Ok(("0x1234".to_string(), 0u32)));

        // Target stage: filter to TotalIssuance, Enter to pick (plain → no keys).
        typ(&mut ed, "TotalIssuance");
        assert!(ed.on_key(press(KeyCode::Enter)).is_none(), "moves to value stage");
        assert!(ed.in_value_stage());

        // Toggle to raw-hex mode (`x`), type a value, Enter to confirm.
        ed.on_key(press(KeyCode::Char('x')));
        typ(&mut ed, "0x05");
        let cmd = ed.on_key(press(KeyCode::Enter)).expect("emits SetStorage");
        match cmd {
            Command::SetStorage(req) => {
                assert_eq!(req.key_hex, "0x1234");
                assert_eq!(req.value_hex.as_deref(), Some("0x05"));
                assert!(req.label.contains("Balances.TotalIssuance"));
            }
            other => panic!("expected SetStorage, got {other:?}"),
        }
    }

    #[test]
    fn flattens_named_struct_into_leaf_paths() {
        // AccountInfo-ish: { nonce: 3, data: { free: 100, reserved: 0 } }
        let v: Value<u32> = Value::named_composite([
            ("nonce", Value::u128(3)),
            (
                "data",
                Value::named_composite([
                    ("free", Value::u128(100)),
                    ("reserved", Value::u128(0)),
                ]),
            ),
        ])
        .map_context(|_| 0u32);
        let leaves = flatten_leaves(&v);
        let paths: Vec<String> = leaves.iter().map(|l| l.display_path.clone()).collect();
        assert_eq!(paths, vec!["nonce", "data.free", "data.reserved"]);
        assert_eq!(leaves[1].text, "100");
    }

    #[test]
    fn writes_leaf_back_by_path() {
        let mut v: Value<u32> = Value::named_composite([
            ("free", Value::u128(1)),
            ("reserved", Value::u128(2)),
        ])
        .map_context(|_| 0u32);
        set_leaf(
            &mut v,
            &[Seg::Field("free".into())],
            Value::u128(999).map_context(|_| 0u32),
        )
        .expect("write");
        let leaves = flatten_leaves(&v);
        assert_eq!(leaves[0].text, "999");
    }

    #[test]
    fn tree_edit_reencodes_changed_value() {
        let (registry, ty) = registry_for::<u128>();
        let original: Value<u32> = Value::u128(100).map_context(|_| 0u32);
        let before = encode_value(&original, ty, &registry).unwrap();
        let mut edited = original.clone();
        set_leaf(&mut edited, &[], Value::u128(999).map_context(|_| 0u32)).unwrap();
        let after = encode_value(&edited, ty, &registry).unwrap();
        assert_ne!(before, after, "edited value must re-encode differently");
    }
}
