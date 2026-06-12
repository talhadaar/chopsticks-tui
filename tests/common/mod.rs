//! Shared spawn/teardown + fixture helpers for the e2e harness (ticket T15).
//!
//! These talk to a real Chopsticks fork. The [`Fork`] guard spawns `npx
//! @acala-network/chopsticks` with `kill_on_drop`, so the child is reaped when the
//! guard drops at the end of a test. Readiness is detected by polling the ws RPC
//! (robust to log-format changes) rather than scraping stdout.
#![allow(dead_code)]

use std::collections::BTreeMap;
use std::time::Duration;

use anyhow::{Context, Result};
use chopsticks_tui::contracts::{
    BlockColumn, BlockHash, BlockRef, ChainClient, KeyArg, PathSeg, PinnedItem, PinnedItemId,
};
use chopsticks_tui::chain::client::SubxtChainClient;
use subxt::rpcs::client::{RpcClient, rpc_params};
use subxt::utils::AccountId32;
use subxt_signer::sr25519::dev;
use tokio::process::{Child, Command};

/// Pinned Chopsticks version (spec §10.4).
pub const CHOPSTICKS: &str = "@acala-network/chopsticks@1.4.2";
/// How long to wait for a fork to come up.
const BOOT_TIMEOUT: Duration = Duration::from_secs(120);

/// A live Chopsticks fork; kills its child process when dropped.
pub struct Fork {
    pub endpoint: String,
    pub rpc: RpcClient,
    _child: Child,
}

/// Spawn a Manual-mode fork of `chain` on `port` and wait until it answers RPC.
///
/// `mock_sig` enables `--mock-signature-host` (needed for impersonation).
pub async fn spawn_fork(chain: &str, port: u16, mock_sig: bool) -> Result<Fork> {
    let mut cmd = Command::new("npx");
    cmd.arg("--yes")
        .arg(CHOPSTICKS)
        .arg("-c")
        .arg(chain)
        .arg("--build-block-mode")
        .arg("Manual")
        .arg("--port")
        .arg(port.to_string());
    if mock_sig {
        cmd.arg("--mock-signature-host");
    }
    // Discard child output so its pipe never fills/blocks; reap on drop.
    cmd.stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true);

    let child = cmd.spawn().context("spawning npx chopsticks")?;
    let endpoint = format!("ws://localhost:{port}");

    // Poll the RPC until the fork answers (or we time out).
    let rpc = tokio::time::timeout(BOOT_TIMEOUT, wait_for_rpc(&endpoint))
        .await
        .with_context(|| format!("fork `{chain}` did not come up on {endpoint}"))??;

    Ok(Fork {
        endpoint,
        rpc,
        _child: child,
    })
}

async fn wait_for_rpc(endpoint: &str) -> Result<RpcClient> {
    loop {
        if let Ok(rpc) = RpcClient::from_url(endpoint).await
            && rpc
                .request::<serde_json::Value>("system_chain", rpc_params![])
                .await
                .is_ok()
        {
            return Ok(rpc);
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
}

/// Connect the real chain client to a fork.
pub async fn connect(fork: &Fork) -> Result<SubxtChainClient> {
    use chopsticks_tui::contracts::WsEndpoint;
    SubxtChainClient::connect(&WsEndpoint(fork.endpoint.clone()))
        .await
        .context("connecting SubxtChainClient")
}

/// Fund an account via Chopsticks `dev_setStorage` so it exists and can pay fees.
pub async fn fund(rpc: &RpcClient, account: &AccountId32, free: u128) -> Result<()> {
    // `free` must be a STRING: a u128 as a JSON number overflows JS's
    // MAX_SAFE_INTEGER (2^53) and polkadot-js rejects it.
    let storage = serde_json::json!({
        "System": { "Account": [[
            [account.to_string()],
            { "providers": 1, "data": { "free": free.to_string() } }
        ]] }
    });
    let _: serde_json::Value = rpc
        .request("dev_setStorage", rpc_params![storage])
        .await
        .context("dev_setStorage funding")?;
    Ok(())
}

/// Fetch every item into a `BlockColumn` at `at` (block number is irrelevant to
/// diffing, so a placeholder is fine).
pub async fn fetch_column(
    client: &SubxtChainClient,
    items: &[PinnedItem],
    at: BlockHash,
) -> Result<BlockColumn> {
    let mut cells = BTreeMap::new();
    for item in items {
        cells.insert(item.id, client.fetch_pinned(item, at).await?);
    }
    Ok(BlockColumn {
        block: BlockRef { number: 0, hash: at },
        cells,
    })
}

// --- pinned-item + account fixtures -----------------------------------------

/// `System.Number` — a plain entry that changes every block.
pub fn system_number(id: u64) -> PinnedItem {
    PinnedItem {
        id: PinnedItemId(id),
        pallet: "System".into(),
        entry: "Number".into(),
        keys: vec![],
        path: vec![],
        label: "System.Number".into(),
    }
}

/// `System.Account(acct).data.free` — a map entry focused to the free balance.
pub fn account_free(id: u64, acct: &AccountId32, label: &str) -> PinnedItem {
    PinnedItem {
        id: PinnedItemId(id),
        pallet: "System".into(),
        entry: "Account".into(),
        keys: vec![KeyArg::AccountId(*acct)],
        path: vec![PathSeg::Field("data".into()), PathSeg::Field("free".into())],
        label: label.into(),
    }
}

pub fn alice() -> AccountId32 {
    dev::alice().public_key().to_account_id()
}
pub fn bob() -> AccountId32 {
    dev::bob().public_key().to_account_id()
}
pub fn charlie() -> AccountId32 {
    dev::charlie().public_key().to_account_id()
}

/// A `MultiAddress::Id(account)` destination value for a Balances call.
pub fn dest(account: &AccountId32) -> scale_value::Value<()> {
    scale_value::Value::unnamed_variant("Id", [scale_value::Value::from_bytes(account.0)])
}
