//! Browsable view over runtime metadata's storage (ticket T05).

use subxt::Metadata;

use crate::contracts::{PalletInfo, StorageCatalog, StorageEntryInfo};

/// Reads pallets/entries from runtime metadata. (Stub — implemented in T05.)
pub struct MetadataCatalog<'a> {
    pub metadata: &'a Metadata,
}

impl StorageCatalog for MetadataCatalog<'_> {
    fn pallets(&self) -> Vec<PalletInfo> {
        todo!("T05: enumerate metadata pallets")
    }

    fn entries(&self, _pallet: &str) -> Vec<StorageEntryInfo> {
        todo!("T05: enumerate storage entries for a pallet")
    }

    fn entry(&self, _pallet: &str, _entry: &str) -> Option<StorageEntryInfo> {
        todo!("T05: look up a single storage entry")
    }
}
