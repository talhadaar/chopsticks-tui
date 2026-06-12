//! `SubxtChainClient` — the `ChainClient` implementation.
//!
//! T03 owns `connect`/`metadata`/`rpc`; T04, T06, and T12 fill in
//! `subscribe_blocks`, `fetch_pinned`, and `build_block`/`submit` respectively
//! (each delegating to `blocks`, `storage_fetch`, `dev_rpc`).

use anyhow::Context;
use async_trait::async_trait;
use futures::stream::BoxStream;
use subxt::rpcs::client::rpc_params;
use subxt::{Metadata, OnlineClient, PolkadotConfig, rpcs::RpcClient};

use crate::chain::{blocks, dev_rpc, storage_fetch};
use crate::contracts::{
    BlockHash, BlockRef, CellState, ChainClient, PinnedItem, PreparedTx, RenderCtx, Result,
    TxOutcome, WsEndpoint,
};

/// Polkadot defaults used when `system_properties` omits a field.
const DEFAULT_SS58_PREFIX: u16 = 0;
const DEFAULT_TOKEN_DECIMALS: u8 = 10;
const DEFAULT_TOKEN_SYMBOL: &str = "DOT";

/// Connects to a fork and serves metadata, subscriptions, storage, and txs.
///
/// `connect` (T03) populates all state; the other methods are thin delegations
/// to the sibling modules so later tickets never touch this file.
pub struct SubxtChainClient {
    inner: OnlineClient<PolkadotConfig>,
    /// Metadata pinned at connect time. `ArcMetadata` is `Arc<Metadata>`, so an
    /// owned clone is just a cheap refcount bump and we can hand out `&Metadata`.
    metadata: subxt::metadata::ArcMetadata,
    rpc: RpcClient,
    render_ctx: RenderCtx,
}

impl SubxtChainClient {
    /// The underlying online client, for the sibling modules' free fns.
    pub(crate) fn inner(&self) -> &OnlineClient<PolkadotConfig> {
        &self.inner
    }

    /// The chain context derived from `system_properties` at connect time.
    pub fn render_ctx(&self) -> &RenderCtx {
        &self.render_ctx
    }
}

#[async_trait]
impl ChainClient for SubxtChainClient {
    async fn connect(endpoint: &WsEndpoint) -> Result<Self>
    where
        Self: Sized,
    {
        let inner = OnlineClient::<PolkadotConfig>::from_url(&endpoint.0)
            .await
            .with_context(|| format!("connecting OnlineClient to {}", endpoint.0))?;

        // Metadata is per-block in subxt 0.50: pin the current finalized block and
        // read its metadata. Keep the `Arc` so we own it for the client's lifetime.
        let at = inner
            .at_current_block()
            .await
            .context("pinning current block for metadata")?;
        let metadata = at.metadata();

        let rpc = RpcClient::from_url(&endpoint.0)
            .await
            .with_context(|| format!("opening rpc client to {}", endpoint.0))?;

        // `system_properties` is best-effort: fall back to Polkadot defaults.
        let props: serde_json::Value = rpc
            .request("system_properties", rpc_params![])
            .await
            .unwrap_or(serde_json::Value::Null);
        let render_ctx = derive_render_ctx(&props);

        Ok(Self {
            inner,
            metadata,
            rpc,
            render_ctx,
        })
    }

    fn metadata(&self) -> &Metadata {
        &self.metadata
    }

    fn subscribe_blocks(&self) -> BoxStream<'static, Result<BlockRef>> {
        blocks::subscribe(self.inner())
    }

    async fn fetch_pinned(&self, item: &PinnedItem, at: BlockHash) -> Result<CellState> {
        storage_fetch::fetch(self.inner(), item, at).await
    }

    async fn build_block(&self) -> Result<BlockRef> {
        dev_rpc::new_block(self.rpc()).await
    }

    async fn submit(&self, tx: PreparedTx) -> Result<TxOutcome> {
        dev_rpc::submit(self.inner(), tx).await
    }

    fn rpc(&self) -> &RpcClient {
        &self.rpc
    }
}

/// Derive a [`RenderCtx`] from a `system_properties` JSON object.
///
/// The RPC shape is `{ "ss58Format": .., "tokenDecimals": .., "tokenSymbol": .. }`,
/// but each field may be absent, a scalar, or a single-element array (chains that
/// support multiple tokens report arrays). Missing/unreadable fields fall back to
/// Polkadot defaults (`0` / `10` / `"DOT"`).
fn derive_render_ctx(props: &serde_json::Value) -> RenderCtx {
    RenderCtx {
        ss58_prefix: props
            .get("ss58Format")
            .and_then(first_u64)
            .map(|n| n as u16)
            .unwrap_or(DEFAULT_SS58_PREFIX),
        token_decimals: props
            .get("tokenDecimals")
            .and_then(first_u64)
            .map(|n| n as u8)
            .unwrap_or(DEFAULT_TOKEN_DECIMALS),
        token_symbol: props
            .get("tokenSymbol")
            .and_then(first_str)
            .unwrap_or_else(|| DEFAULT_TOKEN_SYMBOL.to_string()),
    }
}

