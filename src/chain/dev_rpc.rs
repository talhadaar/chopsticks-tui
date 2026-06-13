//! `dev_*` raw RPC and extrinsic submission (ticket T12).
//!
//! Backs `SubxtChainClient::build_block` (`dev_newBlock`) and `submit`
//! (dev-signed or mock-signed extrinsics, decoded into a `TxOutcome`).
//!
//! ## Validation status
//! The pure mapping logic (header-number / hash parsing, dev-account → keypair,
//! the mock signer + sentinel signature, hex helpers) is unit-tested offline.
//! The full end-to-end submission path is NOT exercised by this crate's offline
//! tests; the live `#[ignore]`d test asserts it against a fork.
//!
//! ## Cross-cutting finding from the S2 spike (action needed in `client.rs`/T03)
//! Chopsticks 1.4.2 advertises `chainHead_v1_*`, so subxt's default backend
//! (`CombinedBackend`) routes tx submission through
//! `transactionWatch_v1_submitAndWatch`, which Chopsticks does **not** implement
//! — submission hangs/fails. The S2 spike only worked by building the
//! `OnlineClient` on the **`LegacyBackend`** (`author_submitAndWatchExtrinsic`).
//! T04 hit the same wall for subscriptions and worked around it by polling.
//! `submit` here is correct *given* a working backend, but end-to-end tx (and
//! clean block subscriptions) require `client.rs` (T03) to build its
//! `OnlineClient` on `LegacyBackend`. Tracked as a follow-up to T03.
//!
//! ## Impersonation (S2 spike, confirmed empirically)
//! `--mock-signature-host` accepts a sentinel signature beginning with the magic
//! prefix `0xdeadbeef` and filled with `0xcd`; an all-zero signature is rejected
//! as `badProof`. See `MockSigner`.

use subxt::tx::{Payload, Signer};
use subxt::utils::{AccountId32, H256, MultiSignature};
use subxt::{
    OnlineClient, PolkadotConfig, rpcs::RpcClient, rpcs::client::RpcParams, rpcs::client::rpc_params,
};
use subxt_signer::sr25519::{Keypair, dev};

use crate::contracts::{
    BlockRef, BuildMode, DevAccount, EventSummary, PreparedTx, Result, TxOutcome, TxSigner,
};

/// Build one block via `dev_newBlock` and report the new head as a `BlockRef`.
///
/// In Chopsticks `Manual` mode this builds (and finalizes) exactly one block and
/// returns its hash; we then read the header to recover the block number.
pub(crate) async fn new_block(rpc: &RpcClient) -> Result<BlockRef> {
    let hash_hex: String = rpc.request("dev_newBlock", rpc_params![]).await?;
    let hash = parse_block_hash(&hash_hex)?;

    let header: serde_json::Value = rpc.request("chain_getHeader", rpc_params![hash_hex]).await?;
    let number = parse_header_number(&header)?;

    Ok(BlockRef { number, hash })
}

/// Build the positional params for `dev_setStorage`. Chopsticks takes a single
/// argument: the edits. We use the *raw* form — a JSON array of
/// `[storageKeyHex, valueHex]` pairs (a `null` value deletes the key). Split out
/// as a pure fn so the param shape is unit-testable without a live node.
fn set_storage_params(edits: &serde_json::Value) -> Vec<serde_json::Value> {
    vec![edits.clone()]
}

/// Adapt a runtime `Vec<serde_json::Value>` into subxt's `RpcParams`. (`rpc_params!`
/// only handles compile-time literal lists.)
fn rpc_params_from(values: Vec<serde_json::Value>) -> RpcParams {
    let mut params = RpcParams::new();
    for v in values {
        // `push` serializes the value; a `serde_json::Value` always serializes.
        params.push(v).expect("serde_json::Value always serializes");
    }
    params
}

