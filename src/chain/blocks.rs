//! Block subscription stream feeding the grid (ticket T04).
//!
//! Backs `SubxtChainClient::subscribe_blocks`: turns subxt's view of the chain
//! head into a stream of `BlockRef`s. Each `dev_newBlock` in Chopsticks `Manual`
//! mode produces one block here.
//!
//! ## Why polling, not `stream_best_blocks`
//! subxt 0.50's high-level `OnlineClient::stream_{best,finalized,all}_blocks`
//! drive the new `chainHead_follow` (`archive`/`chainHead`) backend. Chopsticks
//! 1.4.2 does not serve `chainHead_follow` usefully: the follow subscription is
//! accepted but the header stream ends immediately (no `Initialized` block,
//! no events), so every one of those helpers yields an instantly-empty stream.
//! The legacy `chain_subscribeNewHeads`/`at_current_block` paths work fine — the
//! S1 spike relies on `at_current_block()`. Since `subscribe` only receives an
//! `OnlineClient` (which exposes no raw RPC/legacy-subscription handle), we drive
//! the head ourselves: poll `at_current_block()` and emit whenever the head hash
//! changes. This is the only working, in-contract path until the client is built
//! on the legacy backend (see T03) or `subscribe` is handed a raw `RpcClient`.

use std::time::Duration;

use futures::stream::{self, BoxStream, StreamExt};
use subxt::{OnlineClient, PolkadotConfig};

use crate::contracts::{BlockHash, BlockRef, Result};

/// How often the head is polled. Chopsticks `Manual` mode only advances on
/// `dev_newBlock`, so a brisk poll keeps the grid responsive without busy-waiting.
const POLL_INTERVAL: Duration = Duration::from_millis(250);

/// Map one block's number + hash to a [`BlockRef`].
///
/// subxt reports block numbers as `u64`; `BlockRef::number` is the `u32` the
/// grid uses, so this narrows it. (A `u32` overflow would require ~4.2 billion
/// blocks; a Chopsticks fork never reaches that.) Factored out as a pure fn so
/// the mapping is unit-testable without a live node.
fn to_block_ref(number: u64, hash: BlockHash) -> BlockRef {
    BlockRef {
        number: number as u32,
        hash,
    }
}

/// State threaded through the polling [`stream::unfold`]: the client to poll and
/// the hash of the last head we emitted (`None` until the first emission).
struct PollState {
    client: OnlineClient<PolkadotConfig>,
    last_hash: Option<BlockHash>,
}

/// Poll the head once, yielding `Some(BlockRef)` if it advanced past `last_hash`,
/// `None` if the head is unchanged, or an error. Pure-ish helper over the client.
async fn poll_head(client: &OnlineClient<PolkadotConfig>) -> Result<(u64, BlockHash)> {
    let at = client.at_current_block().await?;
    Ok((at.block_number(), at.block_hash()))
}

