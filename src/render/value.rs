//! Value rendering: `scale_value::Value` -> compact display string (ticket T07).
//!
//! Walks an optional nested [`PathSeg`] path into a decoded value, then formats
//! the focused node as a single-line string fit for one grid cell. Formatting is
//! type-aware: 32-byte arrays render as SS58 addresses, large unsigned integers
//! can be rendered as token balances, enum variants as `Name(..)`, and composites
//! as compact `{ field: val, .. }` / `(a, b)` forms.

use scale_value::{At, Composite, Primitive, Value, ValueDef};

use crate::contracts::{PathSeg, RenderCtx, ValueRenderer};

/// Sentinel rendered when a requested path does not resolve in the value.
const NO_PATH: &str = "⟨no path⟩";

/// Type-aware value renderer (SS58, balances, enums, structs, bytes).
pub struct DefaultRenderer;

impl ValueRenderer for DefaultRenderer {
    fn render(&self, value: &Value<u32>, path: &[PathSeg], ctx: &RenderCtx) -> String {
        match navigate(value, path) {
            Some(focused) => render_value(focused, ctx),
            None => NO_PATH.to_string(),
        }
    }
}

/// Walk `path` into `value` using scale_value's [`At`] trait. Returns `None` if
/// any segment fails to resolve (never panics).
fn navigate<'a>(value: &'a Value<u32>, path: &[PathSeg]) -> Option<&'a Value<u32>> {
    let mut current = value;
    for seg in path {
        current = match seg {
            PathSeg::Field(name) => current.at(name.as_str())?,
            PathSeg::Index(i) => current.at(*i as usize)?,
        };
    }
    Some(current)
}

/// Format a focused value as a compact single-line string.
fn render_value(value: &Value<u32>, ctx: &RenderCtx) -> String {
    match &value.value {
        ValueDef::Primitive(p) => render_primitive(p),
        ValueDef::Composite(c) => render_composite(c, ctx),
        ValueDef::Variant(v) => {
            let inner = render_composite_fields(&v.values, ctx);
            if inner.is_empty() {
                v.name.clone()
            } else {
                format!("{}({inner})", v.name)
            }
        }
        ValueDef::BitSequence(bits) => {
            let s: String = bits.iter().map(|b| if b { '1' } else { '0' }).collect();
            format!("0b{s}")
        }
    }
}

fn render_primitive(p: &Primitive) -> String {
    match p {
        Primitive::Bool(b) => b.to_string(),
        Primitive::Char(c) => c.to_string(),
        Primitive::String(s) => s.clone(),
        Primitive::U128(n) => n.to_string(),
        Primitive::I128(n) => n.to_string(),
        Primitive::U256(bytes) | Primitive::I256(bytes) => format!("0x{}", hex(bytes)),
    }
}

/// Render a composite. A 32-byte unnamed composite of u8 primitives is treated as
/// an AccountId and rendered as an SS58 address; other byte arrays render as hex.
fn render_composite(c: &Composite<u32>, ctx: &RenderCtx) -> String {
    if let Some(bytes) = as_byte_array(c) {
        if bytes.len() == 32 {
            let arr: [u8; 32] = bytes.as_slice().try_into().expect("len checked");
            return ss58_encode(&arr, ctx.ss58_prefix);
        }
        return format!("0x{}", hex(&bytes));
    }
    match c {
        Composite::Named(_) => format!("{{ {} }}", render_composite_fields(c, ctx)),
        Composite::Unnamed(_) => format!("({})", render_composite_fields(c, ctx)),
    }
}

/// Render the inner fields of a composite as a comma-separated list, without the
/// surrounding brackets. Named -> `k: v`, unnamed -> `v`.
fn render_composite_fields(c: &Composite<u32>, ctx: &RenderCtx) -> String {
    match c {
        Composite::Named(fields) => fields
            .iter()
            .map(|(k, v)| format!("{k}: {}", render_value(v, ctx)))
            .collect::<Vec<_>>()
            .join(", "),
        Composite::Unnamed(vals) => vals
            .iter()
            .map(|v| render_value(v, ctx))
            .collect::<Vec<_>>()
            .join(", "),
    }
}

