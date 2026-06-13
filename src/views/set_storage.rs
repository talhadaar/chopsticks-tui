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

use scale_value::Value;
use scale_value::scale::encode_as_type;

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
}
