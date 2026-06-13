//! Metadata-driven [`CallCatalog`](crate::views::tx_builder::CallCatalog): every
//! pallet and dispatchable call from the connected runtime's metadata, with each
//! argument's metadata type-id so the tx builder can encode/validate it against
//! the real type (via `scale_value::scale::encode_as_type`).

use scale_value::Value;
use scale_value::scale::encode_as_type;
use subxt::Metadata;

use crate::views::tx_builder::{ArgKind, ArgSpec, CallCatalog, EncodedCall};

/// A [`CallCatalog`] backed by the connected runtime's metadata.
pub struct MetadataCallCatalog<'a> {
    metadata: &'a Metadata,
}

impl<'a> MetadataCallCatalog<'a> {
    pub fn new(metadata: &'a Metadata) -> Self {
        Self { metadata }
    }
}

/// Infer the friendly [`ArgKind`] for a call argument from its runtime type.
/// Falls back to [`ArgKind::Scale`] (free-form scale-value text) for anything
/// not matching a known primitive/address shape.
fn infer_kind(type_id: u32, types: &scale_info::PortableRegistry) -> ArgKind {
    use scale_info::{TypeDef, TypeDefPrimitive};
    let Some(ty) = types.resolve(type_id) else {
        return ArgKind::Scale;
    };
    let last = ty
        .path
        .segments
        .last()
        .map(String::as_str)
        .unwrap_or_default();
    // Address-like types: a raw AccountId32 or a MultiAddress wrapper.
    if last == "AccountId32" || last == "MultiAddress" {
        return ArgKind::AccountId;
    }
    match &ty.type_def {
        TypeDef::Primitive(TypeDefPrimitive::Bool) => ArgKind::Bool,
        TypeDef::Primitive(
            TypeDefPrimitive::U8
            | TypeDefPrimitive::U16
            | TypeDefPrimitive::U32
            | TypeDefPrimitive::U64
            | TypeDefPrimitive::U128
            | TypeDefPrimitive::U256,
        ) => ArgKind::U128,
        TypeDef::Primitive(TypeDefPrimitive::Str) => ArgKind::Text,
        // Compact<uN> (balances, indices) → integer input.
        TypeDef::Compact(_) => ArgKind::U128,
        _ => ArgKind::Scale,
    }
}

impl CallCatalog for MetadataCallCatalog<'_> {
    fn pallets(&self) -> Vec<String> {
        let mut names: Vec<String> = self
            .metadata
            .pallets()
            .filter(|p| p.call_variants().is_some())
            .map(|p| p.name().to_string())
            .collect();
        names.sort();
        names
    }

    fn calls(&self, pallet: &str) -> Vec<String> {
        let Some(p) = self.metadata.pallet_by_name(pallet) else {
            return Vec::new();
        };
        p.call_variants()
            .map(|vs| vs.iter().map(|v| v.name.clone()).collect())
            .unwrap_or_default()
    }

    fn args(&self, pallet: &str, call: &str) -> Vec<ArgSpec> {
        let Some(p) = self.metadata.pallet_by_name(pallet) else {
            return Vec::new();
        };
        let Some(variant) = p.call_variant_by_name(call) else {
            return Vec::new();
        };
        let types = self.metadata.types();
        variant
            .fields
            .iter()
            .enumerate()
            .map(|(i, field)| {
                let type_id = field.ty.id;
                let name = field
                    .name
                    .clone()
                    .unwrap_or_else(|| format!("arg{i}"));
                ArgSpec::typed(name, infer_kind(type_id, types), type_id)
            })
            .collect()
    }

    fn encode_call(
        &self,
        pallet: &str,
        call: &str,
        args: &[Value<()>],
    ) -> std::result::Result<Option<EncodedCall>, String> {
        let Some(p) = self.metadata.pallet_by_name(pallet) else {
            return Err(format!("unknown pallet `{pallet}`"));
        };
        let Some(variant) = p.call_variant_by_name(call) else {
            return Err(format!("unknown call `{pallet}.{call}`"));
        };
        let types = self.metadata.types();
        let mut coerced = Vec::with_capacity(args.len());
        let mut bytes = Vec::new();
        for (field, value) in variant.fields.iter().zip(args) {
            let type_id = field.ty.id;
            let name = field.name.clone().unwrap_or_default();
            let (value, mut arg_bytes) = encode_arg(value, type_id, types)
                .map_err(|e| format!("{name}: {e}"))?;
            coerced.push(value);
            bytes.append(&mut arg_bytes);
        }
        Ok(Some(EncodedCall { args: coerced, bytes }))
    }
}