/// If a composite is an unnamed sequence of `u8` primitives, return the bytes.
fn as_byte_array(c: &Composite<u32>) -> Option<Vec<u8>> {
    let Composite::Unnamed(vals) = c else {
        return None;
    };
    if vals.is_empty() {
        return None;
    }
    let mut out = Vec::with_capacity(vals.len());
    for v in vals {
        match &v.value {
            ValueDef::Primitive(Primitive::U128(n)) if *n <= u8::MAX as u128 => out.push(*n as u8),
            _ => return None,
        }
    }
    Some(out)
}

/// Format a raw unsigned integer as a token balance: integer part with a `.`
/// fractional part (trailing zeros trimmed) and the token symbol appended.
///
/// e.g. `1_500_000_000_000` with 12 decimals + `DOT` -> `1.5 DOT`.
pub fn format_balance(raw: u128, decimals: u8, symbol: &str) -> String {
    if decimals == 0 {
        return format!("{raw} {symbol}");
    }
    let scale = 10u128.pow(decimals as u32);
    let whole = raw / scale;
    let frac = raw % scale;
    if frac == 0 {
        return format!("{whole} {symbol}");
    }
    // Zero-pad the fractional part to `decimals` digits, then trim trailing zeros.
    let frac_str = format!("{frac:0width$}", width = decimals as usize);
    let frac_trimmed = frac_str.trim_end_matches('0');
    format!("{whole}.{frac_trimmed} {symbol}")
}

fn hex(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push_str(&format!("{b:02x}"));
    }
    s
}

// ---------------------------------------------------------------------------
// SS58 address encoding (configurable prefix).
//
// subxt's `AccountId32` only encodes with the hardcoded substrate prefix (42),
// so we implement the SS58 scheme directly to honour `ctx.ss58_prefix`:
//   data = prefix-bytes ++ account ++ blake2b512("SS58PRE" ++ prefix ++ account)[..2]
//   address = base58(data)
// ---------------------------------------------------------------------------

/// Encode a 32-byte public key as an SS58 address with the given network prefix.
fn ss58_encode(account: &[u8; 32], prefix: u16) -> String {
    let mut body = Vec::with_capacity(35);
    // Prefix encoding: <= 63 takes one byte; 64..=16383 takes two bytes (the
    // "extended" 2-byte form used by larger network ids).
    if prefix <= 63 {
        body.push(prefix as u8);
    } else if prefix <= 16_383 {
        let low = (prefix & 0b0011_1111) as u8;
        let high = (prefix >> 6) as u8;
        let first = 0b0100_0000 | low;
        body.push(first);
        body.push(high);
    } else {
        // Out of range for SS58; fall back to the default substrate prefix.
        body.push(42);
    }
    body.extend_from_slice(account);

    let hash = ss58_checksum_hash(&body);
    body.extend_from_slice(&hash[0..2]);
    base58_encode(&body)
}

/// `blake2b-512("SS58PRE" ++ data)`.
fn ss58_checksum_hash(data: &[u8]) -> [u8; 64] {
    let mut input = Vec::with_capacity(7 + data.len());
    input.extend_from_slice(b"SS58PRE");
    input.extend_from_slice(data);
    blake2b_512(&input)
}

// --- base58 (Bitcoin alphabet) ---------------------------------------------

const BASE58_ALPHABET: &[u8; 58] =
    b"123456789ABCDEFGHJKLMNPQRSTUVWXYZabcdefghijkmnopqrstuvwxyz";