/// Read a `u64` from a value that may be the number itself or a non-empty array
/// whose first element is a number.
fn first_u64(v: &serde_json::Value) -> Option<u64> {
    match v {
        serde_json::Value::Array(items) => items.first().and_then(serde_json::Value::as_u64),
        other => other.as_u64(),
    }
}

/// Read a `String` from a value that may be the string itself or a non-empty
/// array whose first element is a string.
fn first_str(v: &serde_json::Value) -> Option<String> {
    match v {
        serde_json::Value::Array(items) => items
            .first()
            .and_then(serde_json::Value::as_str)
            .map(str::to_string),
        other => other.as_str().map(str::to_string),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn render_ctx_derives_from_properties() {
        // Polkadot-style: ss58 scalar, decimals/symbol as single-element arrays.
        let props = json!({
            "ss58Format": 0,
            "tokenDecimals": [10],
            "tokenSymbol": ["DOT"],
        });
        let ctx = derive_render_ctx(&props);
        assert_eq!(
            ctx,
            RenderCtx {
                ss58_prefix: 0,
                token_decimals: 10,
                token_symbol: "DOT".to_string(),
            }
        );

        // Scalar forms are accepted too.
        let scalar = json!({
            "ss58Format": 42,
            "tokenDecimals": 12,
            "tokenSymbol": "UNIT",
        });
        let ctx = derive_render_ctx(&scalar);
        assert_eq!(ctx.ss58_prefix, 42);
        assert_eq!(ctx.token_decimals, 12);
        assert_eq!(ctx.token_symbol, "UNIT");
    }

    /// Compile-level guarantee that `inner()` and the three sibling free fns
    /// exist with the exact signatures the delegations (and T04/T06/T12) rely on.
    /// The helper is never called — it only has to *type-check*, which it can't
    /// unless every path, argument type, and return type lines up, so the
    /// `todo!()` stubs in those modules can't silently drift out of shape.
    #[allow(dead_code, unreachable_code, clippy::diverging_sub_expression)]
    async fn delegation_surface_typechecks(
        client: &SubxtChainClient,
        item: &PinnedItem,
        at: BlockHash,
        tx: PreparedTx,
    ) {
        // `inner()` returns `&OnlineClient<PolkadotConfig>`.
        let inner: &OnlineClient<PolkadotConfig> = client.inner();

        // Each binding pins the free fn's exact return type.
        let _subscribe: BoxStream<'static, Result<BlockRef>> = blocks::subscribe(inner);
        let _fetch: Result<CellState> = storage_fetch::fetch(inner, item, at).await;
        let _new_block: Result<BlockRef> = dev_rpc::new_block(client.rpc()).await;
        let _submit: Result<TxOutcome> = dev_rpc::submit(inner, tx).await;
    }

    #[test]
    fn render_ctx_falls_back_when_fields_absent() {
        // Empty object: every field falls back to the Polkadot default.
        let ctx = derive_render_ctx(&json!({}));
        assert_eq!(ctx.ss58_prefix, DEFAULT_SS58_PREFIX);
        assert_eq!(ctx.token_decimals, DEFAULT_TOKEN_DECIMALS);
        assert_eq!(ctx.token_symbol, DEFAULT_TOKEN_SYMBOL);

        // `Null` (what we substitute when the RPC errors) also falls back.
        let ctx = derive_render_ctx(&serde_json::Value::Null);
        assert_eq!(ctx.ss58_prefix, DEFAULT_SS58_PREFIX);
        assert_eq!(ctx.token_decimals, DEFAULT_TOKEN_DECIMALS);
        assert_eq!(ctx.token_symbol, DEFAULT_TOKEN_SYMBOL);

        // Empty arrays / wrong types fall back per-field.
        let partial = json!({
            "ss58Format": 7,
            "tokenDecimals": [],
            "tokenSymbol": null,
        });
        let ctx = derive_render_ctx(&partial);
        assert_eq!(ctx.ss58_prefix, 7);
        assert_eq!(ctx.token_decimals, DEFAULT_TOKEN_DECIMALS);
        assert_eq!(ctx.token_symbol, DEFAULT_TOKEN_SYMBOL);
    }
}