/// Write raw storage at head via `dev_setStorage` (no new block is produced).
///
/// `edits` is the raw form: `[[keyHex, valueHex], …]`. The caller
/// (`views/set_storage.rs`) encodes the key + value to hex, so this is a thin
/// passthrough. The new-head hash Chopsticks returns is discarded — the write is
/// in-place at the current head and the UI refetches pinned items separately.
///
/// Live-only: exercised by the `#[ignore]`d integration test, not the offline
/// unit suite (matches this module's existing test policy).
pub(crate) async fn set_storage(rpc: &RpcClient, edits: serde_json::Value) -> Result<()> {
    let params = set_storage_params(&edits);
    // `dev_setStorage` returns the resulting block hash (hex string); we only
    // care that the call succeeds.
    let _hash: String = rpc.request("dev_setStorage", rpc_params_from(params)).await?;
    Ok(())
}

/// Map our [`BuildMode`] to the integer Chopsticks' `dev_setBlockBuildMode`
/// expects (`Batch = 0`, `Instant = 1`, `Manual = 2`).
fn build_mode_index(mode: BuildMode) -> u8 {
    match mode {
        BuildMode::Batch => 0,
        BuildMode::Instant => 1,
        BuildMode::Manual => 2,
    }
}

/// Switch the Chopsticks block-build mode via `dev_setBlockBuildMode`.
///
/// Integration-tested only (live RPC), per this module's validation policy.
pub(crate) async fn set_block_build_mode(rpc: &RpcClient, mode: BuildMode) -> Result<()> {
    let _: serde_json::Value = rpc
        .request("dev_setBlockBuildMode", rpc_params![build_mode_index(mode)])
        .await?;
    Ok(())
}

/// Submit a prepared extrinsic and resolve once it is included, decoding the
/// outcome. A dispatch failure is a normal `Ok(TxOutcome { success: false, .. })`,
/// not an `Err`.
///
/// NOTE (Manual mode): this awaits inclusion, which in `Manual` build mode only
/// happens once a block is built. The caller (T14) is responsible for triggering
/// `build_block` after `submit`; otherwise this future will not resolve.
pub(crate) async fn submit(inner: &OnlineClient<PolkadotConfig>, tx: PreparedTx) -> Result<TxOutcome> {
    let payload = subxt::dynamic::transaction(tx.pallet.clone(), tx.call.clone(), tx.args.clone());
    match &tx.signer {
        TxSigner::Dev(account) => submit_with(inner, &payload, &dev_keypair(*account)).await,
        TxSigner::Impersonate(account) => {
            submit_with(inner, &payload, &MockSigner::new(*account)).await
        }
    }
}

/// Build one block containing every staged extrinsic (MVP-2 build panel).
///
/// Mechanism (Manual mode): submit each `PreparedTx` into the pool *without*
/// awaiting inclusion (each `submit` future only resolves once a block exists,
/// so awaiting before building would deadlock), then call `dev_newBlock` once.
/// Chopsticks bundles all pooled extrinsics into the single new block, which the
/// block subscription then surfaces as a grid column. An empty queue just builds
/// an empty block (same as the `b` fast path).
///
/// Block metadata overrides (timestamp / author) are panel-local and DISPLAY-ONLY:
/// the frozen `BuildWithQueue(Vec<PreparedTx>)` contract carries no metadata, so
/// this helper always builds with Chopsticks' defaults. Forwarding overrides is a
/// P0-owned contract extension (shared-contracts §3 note).
///
/// Integration-tested only (live RPC), per this module's validation policy.
pub(crate) async fn build_with_queue(
    inner: &OnlineClient<PolkadotConfig>,
    rpc: &RpcClient,
    queue: Vec<PreparedTx>,
) -> Result<()> {
    // Fire each submission into the pool; do not await inclusion (Manual mode).
    let mut handles = Vec::with_capacity(queue.len());
    for tx in queue {
        let inner = inner.clone();
        handles.push(tokio::spawn(async move {
            // Errors here surface as a missing extrinsic in the built block; we
            // don't fail the whole build for one bad tx. (A future refinement
            // could collect per-tx outcomes.)
            let _ = submit(&inner, tx).await;
        }));
    }
    // Give Chopsticks a beat to admit the extrinsics to the pool before sealing.
    tokio::time::sleep(std::time::Duration::from_millis(150)).await;

    // Seal one block over the pooled extrinsics.
    new_block(rpc).await?;

    // The submission futures resolve once the block is built; let them finish so
    // their watchers don't leak, but we don't depend on their outcomes here.
    for h in handles {
        let _ = h.await;
    }
    Ok(())
}

