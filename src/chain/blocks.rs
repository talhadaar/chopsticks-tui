//! Block subscription stream feeding the grid (ticket T04).
//!
//! Backs `SubxtChainClient::subscribe_blocks`: maps subxt's best-block
//! subscription to `BlockRef`s. Each `dev_newBlock` in Chopsticks `Manual` mode
//! produces one block here.

use futures::stream::BoxStream;
use subxt::{OnlineClient, PolkadotConfig};

use crate::contracts::{BlockRef, Result};

/// Subscribe to new blocks, mapped to `BlockRef`. Body owned by T04.
pub(crate) fn subscribe(
    _inner: &OnlineClient<PolkadotConfig>,
) -> BoxStream<'static, Result<BlockRef>> {
    todo!("T04: blocks().subscribe_best() mapped to BlockRef")
}
