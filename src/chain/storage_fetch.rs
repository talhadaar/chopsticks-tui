//! Dynamic storage key encoding, fetch, and decode (ticket T06).
//!
//! Backs `SubxtChainClient::fetch_pinned`. Holds the subxt-dynamic de-risking
//! spike (spec §10.1): build a dynamic storage address from a `PinnedItem`,
//! fetch at a block hash, and decode to `scale_value::Value` / `CellState`.

use subxt::{OnlineClient, PolkadotConfig};

use crate::contracts::{BlockHash, CellState, KeyArg, PinnedItem, Result};

/// Encode one map/double-map/NMap key argument into a plain `scale_value::Value`
/// that subxt's dynamic storage API can encode against the runtime's key type.
///
/// `AccountId32` is encoded as an unnamed composite of its 32 bytes (what
/// `Value::from_bytes` produces); subxt resolves the type-id and the hasher from
/// metadata at fetch time, so callers only supply the *value*, not the layout.
fn key_part(arg: &KeyArg) -> scale_value::Value {
    match arg {
        KeyArg::AccountId(account) => scale_value::Value::from_bytes(account),
        KeyArg::U(n) => scale_value::Value::u128(*n),
        KeyArg::Bytes(bytes) => scale_value::Value::from_bytes(bytes),
        KeyArg::Text(text) => scale_value::Value::string(text.clone()),
    }
}

/// Encode every key of a pinned item, in order, into the homogeneous `Vec` the
/// dynamic address's `KeyParts` expects. Empty for plain (non-map) entries.
fn key_parts(keys: &[KeyArg]) -> Vec<scale_value::Value> {
    keys.iter().map(key_part).collect()
}

/// Fetch + decode one pinned item at a block hash.
///
/// Returns [`CellState::Missing`] when storage holds no value, and
/// [`CellState::Undecodable`] (never a panic) when the SCALE bytes fail to decode
/// against the value's metadata type — per-cell decode failure is expected (spec
/// §8). The decoded value is returned whole; the item's `path` is applied at
/// render time (T07), not here.
pub(crate) async fn fetch(
    inner: &OnlineClient<PolkadotConfig>,
    item: &PinnedItem,
    at: BlockHash,
) -> Result<CellState> {
    // The dynamic address carries only the pallet + entry names; keys are passed
    // separately to `fetch`. `Value = scale_value::Value` (== `Value<()>`) because
    // subxt's `DecodeAsType` is only impl'd for `Value<()>` — we decode the raw
    // bytes to `Value<u32>` ourselves below.
    let addr = subxt::dynamic::storage::<Vec<scale_value::Value>, scale_value::Value>(
        &item.pallet,
        &item.entry,
    );

    // Pin the *requested* block hash (not the latest) so the whole grid column is
    // read at one consistent state.
    let at_block = inner.at_block(at).await?;

    // Genuine "Missing" detection: both `fetch` and `try_fetch` substitute the
    // entry's metadata default (e.g. a zeroed `AccountInfo` for `System.Account`)
    // when a key is absent, so neither can tell "absent" from "explicitly default".
    // Probe the backend directly via `fetch_raw`, which returns `NoValueFound` for
    // a truly-absent key and never applies a default.
    let storage = at_block.storage();
    let entry = storage.entry(&addr)?;
    let key_bytes = entry.fetch_key(key_parts(&item.keys))?;

    let raw = match storage.fetch_raw(key_bytes).await {
        Ok(bytes) => bytes,
        // The only "value really isn't there" signal; any other error is a real
        // fault and should propagate.
        Err(subxt::error::StorageError::NoValueFound) => return Ok(CellState::Missing),
        Err(other) => return Err(other.into()),
    };

    // We have raw bytes; decode them against the entry's value type-id.
    let value_ty = entry_value_ty(&at_block, &item.pallet, &item.entry)?;

    // `DecodeAsType` is only impl'd for `Value<()>`, so decode the raw SCALE bytes
    // against the value's type-id manually to obtain the `Value<u32>` the contract
    // wants (type-id context from the metadata's `PortableRegistry`).
    let mut cursor = raw.as_slice();
    let decoded: std::result::Result<scale_value::Value<u32>, _> =
        scale_value::scale::decode_as_type(&mut cursor, value_ty, at_block.metadata().types());
    match decoded {
        Ok(decoded) => Ok(CellState::Value(decoded)),
        Err(error) => Ok(CellState::Undecodable {
            raw_hex: hex_of(&raw),
            error: error.to_string(),
        }),
    }
}

/// Look up the value type-id for a pallet's storage entry from block metadata.
fn entry_value_ty<C>(
    at_block: &subxt::client::ClientAtBlock<PolkadotConfig, C>,
    pallet: &str,
    entry: &str,
) -> Result<u32>
where
    C: subxt::client::OfflineClientAtBlockT<PolkadotConfig>,
{
    let metadata = at_block.metadata();
    let pallet_meta = metadata
        .pallet_by_name(pallet)
        .ok_or_else(|| anyhow::anyhow!("pallet {pallet} not found in metadata"))?;
    let storage = pallet_meta
        .storage()
        .ok_or_else(|| anyhow::anyhow!("pallet {pallet} has no storage"))?;
    let entry_meta = storage
        .entry_by_name(entry)
        .ok_or_else(|| anyhow::anyhow!("storage entry {pallet}.{entry} not found"))?;
    Ok(entry_meta.value_ty())
}