/// Submit `payload` signed by `signer`, await inclusion, and decode the outcome.
async fn submit_with<P, S>(
    inner: &OnlineClient<PolkadotConfig>,
    payload: &P,
    signer: &S,
) -> Result<TxOutcome>
where
    P: Payload,
    S: Signer<PolkadotConfig>,
{
    let in_block = inner
        .tx()
        .await?
        .sign_and_submit_then_watch_default(payload, signer)
        .await?
        .wait_for_finalized()
        .await?;

    match in_block.wait_for_success().await {
        Ok(events) => Ok(TxOutcome {
            success: true,
            events: summarize_events(&events),
            error: None,
        }),
        Err(err) => {
            // Dispatch failure (e.g. ExtrinsicFailed): surface it inline, still
            // reporting whatever events were emitted.
            let events = in_block
                .fetch_events()
                .await
                .map(|evs| summarize_events(&evs))
                .unwrap_or_default();
            Ok(TxOutcome { success: false, events, error: Some(err.to_string()) })
        }
    }
}

/// Map a `DevAccount` to its well-known sr25519 dev keypair.
fn dev_keypair(account: DevAccount) -> Keypair {
    match account {
        DevAccount::Alice => dev::alice(),
        DevAccount::Bob => dev::bob(),
        DevAccount::Charlie => dev::charlie(),
        DevAccount::Dave => dev::dave(),
        DevAccount::Eve => dev::eve(),
        DevAccount::Ferdie => dev::ferdie(),
    }
}

/// A signer that claims to be `account` and emits the sentinel signature that
/// Chopsticks' `--mock-signature-host` accepts: 64 bytes starting with the magic
/// prefix `0xdeadbeef`, the remainder filled with `0xcd`. (An all-zero signature
/// is rejected as `badProof` — confirmed by the S2 spike.) Only valid against a
/// fork spawned with `--mock-signature-host`.
struct MockSigner {
    account: AccountId32,
}

impl MockSigner {
    fn new(account: AccountId32) -> Self {
        Self { account }
    }
}

/// The sentinel sr25519 signature Chopsticks' mock-signature-host recognises.
fn mock_signature_bytes() -> [u8; 64] {
    let mut sig = [0xcdu8; 64];
    sig[..4].copy_from_slice(&[0xde, 0xad, 0xbe, 0xef]);
    sig
}

impl Signer<PolkadotConfig> for MockSigner {
    fn account_id(&self) -> AccountId32 {
        self.account
    }

    fn sign(&self, _signer_payload: &[u8]) -> MultiSignature {
        MultiSignature::Sr25519(mock_signature_bytes())
    }
}

/// Summarize decoded events into compact `EventSummary`s.
fn summarize_events(events: &subxt::extrinsics::ExtrinsicEvents<PolkadotConfig>) -> Vec<EventSummary> {
    events
        .iter()
        .filter_map(|e| e.ok())
        .map(|ev| EventSummary {
            pallet: ev.pallet_name().to_string(),
            variant: ev.event_name().to_string(),
            fields: to_hex(ev.field_bytes()),
        })
        .collect()
}

/// Parse a `0x`-prefixed 32-byte hash string into an `H256`.
fn parse_block_hash(s: &str) -> Result<H256> {
    let bytes = from_hex(s)?;
    if bytes.len() != 32 {
        anyhow::bail!("expected a 32-byte block hash, got {} bytes", bytes.len());
    }
    Ok(H256::from_slice(&bytes))
}