fn base58_encode(input: &[u8]) -> String {
    // Count leading zero bytes -> leading '1's.
    let zeros = input.iter().take_while(|&&b| b == 0).count();

    // Convert base-256 to base-58 via repeated division (big-endian digit buffer).
    let mut digits: Vec<u8> = Vec::with_capacity(input.len() * 138 / 100 + 1);
    for &byte in input {
        let mut carry = byte as u32;
        for digit in digits.iter_mut() {
            carry += (*digit as u32) << 8;
            *digit = (carry % 58) as u8;
            carry /= 58;
        }
        while carry > 0 {
            digits.push((carry % 58) as u8);
            carry /= 58;
        }
    }

    let mut out = String::with_capacity(zeros + digits.len());
    for _ in 0..zeros {
        out.push(BASE58_ALPHABET[0] as char);
    }
    for &d in digits.iter().rev() {
        out.push(BASE58_ALPHABET[d as usize] as char);
    }
    out
}

// --- blake2b-512 ------------------------------------------------------------
// Reference implementation of BLAKE2b (RFC 7693) producing a 64-byte digest.
// Self-contained so the renderer needs no extra hashing dependency.

const BLAKE2B_IV: [u64; 8] = [
    0x6a09e667f3bcc908,
    0xbb67ae8584caa73b,
    0x3c6ef372fe94f82b,
    0xa54ff53a5f1d36f1,
    0x510e527fade682d1,
    0x9b05688c2b3e6c1f,
    0x1f83d9abfb41bd6b,
    0x5be0cd19137e2179,
];

const BLAKE2B_SIGMA: [[usize; 16]; 12] = [
    [0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15],
    [14, 10, 4, 8, 9, 15, 13, 6, 1, 12, 0, 2, 11, 7, 5, 3],
    [11, 8, 12, 0, 5, 2, 15, 13, 10, 14, 3, 6, 7, 1, 9, 4],
    [7, 9, 3, 1, 13, 12, 11, 14, 2, 6, 5, 10, 4, 0, 15, 8],
    [9, 0, 5, 7, 2, 4, 10, 15, 14, 1, 11, 12, 6, 8, 3, 13],
    [2, 12, 6, 10, 0, 11, 8, 3, 4, 13, 7, 5, 15, 14, 1, 9],
    [12, 5, 1, 15, 14, 13, 4, 10, 0, 7, 6, 3, 9, 2, 8, 11],
    [13, 11, 7, 14, 12, 1, 3, 9, 5, 0, 15, 4, 8, 6, 2, 10],
    [6, 15, 14, 9, 11, 3, 0, 8, 12, 2, 13, 7, 1, 4, 10, 5],
    [10, 2, 8, 4, 7, 6, 1, 5, 15, 11, 9, 14, 3, 12, 13, 0],
    [0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15],
    [14, 10, 4, 8, 9, 15, 13, 6, 1, 12, 0, 2, 11, 7, 5, 3],
];

fn blake2b_512(input: &[u8]) -> [u8; 64] {
    let out_len: u64 = 64;
    let mut h = BLAKE2B_IV;
    // Parameter block: digest length, key length (0), fanout (1), depth (1).
    h[0] ^= 0x0101_0000 ^ out_len;

    let mut t: u128 = 0; // total bytes compressed so far
    let mut chunks = input.chunks_exact(128);
    let mut last_block = [0u8; 128];

    // Process all but the final block (the final block is always handled with
    // the `final` flag set, even when the input is empty or exactly 128 bytes).
    let full_blocks: Vec<&[u8]> = chunks.by_ref().collect();
    let remainder = chunks.remainder();

    let last_len = if remainder.is_empty() && !full_blocks.is_empty() {
        // Last full block becomes the final block.
        let (final_block, leading) = full_blocks.split_last().unwrap();
        for block in leading {
            t += 128;
            blake2b_compress(&mut h, block, t, false);
        }
        last_block[..128].copy_from_slice(final_block);
        128
    } else {
        for block in &full_blocks {
            t += 128;
            blake2b_compress(&mut h, block, t, false);
        }
        last_block[..remainder.len()].copy_from_slice(remainder);
        remainder.len()
    };

    t += last_len as u128;
    blake2b_compress(&mut h, &last_block, t, true);

    let mut out = [0u8; 64];
    for (i, word) in h.iter().enumerate() {
        out[i * 8..i * 8 + 8].copy_from_slice(&word.to_le_bytes());
    }
    out
}