/// Encode one arg against its runtime type, returning the submission-ready value
/// and its SCALE bytes. Applies the one Substrate-specific coercion the friendly
/// inputs need: a bare AccountId byte value won't encode into a `MultiAddress`,
/// so if the direct encode fails we retry once wrapped in the `Id` variant.
fn encode_arg(
    value: &Value<()>,
    type_id: u32,
    types: &scale_info::PortableRegistry,
) -> std::result::Result<(Value<()>, Vec<u8>), String> {
    match encode_value(value, type_id, types) {
        Ok(bytes) => Ok((value.clone(), bytes)),
        Err(first) => {
            let wrapped = Value::unnamed_variant("Id", [value.clone()]);
            match encode_value(&wrapped, type_id, types) {
                Ok(bytes) => Ok((wrapped, bytes)),
                // Report the original error — clearer than the wrapped retry's.
                Err(_) => Err(first),
            }
        }
    }
}

/// SCALE-encode a parsed scale-value against a runtime metadata `type_id`,
/// returning the encoded bytes. The error string is the human-readable encode
/// failure (e.g. type mismatch), suitable for inline display.
///
/// `R` is the metadata type resolver — at the call site this is
/// `metadata.types()` (a `scale_info::PortableRegistry`), mirroring how
/// `set_storage` encodes values.
pub fn encode_value<R>(
    value: &Value<()>,
    type_id: R::TypeId,
    registry: &R,
) -> std::result::Result<Vec<u8>, String>
where
    R: scale_value::scale::TypeResolver,
{
    let mut buf = Vec::new();
    encode_as_type(value, type_id, registry, &mut buf).map_err(|e| format!("{e}"))?;
    Ok(buf)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fixtures::polkadot_metadata;
    use subxt::utils::AccountId32;

    const ALICE_SS58: &str = "5GrwvaEF5zXb26Fz9rcQpDWS57CtERHpNehXCPcNoHGKutQY";

    /// The ordered field type-ids of a `pallet.call`, via subxt metadata.
    fn field_type_ids(md: &Metadata, pallet: &str, call: &str) -> Vec<u32> {
        md.pallet_by_name(pallet)
            .expect("pallet present")
            .call_variant_by_name(call)
            .expect("call present")
            .fields
            .iter()
            .map(|f| f.ty.id)
            .collect()
    }

    #[test]
    fn lists_many_pallets_including_balances_and_system() {
        let md = polkadot_metadata();
        let cat = MetadataCallCatalog::new(&md);
        let pallets = cat.pallets();
        assert!(
            pallets.len() > 2,
            "metadata exposes many pallets, got {}",
            pallets.len()
        );
        assert!(pallets.contains(&"Balances".to_string()));
        assert!(pallets.contains(&"System".to_string()));
        let mut sorted = pallets.clone();
        sorted.sort();
        assert_eq!(pallets, sorted, "pallets are alphabetical");
    }

    #[test]
    fn balances_calls_include_transfer_keep_alive() {
        let md = polkadot_metadata();
        let cat = MetadataCallCatalog::new(&md);
        assert!(
            cat.calls("Balances").contains(&"transfer_keep_alive".to_string()),
            "Balances calls: {:?}",
            cat.calls("Balances")
        );
    }

    #[test]
    fn args_carry_names_type_ids_and_inferred_kinds() {
        let md = polkadot_metadata();
        let cat = MetadataCallCatalog::new(&md);
        let args = cat.args("Balances", "transfer_keep_alive");
        assert_eq!(args.len(), 2, "transfer_keep_alive has (dest, value)");
        assert_eq!(args[0].name, "dest");
        assert_eq!(args[1].name, "value");
        assert!(args[0].type_id.is_some());
        assert!(args[1].type_id.is_some());
        // dest is MultiAddress → AccountId friendly; value is u128.
        assert_eq!(args[0].kind, ArgKind::AccountId);
        assert_eq!(args[1].kind, ArgKind::U128);
    }

    #[test]
    fn unknown_pallet_or_call_yields_empty() {
        let md = polkadot_metadata();
        let cat = MetadataCallCatalog::new(&md);
        assert!(cat.calls("NoSuchPallet").is_empty());
        assert!(cat.args("Balances", "no_such_call").is_empty());
    }

    #[test]
    fn encodes_u128_value_arg_against_its_type() {
        let md = polkadot_metadata();
        let ids = field_type_ids(&md, "Balances", "transfer_keep_alive"); // [dest, value]
        let bytes = encode_value(&Value::u128(1_000_000), ids[1], md.types())
            .expect("u128 encodes against the value type");
        assert!(!bytes.is_empty(), "encoded value must be non-empty");
    }

    #[test]
    fn encode_call_coerces_accountid_and_encodes_value() {
        let md = polkadot_metadata();
        let cat = MetadataCallCatalog::new(&md);
        let alice: AccountId32 = ALICE_SS58.parse().unwrap();
        // The friendly AccountId arg arrives as bare bytes (ArgKind::AccountId).
        let raw = vec![Value::from_bytes(alice.0), Value::u128(1_000_000)];
        let enc = cat
            .encode_call("Balances", "transfer_keep_alive", &raw)
            .expect("encodes")
            .expect("metadata catalog yields Some");
        assert!(!enc.bytes.is_empty(), "real encoded bytes");
        // `dest` was coerced into MultiAddress::Id(...) so it (and submission) encodes.
        assert_eq!(
            enc.args[0],
            Value::unnamed_variant("Id", [Value::from_bytes(alice.0)])
        );
        assert_eq!(enc.args[1], Value::u128(1_000_000), "value passes through");
    }

    #[test]
    fn encode_call_errors_on_unencodable_value() {
        let md = polkadot_metadata();
        let cat = MetadataCallCatalog::new(&md);
        let raw = vec![
            Value::unnamed_variant("Id", [Value::from_bytes([0u8; 32])]),
            Value::string("not-a-number"), // value is u128 → must fail
        ];
        let err = cat.encode_call("Balances", "transfer_keep_alive", &raw);
        assert!(err.is_err(), "string into u128 must fail: {err:?}");
    }

    #[test]
    fn accountid_encodes_into_multiaddress_when_wrapped_as_id() {
        // Discovery-confirmed: `dest` is a MultiAddress, so a bare AccountId byte
        // value fails; it must be wrapped as `Id(bytes)` to encode.
        let md = polkadot_metadata();
        let ids = field_type_ids(&md, "Balances", "transfer_keep_alive");
        let alice: AccountId32 = ALICE_SS58.parse().unwrap();
        let raw = encode_value(&Value::from_bytes(alice.0), ids[0], md.types());
        let wrapped = encode_value(
            &Value::unnamed_variant("Id", [Value::from_bytes(alice.0)]),
            ids[0],
            md.types(),
        );
        assert!(raw.is_err(), "bare account bytes must NOT encode into MultiAddress");
        assert!(wrapped.is_ok(), "Id-wrapped account must encode: {wrapped:?}");
    }
}
