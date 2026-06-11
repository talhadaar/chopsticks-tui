//! Test fixtures: a captured Polkadot metadata blob plus sample decoded values,
//! shared by the storage and render unit tests (ticket T01; consumed by
//! T05–T08). Compiled only under `cfg(test)`, so the blob never ships in a
//! release binary.

use scale_value::Value;
use subxt::Metadata;

/// Raw SCALE-encoded `RuntimeMetadataPrefixed` (metadata v14) captured from
/// Polkadot mainnet via `state_getMetadata`.
const POLKADOT_METADATA: &[u8] = include_bytes!("polkadot_metadata.scale");

/// Decode the captured Polkadot runtime metadata into `subxt::Metadata`.
pub fn polkadot_metadata() -> Metadata {
    Metadata::decode_from(POLKADOT_METADATA).expect("captured Polkadot metadata decodes")
}

/// A few decoded sample values (`Value<u32>`) exercising primitive render cases.
/// Storage/render tickets build richer trees against real decoded data.
pub fn sample_values() -> Vec<Value<u32>> {
    let with_dummy_type_id = |_: ()| 0u32;
    vec![
        Value::u128(1_000_000_000_000).map_context(with_dummy_type_id),
        Value::string("hello").map_context(with_dummy_type_id),
        Value::bool(true).map_context(with_dummy_type_id),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn metadata_blob_decodes_with_core_pallets() {
        let metadata = polkadot_metadata();
        let system = metadata
            .pallet_by_name("System")
            .expect("System pallet present");
        assert!(system.storage().is_some(), "System exposes storage");
        assert!(
            metadata.pallet_by_name("Balances").is_some(),
            "Balances pallet present"
        );
    }

    #[test]
    fn sample_values_carry_u32_context() {
        let values = sample_values();
        assert_eq!(values.len(), 3);
        assert!(values.iter().all(|v| v.context == 0u32));
    }
}
