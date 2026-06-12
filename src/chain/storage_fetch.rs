//! Dynamic storage key encoding, fetch, and decode (ticket T06).
//!
//! Backs `SubxtChainClient::fetch_pinned`. Holds the subxt-dynamic de-risking
//! spike (spec §10.1): build a dynamic storage address from a `PinnedItem`,
//! fetch at a block hash, and decode to `scale_value::Value` / `CellState`.

use subxt::{OnlineClient, PolkadotConfig};

use crate::contracts::{BlockHash, CellState, PinnedItem, Result};

/// Fetch + decode one pinned item at a block hash. Body owned by T06.
pub(crate) async fn fetch(
    _inner: &OnlineClient<PolkadotConfig>,
    _item: &PinnedItem,
    _at: BlockHash,
) -> Result<CellState> {
    todo!("T06: dynamic storage fetch + decode")
}
