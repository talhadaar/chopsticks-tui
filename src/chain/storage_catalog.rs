//! Browsable view over runtime metadata's storage (ticket T05).

use subxt::Metadata;
use subxt::metadata::StorageEntryMetadata;

use crate::contracts::{PalletInfo, StorageCatalog, StorageEntryInfo, StorageKind};

/// Reads pallets/entries from runtime metadata.
pub struct MetadataCatalog<'a> {
    pub metadata: &'a Metadata,
}

/// Build a [`StorageEntryInfo`] from a subxt storage entry's metadata.
///
/// A plain entry has no keys; map/double-map/NMap entries carry one key per
/// dimension. subxt handles hashers at fetch time, so only the ordered key
/// type-ids are surfaced here.
fn entry_info(pallet: &str, entry: &StorageEntryMetadata) -> StorageEntryInfo {
    let key_type_ids: Vec<u32> = entry.keys().map(|k| k.key_id).collect();
    let kind = if key_type_ids.is_empty() {
        StorageKind::Plain
    } else {
        StorageKind::Map { key_type_ids }
    };
    StorageEntryInfo {
        pallet: pallet.to_string(),
        name: entry.name().to_string(),
        kind,
        value_type_id: entry.value_ty(),
        docs: entry.docs().join("\n"),
    }
}

impl StorageCatalog for MetadataCatalog<'_> {
    fn pallets(&self) -> Vec<PalletInfo> {
        let mut pallets: Vec<PalletInfo> = self
            .metadata
            .pallets()
            .filter_map(|pallet| {
                let storage = pallet.storage()?;
                Some(PalletInfo {
                    name: pallet.name().to_string(),
                    entry_count: storage.entries().len(),
                })
            })
            .collect();
        pallets.sort_by(|a, b| a.name.cmp(&b.name));
        pallets
    }

    fn entries(&self, pallet: &str) -> Vec<StorageEntryInfo> {
        let Some(pallet_meta) = self.metadata.pallet_by_name(pallet) else {
            return Vec::new();
        };
        let Some(storage) = pallet_meta.storage() else {
            return Vec::new();
        };
        storage
            .entries()
            .iter()
            .map(|entry| entry_info(pallet, entry))
            .collect()
    }

    fn entry(&self, pallet: &str, entry: &str) -> Option<StorageEntryInfo> {
        let storage = self.metadata.pallet_by_name(pallet)?.storage()?;
        let entry_meta = storage.entry_by_name(entry)?;
        Some(entry_info(pallet, entry_meta))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fixtures::polkadot_metadata;

    fn catalog(metadata: &Metadata) -> MetadataCatalog<'_> {
        MetadataCatalog { metadata }
    }

    #[test]
    fn pallets_includes_system_and_balances_with_counts() {
        let metadata = polkadot_metadata();
        let catalog = catalog(&metadata);
        let pallets = catalog.pallets();

        let system = pallets
            .iter()
            .find(|p| p.name == "System")
            .expect("System pallet present");
        let balances = pallets
            .iter()
            .find(|p| p.name == "Balances")
            .expect("Balances pallet present");

        assert!(system.entry_count > 0, "System has storage entries");
        assert!(balances.entry_count > 0, "Balances has storage entries");
    }

    #[test]
    fn system_account_is_a_map_with_one_accountid_key() {
        let metadata = polkadot_metadata();
        let catalog = catalog(&metadata);
        let entry = catalog
            .entry("System", "Account")
            .expect("System.Account present");

        let StorageKind::Map { key_type_ids } = &entry.kind else {
            panic!("System.Account should be a map, got {:?}", entry.kind);
        };
        assert_eq!(key_type_ids.len(), 1, "System.Account has a single key");

        let key_ty = metadata
            .types()
            .resolve(key_type_ids[0])
            .expect("key type resolves in registry");
        let last_segment = key_ty
            .path
            .segments
            .last()
            .map(String::as_str)
            .unwrap_or_default();
        assert_eq!(last_segment, "AccountId32", "key type is AccountId32");
    }

    #[test]
    fn system_number_is_plain() {
        let metadata = polkadot_metadata();
        let catalog = catalog(&metadata);
        let entry = catalog
            .entry("System", "Number")
            .expect("System.Number present");
        assert_eq!(entry.kind, StorageKind::Plain);
    }

    #[test]
    fn unknown_entry_returns_none() {
        let metadata = polkadot_metadata();
        let catalog = catalog(&metadata);
        assert!(catalog.entry("System", "NoSuchEntry").is_none());
        assert!(catalog.entry("NoSuchPallet", "Account").is_none());
    }

    #[test]
    fn entries_are_stably_ordered() {
        let metadata = polkadot_metadata();
        let catalog = catalog(&metadata);

        let first = catalog.entries("System");
        let second = catalog.entries("System");
        assert!(!first.is_empty(), "System has entries");
        assert_eq!(first, second, "entries() is deterministic across calls");

        let pallets_a = catalog.pallets();
        let pallets_b = catalog.pallets();
        assert_eq!(pallets_a, pallets_b, "pallets() is deterministic");
        let mut sorted = pallets_a.clone();
        sorted.sort_by(|a, b| a.name.cmp(&b.name));
        assert_eq!(pallets_a, sorted, "pallets() is alphabetical by name");
    }
}