/// Subscribe to new chain heads, each mapped to a [`BlockRef`].
///
/// Clones `inner` so the returned stream owns its client and is `'static`. Polls
/// `at_current_block()` every [`POLL_INTERVAL`] and emits a `BlockRef` each time
/// the head hash changes (the first observed head is always emitted). Any polling
/// failure surfaces as an `Err(_)` item; the stream never panics and never ends
/// on its own.
pub(crate) fn subscribe(
    inner: &OnlineClient<PolkadotConfig>,
) -> BoxStream<'static, Result<BlockRef>> {
    let init = PollState {
        client: inner.clone(),
        last_hash: None,
    };

    stream::unfold(init, |mut state| async move {
        loop {
            match poll_head(&state.client).await {
                Ok((number, hash)) => {
                    if state.last_hash != Some(hash) {
                        state.last_hash = Some(hash);
                        return Some((Ok(to_block_ref(number, hash)), state));
                    }
                    // Head unchanged: wait and poll again without emitting.
                    tokio::time::sleep(POLL_INTERVAL).await;
                }
                Err(e) => {
                    // Surface the error, then back off so a persistent failure
                    // (e.g. a dropped connection) doesn't spin into an error flood.
                    tokio::time::sleep(POLL_INTERVAL).await;
                    return Some((Err(e), state));
                }
            }
        }
    })
    .boxed()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The pure per-block mapping narrows `u64` -> `u32` and carries the hash.
    #[test]
    fn block_maps_to_blockref() {
        let hash = BlockHash::from([7u8; 32]);
        let block_ref = to_block_ref(42, hash);
        assert_eq!(
            block_ref,
            BlockRef {
                number: 42,
                hash,
            }
        );
    }

    /// Compile-level guarantee that `subscribe` returns exactly the contract
    /// type `BoxStream<'static, Result<BlockRef>>`. Never called — it only has
    /// to type-check, which it can't unless the return type lines up.
    #[allow(dead_code)]
    fn subscribe_returns_static_boxstream(inner: &OnlineClient<PolkadotConfig>) {
        let _stream: BoxStream<'static, Result<BlockRef>> = subscribe(inner);
    }

    /// Live end-to-end check (ignored by default so `cargo test` stays offline).
    ///
    /// Spawns a Chopsticks polkadot fork in `Manual` build mode, subscribes,
    /// builds one block via `dev_newBlock`, and asserts the subscription yields
    /// a `BlockRef` whose number advanced past the head at subscribe time.
    ///
    /// Run with: `cargo test --package chopsticks-tui -- --ignored live`
    #[tokio::test]
    #[ignore = "live: requires spawning a Chopsticks fork"]
    async fn live_subscription_yields_new_block() {
        use std::process::{Command, Stdio};
        use std::time::Duration;

        use subxt::rpcs::client::{RpcClient, rpc_params};

        const WS_URL: &str = "ws://localhost:8001";

        // Spawn the fork in Manual mode so blocks are produced only on demand.
        let mut child = Command::new("npx")
            .args([
                "--yes",
                "@acala-network/chopsticks@1.4.2",
                "-c",
                "polkadot",
                "--build-block-mode",
                "Manual",
                "--port",
                "8001",
            ])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("failed to spawn chopsticks");

        // Wrap the body so the child is always killed, even on assertion failure.
        let result = async {
            // Wait for the ws endpoint to come up (chopsticks boot can be slow).
            let client = {
                let mut last_err = None;
                let mut connected = None;
                for _ in 0..60 {
                    match OnlineClient::<PolkadotConfig>::from_url(WS_URL).await {
                        Ok(c) => {
                            connected = Some(c);
                            break;
                        }
                        Err(e) => {
                            last_err = Some(e);
                            tokio::time::sleep(Duration::from_secs(1)).await;
                        }
                    }
                }
                connected.unwrap_or_else(|| {
                    panic!("chopsticks never came up on {WS_URL}: {last_err:?}")
                })
            };

            // Record the head before building, and open the subscription.
            let start_number = client
                .at_current_block()
                .await
                .expect("at_current_block")
                .block_number();
            let mut stream = subscribe(&client);

            // The subscription emits the current head first; it equals (or trails)
            // `start_number`. Drain those so the post-build assertion is unambiguous.
            // We don't block on this — just clear anything already buffered.
            while let Ok(Some(item)) =
                tokio::time::timeout(Duration::from_millis(500), stream.next()).await
            {
                let block = item.expect("pre-build block item was an error");
                if u64::from(block.number) > start_number {
                    // Already advanced (a block slipped in); nothing more to prove.
                    let _ = (child.kill(), child.wait());
                    return;
                }
            }

            // Drive one block.
            let rpc = RpcClient::from_url(WS_URL).await.expect("rpc connect");
            let _new_hash: String = rpc
                .request("dev_newBlock", rpc_params![])
                .await
                .expect("dev_newBlock");

            // The subscription must now yield a BlockRef whose number advanced.
            let mut advanced = None;
            let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
            while tokio::time::Instant::now() < deadline {
                match tokio::time::timeout(Duration::from_secs(20), stream.next()).await {
                    Ok(Some(item)) => {
                        let block = item.expect("block item was an error");
                        if u64::from(block.number) > start_number {
                            advanced = Some(block);
                            break;
                        }
                    }
                    Ok(None) => panic!("stream ended unexpectedly"),
                    Err(_) => panic!("timed out waiting for a new block"),
                }
            }

            let next = advanced.expect("subscription never advanced past start head");
            assert!(
                u64::from(next.number) > start_number,
                "expected new block number > {start_number}, got {}",
                next.number
            );
        }
        .await;

        let _ = child.kill();
        let _ = child.wait();

        // Surface any panic from the body after the child is cleaned up.
        result
    }
}