fn blake2b_compress(h: &mut [u64; 8], block: &[u8], t: u128, last: bool) {
    let mut m = [0u64; 16];
    for (i, word) in m.iter_mut().enumerate() {
        let mut bytes = [0u8; 8];
        bytes.copy_from_slice(&block[i * 8..i * 8 + 8]);
        *word = u64::from_le_bytes(bytes);
    }

    let mut v = [0u64; 16];
    v[..8].copy_from_slice(h);
    v[8..].copy_from_slice(&BLAKE2B_IV);

    v[12] ^= (t & 0xffff_ffff_ffff_ffff) as u64;
    v[13] ^= (t >> 64) as u64;
    if last {
        v[14] = !v[14];
    }

    for sigma in BLAKE2B_SIGMA.iter() {
        blake2b_mix(&mut v, 0, 4, 8, 12, m[sigma[0]], m[sigma[1]]);
        blake2b_mix(&mut v, 1, 5, 9, 13, m[sigma[2]], m[sigma[3]]);
        blake2b_mix(&mut v, 2, 6, 10, 14, m[sigma[4]], m[sigma[5]]);
        blake2b_mix(&mut v, 3, 7, 11, 15, m[sigma[6]], m[sigma[7]]);
        blake2b_mix(&mut v, 0, 5, 10, 15, m[sigma[8]], m[sigma[9]]);
        blake2b_mix(&mut v, 1, 6, 11, 12, m[sigma[10]], m[sigma[11]]);
        blake2b_mix(&mut v, 2, 7, 8, 13, m[sigma[12]], m[sigma[13]]);
        blake2b_mix(&mut v, 3, 4, 9, 14, m[sigma[14]], m[sigma[15]]);
    }

    for i in 0..8 {
        h[i] ^= v[i] ^ v[i + 8];
    }
}