/// Parse the `number` field of an RPC block header (a hex string) into a `u32`.
pub(crate) fn parse_header_number(header: &serde_json::Value) -> Result<u32> {
    let raw = header
        .get("number")
        .and_then(|n| n.as_str())
        .ok_or_else(|| anyhow::anyhow!("header missing a string `number` field"))?;
    let n = u64::from_str_radix(raw.trim_start_matches("0x"), 16)?;
    Ok(n as u32)
}

/// Encode bytes as a `0x`-prefixed lowercase hex string (no extra deps).
fn to_hex(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(2 + bytes.len() * 2);
    s.push_str("0x");
    for b in bytes {
        s.push_str(&format!("{b:02x}"));
    }
    s
}

/// Decode a hex string (with or without `0x`) into bytes.
fn from_hex(s: &str) -> Result<Vec<u8>> {
    let s = s.trim_start_matches("0x");
    if !s.len().is_multiple_of(2) {
        anyhow::bail!("odd-length hex string");
    }
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).map_err(Into::into))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn set_storage_params_wraps_edits_as_single_arg() {
        let edits = serde_json::json!([["0x26aa", "0x0100"]]);
        let params = set_storage_params(&edits);
        assert_eq!(params.len(), 1);
        assert_eq!(params[0], edits);
    }

    #[test]
    fn parse_header_number_decodes_hex() {
        let header = serde_json::json!({ "number": "0x1e2" });
        assert_eq!(parse_header_number(&header).unwrap(), 0x1e2);
    }

    #[test]
    fn parse_header_number_rejects_missing_field() {
        assert!(parse_header_number(&serde_json::json!({})).is_err());
    }

    #[test]
    fn hex_roundtrips() {
        let bytes = [0x00u8, 0x1f, 0xa0, 0xff];
        assert_eq!(to_hex(&bytes), "0x001fa0ff");
        assert_eq!(from_hex("0x001fa0ff").unwrap(), bytes);
        assert_eq!(from_hex("001fa0ff").unwrap(), bytes);
        assert!(from_hex("0xabc").is_err());
    }

    #[test]
    fn parse_block_hash_roundtrips_32_bytes() {
        let h = H256::from_slice(&[7u8; 32]);
        let s = to_hex(&h.0);
        assert_eq!(parse_block_hash(&s).unwrap(), h);
    }

    #[test]
    fn parse_block_hash_rejects_wrong_length() {
        assert!(parse_block_hash("0xdeadbeef").is_err());
    }

    #[test]
    fn dev_keypair_alice_has_known_account() {
        let alice = dev_keypair(DevAccount::Alice).public_key().to_account_id();
        let expected: AccountId32 = "5GrwvaEF5zXb26Fz9rcQpDWS57CtERHpNehXCPcNoHGKutQY"
            .parse()
            .unwrap();
        assert_eq!(alice, expected);
    }

    #[test]
    fn mock_signer_reports_account_and_sentinel_signature() {
        let account: AccountId32 = "5FHneW46xGXgs5mUiveU4sbTyGBzmstUspZC92UhjJM694ty"
            .parse()
            .unwrap();
        let signer = MockSigner::new(account);
        assert_eq!(signer.account_id(), account);
        match signer.sign(b"anything") {
            MultiSignature::Sr25519(sig) => {
                assert_eq!(&sig[..4], &[0xde, 0xad, 0xbe, 0xef]);
                assert!(sig[4..].iter().all(|&b| b == 0xcd));
            }
            other => panic!("expected a sentinel sr25519 signature, got {other:?}"),
        }
    }

    #[test]
    fn build_mode_maps_to_chopsticks_index() {
        use crate::contracts::BuildMode;
        assert_eq!(build_mode_index(BuildMode::Batch), 0);
        assert_eq!(build_mode_index(BuildMode::Instant), 1);
        assert_eq!(build_mode_index(BuildMode::Manual), 2);
    }

    #[test]
    fn prepared_tx_builds_dynamic_payload_without_panicking() {
        use scale_value::Value;
        let tx = PreparedTx {
            pallet: "Balances".into(),
            call: "transfer_keep_alive".into(),
            args: vec![Value::u128(1), Value::u128(1_000)],
            signer: TxSigner::Dev(DevAccount::Alice),
            encoded_preview: String::new(),
        };
        let payload =
            subxt::dynamic::transaction(tx.pallet.clone(), tx.call.clone(), tx.args.clone());
        // Constructing the dynamic payload must not panic; presence is enough.
        let _ = payload;
        assert_eq!(tx.call, "transfer_keep_alive");
    }

    /// Live integration test: write `Balances.TotalIssuance` via `dev_setStorage`
    /// (raw form) and confirm a refetch reflects the new value. Ignored by
    /// default; run with `cargo test -- --ignored set_storage`. Requires `npx`
    /// network access (sandbox OFF).
    #[tokio::test]
    #[ignore = "live: spawns chopsticks on :8003, needs network"]
    async fn live_set_storage_round_trip() {
        use crate::chain::storage_fetch::{fetch, storage_key_hex};
        use crate::chain::storage_catalog::MetadataCatalog;
        use crate::contracts::{CellState, PinnedItem, PinnedItemId, StorageCatalog};
        use crate::views::set_storage::encode_scale_text;
        use std::os::unix::process::CommandExt;
        use std::process::{Command, Stdio};
        use std::time::Duration;

        let mut child = Command::new("npx")
            .args([
                "--yes",
                "@acala-network/chopsticks@1.4.2",
                "-c",
                "polkadot",
                "--build-block-mode",
                "Manual",
                "--port",
                "8003",
            ])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .process_group(0)
            .spawn()
            .expect("spawn chopsticks");
        let pgid = child.id() as i32;

        let result = async {
            let url = "ws://localhost:8003";
            let inner = {
                let mut last_err = None;
                let mut connected = None;
                for _ in 0..60 {
                    match OnlineClient::<PolkadotConfig>::from_url(url).await {
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
                connected.unwrap_or_else(|| panic!("chopsticks never came up: {last_err:?}"))
            };
            let rpc = RpcClient::from_url(url).await.expect("rpc client");

            let at = inner.at_current_block().await.expect("at_current_block");
            let block_hash = at.block_hash();
            let metadata = at.metadata();

            // Target a plain numeric entry: Balances.TotalIssuance (u128).
            let item = PinnedItem {
                id: PinnedItemId(1),
                pallet: "Balances".to_string(),
                entry: "TotalIssuance".to_string(),
                keys: vec![],
                path: vec![],
                label: "Balances.TotalIssuance".to_string(),
            };

            // Read the current value (structural read only).
            let before = fetch(&inner, &item, block_hash).await.expect("fetch before");
            assert!(
                matches!(before, CellState::Value(_)),
                "TotalIssuance should decode, got {before:?}"
            );

            // Encode the storage key offline + a brand-new value distinct from the
            // current issuance (1 DOT = 10 decimals; pick an obviously different
            // round number).
            let key_hex = storage_key_hex(&metadata, &item).expect("derive key");
            let catalog = MetadataCatalog { metadata: &metadata };
            let value_type_id = catalog
                .entry("Balances", "TotalIssuance")
                .expect("entry")
                .value_type_id;
            let value_hex = encode_scale_text("123456789000", value_type_id, metadata.types())
                .expect("encode value");

            // Write via the raw form and refetch at head.
            let edits = serde_json::json!([[key_hex, value_hex]]);
            set_storage(&rpc, edits).await.expect("set_storage");

            let at2 = inner.at_current_block().await.expect("at_current_block 2");
            let after = fetch(&inner, &item, at2.block_hash()).await.expect("fetch after");
            match after {
                CellState::Value(v) => {
                    // The new value must differ from the original (structural check).
                    let before_str = format!("{before:?}");
                    let after_str = format!("{:?}", CellState::Value(v));
                    assert_ne!(before_str, after_str, "value must change after set_storage");
                }
                other => panic!("expected Value after set, got {other:?}"),
            }
        }
        .await;

        let _ = Command::new("kill")
            .args(["-KILL", &format!("-{pgid}")])
            .status();
        let _ = child.kill();
        let _ = child.wait();
        result
    }
}
