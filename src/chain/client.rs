//! `SubxtChainClient` — the `ChainClient` implementation.
//!
//! T03 owns `connect`/`metadata`/`rpc`; T04, T06, and T12 fill in
//! `subscribe_blocks`, `fetch_pinned`, and `build_block`/`submit` respectively
//! (refactoring them to delegate to `blocks`, `storage_fetch`, `dev_rpc`).

use async_trait::async_trait;
use futures::stream::BoxStream;
use subxt::{Metadata, rpcs::RpcClient};

use crate::contracts::{
    BlockHash, BlockRef, CellState, ChainClient, PinnedItem, PreparedTx, Result, TxOutcome,
    WsEndpoint,
};

/// Connects to a fork and serves metadata, subscriptions, storage, and txs.
/// (Stub — implemented across T03/T04/T06/T12.)
pub struct SubxtChainClient;

#[async_trait]
impl ChainClient for SubxtChainClient {
    async fn connect(_endpoint: &WsEndpoint) -> Result<Self>
    where
        Self: Sized,
    {
        todo!("T03: OnlineClient::from_url + load metadata + derive RenderCtx")
    }

    fn metadata(&self) -> &Metadata {
        todo!("T03: return loaded metadata")
    }

    fn subscribe_blocks(&self) -> BoxStream<'static, Result<BlockRef>> {
        todo!("T04: blocks().subscribe_best() mapped to BlockRef")
    }

    async fn fetch_pinned(&self, _item: &PinnedItem, _at: BlockHash) -> Result<CellState> {
        todo!("T06: dynamic storage fetch + decode")
    }

    async fn build_block(&self) -> Result<BlockRef> {
        todo!("T12: dev_newBlock")
    }

    async fn submit(&self, _tx: PreparedTx) -> Result<TxOutcome> {
        todo!("T12: sign/mock-sign + submit + decode outcome")
    }

    fn rpc(&self) -> &RpcClient {
        todo!("T03: return the rpc client")
    }
}
