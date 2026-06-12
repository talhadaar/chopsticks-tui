//! `dev_*` raw RPC and extrinsic submission (ticket T12).
//!
//! Backs `SubxtChainClient::build_block` (`dev_newBlock`) and `submit`
//! (dev-signed or mock-signed extrinsics, decoded into a `TxOutcome`).

use subxt::{OnlineClient, PolkadotConfig, rpcs::RpcClient};

use crate::contracts::{BlockRef, PreparedTx, Result, TxOutcome};

/// Build one block via `dev_newBlock`. Body owned by T12.
pub(crate) async fn new_block(_rpc: &RpcClient) -> Result<BlockRef> {
    todo!("T12: dev_newBlock")
}

/// Sign/mock-sign, submit, and decode the outcome. Body owned by T12.
pub(crate) async fn submit(
    _inner: &OnlineClient<PolkadotConfig>,
    _tx: PreparedTx,
) -> Result<TxOutcome> {
    todo!("T12: sign/mock-sign + submit + decode outcome")
}