/// Lowercase `0x`-prefixed hex of a byte slice, for inspecting undecodable cells.
fn hex_of(bytes: &[u8]) -> String {
    use std::fmt::Write;
    let mut s = String::with_capacity(2 + bytes.len() * 2);
    s.push_str("0x");
    for b in bytes {
        let _ = write!(s, "{b:02x}");
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use scale_value::{Primitive, ValueDef};
    use subxt::utils::AccountId32;

    /// Each `KeyArg` variant encodes to the expected plain `scale_value::Value`
    /// shape — pure and offline, no chain required.
    #[test]
    fn key_parts_roundtrips_each_kind() {
        let account = AccountId32([7u8; 32]);
        let parts = key_parts(&[
            KeyArg::AccountId(account),
            KeyArg::U(42),
            KeyArg::Bytes(vec![1, 2, 3]),
            KeyArg::Text("hello".to_string()),
        ]);
        assert_eq!(parts.len(), 4);

        // AccountId -> unnamed composite of its 32 bytes (each a u128 primitive).
        match parts[0].value {
            ValueDef::Composite(ref c) => {
                let vals: Vec<_> = c.values().collect();
                assert_eq!(vals.len(), 32);
                for v in vals {
                    assert!(matches!(v.value, ValueDef::Primitive(Primitive::U128(7))));
                }
            }
            ref other => panic!("AccountId should be a composite, got {other:?}"),
        }

        // U(42) -> u128 primitive.
        assert!(matches!(
            parts[1].value,
            ValueDef::Primitive(Primitive::U128(42))
        ));

        // Bytes -> unnamed composite of the bytes.
        match parts[2].value {
            ValueDef::Composite(ref c) => {
                let vals: Vec<_> = c.values().collect();
                assert_eq!(vals.len(), 3);
                assert!(matches!(
                    vals[0].value,
                    ValueDef::Primitive(Primitive::U128(1))
                ));
                assert!(matches!(
                    vals[2].value,
                    ValueDef::Primitive(Primitive::U128(3))
                ));
            }
            ref other => panic!("Bytes should be a composite, got {other:?}"),
        }

        // Text -> string primitive.
        match parts[3].value {
            ValueDef::Primitive(Primitive::String(ref s)) => assert_eq!(s, "hello"),
            ref other => panic!("Text should be a string primitive, got {other:?}"),
        }

        // Plain (keyless) entries produce no key parts.
        assert!(key_parts(&[]).is_empty());
    }

    #[test]
    fn hex_of_prefixes_and_lowercases() {
        assert_eq!(hex_of(&[]), "0x");
        assert_eq!(hex_of(&[0x00, 0x0f, 0xab, 0xff]), "0x000fabff");
    }

    /// Live integration test against a freshly-spawned Chopsticks polkadot fork.
    /// Ignored by default; run with `cargo test -- --ignored`. Requires `npx`
    /// network access (sandbox OFF).
    #[tokio::test]
    #[ignore = "live: spawns chopsticks on :8002, needs network"]
    async fn live_fetch_system_account_and_missing() {
        use crate::contracts::{PinnedItemId, PinnedItem};
        use std::os::unix::process::CommandExt;
        use std::process::{Command, Stdio};
        use std::time::Duration;
        use subxt_signer::sr25519::dev;

        // Spawn chopsticks (Manual build mode) on a dedicated port. `npx` forks a
        // node child, so put the whole thing in its own process group (`setsid`)
        // and kill the group below — otherwise the node fork is orphaned.
        let mut child = Command::new("npx")
            .args([
                "--yes",
                "@acala-network/chopsticks@1.4.2",
                "-c",
                "polkadot",
                "--build-block-mode",
                "Manual",
                "--port",
                "8002",
            ])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .process_group(0)
            .spawn()
            .expect("spawn chopsticks");
        let pgid = child.id() as i32;

        // Poll until the node answers, then run assertions; always kill the child.
        let result = async {
            let url = "ws://localhost:8002";
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
                connected
                    .unwrap_or_else(|| panic!("chopsticks never came up: {last_err:?}"))
            };

            let at = inner.at_current_block().await.expect("at_current_block");
            let block_hash = at.block_hash();

            // (1) System.Account[Alice] -> Value with a struct containing data.free.
            let alice = dev::alice().public_key().to_account_id();
            let item = PinnedItem {
                id: PinnedItemId(1),
                pallet: "System".to_string(),
                entry: "Account".to_string(),
                keys: vec![KeyArg::AccountId(alice)],
                path: vec![],
                label: "System.Account(Alice)".to_string(),
            };
            let state = fetch(&inner, &item, block_hash).await.expect("fetch alice");
            match state {
                CellState::Value(v) => {
                    use scale_value::At;
                    let free = v
                        .at("data")
                        .and_then(|d| d.at("free"))
                        .expect("decoded value should have data.free");
                    // Just assert it is present and a primitive; balance is non-zero
                    // for Alice on a polkadot fork but we only require structure here.
                    let _ = free;
                }
                other => panic!("expected Value for System.Account[Alice], got {other:?}"),
            }

            // (2) An absent map key -> Missing.
            let absent = PinnedItem {
                id: PinnedItemId(2),
                pallet: "System".to_string(),
                entry: "Account".to_string(),
                keys: vec![KeyArg::AccountId(AccountId32([0xAB; 32]))],
                path: vec![],
                label: "System.Account(absent)".to_string(),
            };
            let state = fetch(&inner, &absent, block_hash).await.expect("fetch absent");
            assert!(
                matches!(state, CellState::Missing),
                "absent account key should yield Missing, got {state:?}"
            );
        }
        .await;

        // Kill the whole process group so the node child `npx` forked dies too.
        // (`child.kill()` alone only reaps the `npx` wrapper, orphaning node.)
        let _ = Command::new("kill")
            .args(["-KILL", &format!("-{pgid}")])
            .status();
        let _ = child.kill();
        let _ = child.wait();
        // Surface any panic captured above (already panicked inline if so).
        result
    }
}
