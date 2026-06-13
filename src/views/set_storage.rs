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
    /// scale-value text expression (default for free-text).
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

    resolve_key: Box<KeyResolver<'a>>,
    encode_text: Box<TextEncoder<'a>>,
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
            resolve_key: Box::new(resolve_key),
            encode_text: Box::new(|mode, raw, _ty| match mode {
                ValueMode::RawHex => encode_raw_hex(raw),
                ValueMode::ScaleText => {
                    Err("scale-value text needs metadata (press `x` for raw hex)".to_string())
                }
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
        match key.code {
            // `x` toggles raw-hex / scale-text while the buffer is empty.
            KeyCode::Char('x') if self.input.is_empty() => {
                self.mode = match self.mode {
                    ValueMode::ScaleText => ValueMode::RawHex,
                    ValueMode::RawHex => ValueMode::ScaleText,
                };
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
            KeyCode::Enter => self.confirm(),
            _ => None,
        }
    }

    /// Encode the current input and emit `SetStorage`, or stash an error.
    fn confirm(&mut self) -> Option<Command> {
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
            ValueMode::ScaleText => "scale-value text",
            ValueMode::RawHex => "raw SCALE hex",
        };
        let editor = Paragraph::new(Text::from(vec![
            Line::from(format!("[{mode_label}]")),
            Line::from(self.input.as_str()),
        ]))
        .wrap(Wrap { trim: false })
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title("Value")
                // purple = dev/command surface (spec §2.3).
                .border_style(Style::default().fg(Color::Magenta)),
        );
        frame.render_widget(editor, rows[1]);

        let hint = match &self.error {
            Some(e) => Span::styled(format!("error: {e}"), Style::default().fg(Color::Red)),
            None => Span::styled(
                "x toggle hex/text · ↵ write at tip · esc cancel",
                Style::default().add_modifier(Modifier::DIM),
            ),
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
}