#[allow(clippy::too_many_arguments)]
fn blake2b_mix(v: &mut [u64; 16], a: usize, b: usize, c: usize, d: usize, x: u64, y: u64) {
    v[a] = v[a].wrapping_add(v[b]).wrapping_add(x);
    v[d] = (v[d] ^ v[a]).rotate_right(32);
    v[c] = v[c].wrapping_add(v[d]);
    v[b] = (v[b] ^ v[c]).rotate_right(24);
    v[a] = v[a].wrapping_add(v[b]).wrapping_add(y);
    v[d] = (v[d] ^ v[a]).rotate_right(16);
    v[c] = v[c].wrapping_add(v[d]);
    v[b] = (v[b] ^ v[c]).rotate_right(63);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fixtures::sample_values;
    use scale_value::Value;

    fn ctx() -> RenderCtx {
        RenderCtx {
            ss58_prefix: 0,
            token_decimals: 12,
            token_symbol: "DOT".to_string(),
        }
    }

    /// Map a `Value<()>` literal into the `Value<u32>` the renderer expects.
    fn v32(v: Value<()>) -> Value<u32> {
        v.map_context(|_| 0u32)
    }

    #[test]
    fn renders_primitive_u128() {
        let values = sample_values();
        let out = DefaultRenderer.render(&values[0], &[], &ctx());
        assert_eq!(out, "1000000000000");
    }

    #[test]
    fn renders_string() {
        let values = sample_values();
        let out = DefaultRenderer.render(&values[1], &[], &ctx());
        assert_eq!(out, "hello");
    }

    #[test]
    fn renders_bool() {
        let values = sample_values();
        let out = DefaultRenderer.render(&values[2], &[], &ctx());
        assert_eq!(out, "true");
    }

    #[test]
    fn navigates_named_field_path() {
        // AccountData { data: { free: 42, reserved: 7 } }
        let value = v32(Value::named_composite([(
            "data",
            Value::named_composite([
                ("free", Value::u128(42)),
                ("reserved", Value::u128(7)),
            ]),
        )]));
        let path = [PathSeg::Field("data".into()), PathSeg::Field("free".into())];
        let out = DefaultRenderer.render(&value, &path, &ctx());
        assert_eq!(out, "42");
    }

    #[test]
    fn navigates_tuple_index_path() {
        let value = v32(Value::unnamed_composite([
            Value::u128(10),
            Value::string("second"),
            Value::bool(false),
        ]));
        let path = [PathSeg::Index(1)];
        let out = DefaultRenderer.render(&value, &path, &ctx());
        assert_eq!(out, "second");
    }

    #[test]
    fn unresolvable_path_renders_sentinel_not_panic() {
        let value = v32(Value::named_composite([("free", Value::u128(1))]));
        let path = [PathSeg::Field("nonexistent".into())];
        let out = DefaultRenderer.render(&value, &path, &ctx());
        assert_eq!(out, NO_PATH);

        // Indexing into a primitive must also be safe.
        let prim = v32(Value::u128(5));
        let out2 = DefaultRenderer.render(&prim, &[PathSeg::Index(0)], &ctx());
        assert_eq!(out2, NO_PATH);
    }

    #[test]
    fn accountid_bytes_render_as_ss58_with_prefix() {
        // Alice's well-known sr25519 public key.
        let alice: [u8; 32] = [
            0xd4, 0x35, 0x93, 0xc7, 0x15, 0xfd, 0xd3, 0x1c, 0x61, 0x14, 0x1a, 0xbd, 0x04, 0xa9,
            0x9f, 0xd6, 0x82, 0x2c, 0x85, 0x58, 0x85, 0x4c, 0xcd, 0xe3, 0x9a, 0x56, 0x84, 0xe7,
            0xa5, 0x6d, 0xa2, 0x7d,
        ];
        let value = v32(Value::unnamed_composite(
            alice.iter().map(|b| Value::u128(*b as u128)),
        ));

        // Substrate generic prefix (42).
        let substrate_ctx = RenderCtx {
            ss58_prefix: 42,
            token_decimals: 12,
            token_symbol: "UNIT".into(),
        };
        let out = DefaultRenderer.render(&value, &[], &substrate_ctx);
        assert_eq!(out, "5GrwvaEF5zXb26Fz9rcQpDWS57CtERHpNehXCPcNoHGKutQY");

        // Polkadot prefix (0) yields a different, '1'-leading address.
        let polkadot_ctx = RenderCtx {
            ss58_prefix: 0,
            token_decimals: 10,
            token_symbol: "DOT".into(),
        };
        let out_dot = DefaultRenderer.render(&value, &[], &polkadot_ctx);
        assert_eq!(out_dot, "15oF4uVJwmo4TdGW7VfQxNLavjCXviqxT9S1MgbjMNHr6Sp5");
    }

    #[test]
    fn balance_helper_applies_decimals_and_symbol() {
        assert_eq!(format_balance(1_500_000_000_000, 12, "DOT"), "1.5 DOT");
        assert_eq!(format_balance(1_000_000_000_000, 12, "DOT"), "1 DOT");
        assert_eq!(format_balance(0, 12, "DOT"), "0 DOT");
        assert_eq!(format_balance(1, 12, "DOT"), "0.000000000001 DOT");
        assert_eq!(format_balance(42, 0, "PLANCK"), "42 PLANCK");
    }

    #[test]
    fn renders_variant_with_fields() {
        let value = v32(Value::named_variant(
            "Transfer",
            [("amount", Value::u128(100))],
        ));
        assert_eq!(
            DefaultRenderer.render(&value, &[], &ctx()),
            "Transfer(amount: 100)"
        );

        let unit = v32(Value::variant("None", Composite::Unnamed(vec![])));
        assert_eq!(DefaultRenderer.render(&unit, &[], &ctx()), "None");
    }

    #[test]
    fn renders_named_struct_compact() {
        let value = v32(Value::named_composite([
            ("free", Value::u128(1)),
            ("reserved", Value::u128(2)),
        ]));
        assert_eq!(
            DefaultRenderer.render(&value, &[], &ctx()),
            "{ free: 1, reserved: 2 }"
        );
    }
}
