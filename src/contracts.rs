//! Shared types and trait boundaries for the whole crate (ticket T01).
//!
//! Every module codes against the interfaces defined here. See
//! `docs/superpowers/MVP-1/contracts.md` for the rationale and ownership map.

use std::collections::BTreeMap;

use async_trait::async_trait;
use futures::stream::BoxStream;
use scale_value::Value;
use subxt::utils::{AccountId32, H256};
use subxt::{Metadata, rpcs::RpcClient};

/// Crate-wide result type.
pub type Result<T> = anyhow::Result<T>;

/// Hash of a block in the fork.
pub type BlockHash = H256;

// ---------------------------------------------------------------------------
// Core data types (contracts.md §2)
// ---------------------------------------------------------------------------

/// Stable unique id for a pinned storage item.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Serialize, serde::Deserialize,
)]
pub struct PinnedItemId(pub u64);

/// Identifies a block in the fork.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BlockRef {
    pub number: u32,
    pub hash: BlockHash,
}

/// A segment of a path into a decoded value (for nested-field pinning).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum PathSeg {
    Field(String),
    Index(u32),
}

/// A single map/double-map/NMap key argument, pre-encoding. Must round-trip to a
/// `scale_value::Value` for subxt's dynamic storage API.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum KeyArg {
    AccountId(AccountId32),
    /// A numeric key. Serialized as a decimal string so it round-trips through
    /// TOML, which has no `u128` (only i64); P4 sessions persist `KeyArg`.
    U(#[serde(with = "u128_string")] u128),
    Bytes(Vec<u8>),
    Text(String),
}

/// serde adaptor: represent a `u128` as a decimal string (TOML has no u128).
mod u128_string {
    pub fn serialize<S: serde::Serializer>(v: &u128, s: S) -> std::result::Result<S::Ok, S::Error> {
        s.serialize_str(&v.to_string())
    }
    pub fn deserialize<'de, D: serde::Deserializer<'de>>(d: D) -> std::result::Result<u128, D::Error> {
        let s = <String as serde::Deserialize>::deserialize(d)?;
        s.parse::<u128>().map_err(serde::de::Error::custom)
    }
}

/// A user-chosen storage item to watch. Fully specifies how to fetch it and
/// which nested path to display.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct PinnedItem {
    pub id: PinnedItemId,
    pub pallet: String,
    pub entry: String,
    /// Empty for plain entries.
    pub keys: Vec<KeyArg>,
    /// Nested field path into the decoded value; empty = whole value.
    pub path: Vec<PathSeg>,
    /// Display label, e.g. `System.Account(Alice).data.free`.
    pub label: String,
}

/// Decoded state of one pinned item at one block.
#[derive(Debug, Clone)]
pub enum CellState {
    /// Decoded value (`u32` = type-id context from the metadata registry).
    Value(Value<u32>),
    /// Storage returned `None`.
    Missing,
    /// Decode failed; carries the raw hex for inspection. Never panics a render.
    Undecodable { raw_hex: String, error: String },
}

/// One column of the grid: the state of every pinned item at one block.
#[derive(Debug, Clone)]
pub struct BlockColumn {
    pub block: BlockRef,
    pub cells: BTreeMap<PinnedItemId, CellState>,
}

// ---------------------------------------------------------------------------
// Chopsticks supervisor (contracts.md §3.1) — impl: T02
// ---------------------------------------------------------------------------

/// How Chopsticks builds blocks. MVP-1 always spawns `Manual`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum BuildMode {
    Manual,
    Instant,
    Batch,
}

/// A live WebSocket endpoint, e.g. `ws://localhost:8000`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WsEndpoint(pub String);

/// How to obtain a fork: spawn a new Chopsticks process or attach to one.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum ForkConfig {
    Spawn {
        chain_or_path: String,
        build_mode: BuildMode,
        mock_signature_host: bool,
    },
    Attach {
        url: String,
    },
}

/// Owns the Chopsticks child process (or attaches to a running instance).
#[async_trait]
pub trait ChopsticksSupervisor: Send + Sync {
    /// Spawn (or attach) and return the live ws endpoint once it is listening.
    async fn start(&self, cfg: &ForkConfig) -> Result<WsEndpoint>;
    /// Stream of boot/log lines for the connection screen.
    fn log_lines(&self) -> tokio::sync::broadcast::Receiver<String>;
    /// Terminate the child (no-op in attach mode).
    async fn shutdown(&self) -> Result<()>;
}

// ---------------------------------------------------------------------------
// Chain client (contracts.md §3.2) — impl: T03 (connect/metadata), T04
// (subscribe_blocks), T06 (fetch_pinned), T12 (build_block/submit)
// ---------------------------------------------------------------------------

/// The chain-facing client: metadata, subscriptions, dynamic storage, txs.
#[async_trait]
pub trait ChainClient: Send + Sync {
    /// Connect to a fork endpoint and load runtime metadata (T03).
    async fn connect(endpoint: &WsEndpoint) -> Result<Self>
    where
        Self: Sized;
    /// Loaded runtime metadata.
    fn metadata(&self) -> &Metadata;
    /// Newest-block stream driving the grid (T04).
    fn subscribe_blocks(&self) -> BoxStream<'static, Result<BlockRef>>;
    /// Fetch + decode one pinned item at a block (T06).
    async fn fetch_pinned(&self, item: &PinnedItem, at: BlockHash) -> Result<CellState>;
    /// Build a block via `dev_newBlock` (T12).
    async fn build_block(&self) -> Result<BlockRef>;
    /// Submit a prepared extrinsic; resolves after inclusion with its outcome (T12).
    async fn submit(&self, tx: PreparedTx) -> Result<TxOutcome>;
    /// Raw rpc client for `dev_*` passthrough (T12).
    fn rpc(&self) -> &RpcClient;
}

// ---------------------------------------------------------------------------
// Storage catalog (contracts.md §3.3) — impl: T05
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PalletInfo {
    pub name: String,
    pub entry_count: usize,
}

/// Storage entry shape. Hashers are handled by subxt at fetch time, so only the
/// ordered key type-ids are needed to build the picker form.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StorageKind {
    Plain,
    Map { key_type_ids: Vec<u32> },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StorageEntryInfo {
    pub pallet: String,
    pub name: String,
    pub kind: StorageKind,
    /// Index into the metadata type registry.
    pub value_type_id: u32,
    pub docs: String,
}

/// A browsable view over runtime metadata's storage.
pub trait StorageCatalog {
    fn pallets(&self) -> Vec<PalletInfo>;
    fn entries(&self, pallet: &str) -> Vec<StorageEntryInfo>;
    fn entry(&self, pallet: &str, entry: &str) -> Option<StorageEntryInfo>;
}

// ---------------------------------------------------------------------------
// Value rendering (contracts.md §3.4) — impl: T07
// ---------------------------------------------------------------------------

/// Chain context needed to render values readably.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RenderCtx {
    pub ss58_prefix: u16,
    pub token_decimals: u8,
    pub token_symbol: String,
}

/// Renders a decoded value (optionally focused to a nested path) to a compact
/// display string.
pub trait ValueRenderer {
    fn render(&self, value: &Value<u32>, path: &[PathSeg], ctx: &RenderCtx) -> String;
}

// ---------------------------------------------------------------------------
// Diff (contracts.md §3.5) — `diff_columns` impl: T08 (render/diff.rs)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CellDiff {
    Unchanged,
    Changed { from: String, to: String },
    Added,
    Removed,
}

// ---------------------------------------------------------------------------
// Transactions (contracts.md §3.6) — build: T11, submit: T12
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum DevAccount {
    Alice,
    Bob,
    Charlie,
    Dave,
    Eve,
    Ferdie,
}

/// A fully-prepared `dev_setStorage` write (payload refined by P2 from P0's stub).
///
/// The set-storage editor encodes the storage key and the new value to hex up
/// front, so the RPC helper is a thin passthrough and this payload is
/// `serde`-serializable for session replay (P4).
///
/// `key_hex` is the *hashed* storage key (what `StorageEntry::fetch_key`
/// produces); `value_hex` is the SCALE-encoded new value. Both are
/// `0x`-prefixed. A `None` `value_hex` deletes the key (Chopsticks treats a
/// `null` value as a removal) — not surfaced in the MVP-2 UI yet but modelled
/// so the type is complete.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct SetStorageReq {
    /// `0x`-prefixed hashed storage key.
    pub key_hex: String,
    /// `0x`-prefixed SCALE-encoded value, or `None` to delete the key.
    pub value_hex: Option<String>,
    /// Human label for banners / the action log, e.g.
    /// `System.Account(Alice).data.free = 1000 DOT`.
    pub label: String,
}

/// How a `time-travel` target is specified. Parser + remaining variants owned by
/// P4; P0 declares enough for `Command::TimeTravel` to compile.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TimeSpec {
    /// A unix timestamp in milliseconds.
    UnixMillis(u64),
    /// A relative offset like `+6s` / `+1d` (P4 pins the grammar).
    Relative(String),
}

/// How a transaction is signed. `Impersonate` requires the fork spawned with
/// `--mock-signature-host`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TxSigner {
    Dev(DevAccount),
    Impersonate(AccountId32),
}

/// A fully-built, ready-to-submit transaction.
#[derive(Debug, Clone)]
pub struct PreparedTx {
    pub pallet: String,
    pub call: String,
    /// Dynamic call args (no type-id context needed for encoding).
    pub args: Vec<Value<()>>,
    pub signer: TxSigner,
    /// Hex preview, filled by the builder.
    pub encoded_preview: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EventSummary {
    pub pallet: String,
    pub variant: String,
    pub fields: String,
}

#[derive(Debug, Clone)]
pub struct TxOutcome {
    pub success: bool,
    pub events: Vec<EventSummary>,
    pub error: Option<String>,
}

// ---------------------------------------------------------------------------
// UI <-> async messaging (contracts.md §4) — owned by T14, consumed by all
// ---------------------------------------------------------------------------

/// UI loop → async tasks.
#[derive(Debug, Clone)]
pub enum Command {
    Connect(ForkConfig),
    Pin(PinnedItem),
    Unpin(PinnedItemId),
    BuildBlock,
    SubmitTx(PreparedTx),
    Reconnect,
    Quit,
    // --- MVP-2 additions (shared-contracts freeze §3) ---
    /// Write a storage value at the tip (P2 owns dispatch).
    SetStorage(SetStorageReq),
    /// Rewind / jump the chain head to a block (P4 owns dispatch).
    SetHead(u32),
    /// Set the chain timestamp (P4 owns dispatch).
    TimeTravel(TimeSpec),
    /// Switch the Chopsticks build mode (P3 owns dispatch).
    SetBuildMode(BuildMode),
    /// Build one block from the staged extrinsic queue (P3 owns dispatch).
    BuildWithQueue(Vec<PreparedTx>),
    /// Persist the current fork session (P4 owns dispatch).
    SaveSession(String),
    /// Restore a saved session (P4 owns dispatch).
    LoadSession(String),
}

/// Async tasks → UI loop.
#[derive(Debug, Clone)]
pub enum Event {
    BootLog(String),
    Connected {
        metadata_ready: bool,
        ctx: RenderCtx,
    },
    NewColumn(BlockColumn),
    TxResult(TxOutcome),
    Disconnected(String),
    Error(String),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mvp2_command_variants_exist_and_clone() {
        // Constructing each variant proves the shape compiles and is `Clone`.
        let cmds = vec![
            Command::SetStorage(SetStorageReq {
                key_hex: "0x26aa".into(),
                value_hex: Some("0x0100".into()),
                label: "System.Account = …".into(),
            }),
            Command::SetHead(1030),
            Command::TimeTravel(TimeSpec::Relative("+6s".into())),
            Command::SetBuildMode(BuildMode::Manual),
            Command::BuildWithQueue(vec![]),
            Command::SaveSession("snap".into()),
            Command::LoadSession("snap".into()),
        ];
        let _again = cmds.clone();
        assert_eq!(cmds.len(), 7);
    }

    #[test]
    fn pinned_item_round_trips_through_toml() {
        let item = PinnedItem {
            id: PinnedItemId(7),
            pallet: "System".to_string(),
            entry: "Account".to_string(),
            keys: vec![KeyArg::U(42), KeyArg::Text("hi".to_string())],
            path: vec![PathSeg::Field("data".to_string()), PathSeg::Index(0)],
            label: "System.Account(42).data.0".to_string(),
        };
        #[derive(serde::Serialize, serde::Deserialize, PartialEq, Debug)]
        struct Wrap {
            item: PinnedItem,
        }
        let text = toml::to_string(&Wrap { item: item.clone() }).unwrap();
        let back: Wrap = toml::from_str(&text).unwrap();
        assert_eq!(back.item, item);
    }

    #[test]
    fn build_mode_round_trips() {
        #[derive(serde::Serialize, serde::Deserialize, PartialEq, Debug)]
        struct Wrap {
            m: BuildMode,
        }
        let text = toml::to_string(&Wrap { m: BuildMode::Instant }).unwrap();
        let back: Wrap = toml::from_str(&text).unwrap();
        assert_eq!(back.m, BuildMode::Instant);
    }

    #[test]
    fn fork_config_round_trips_through_toml() {
        #[derive(serde::Serialize, serde::Deserialize, PartialEq, Debug)]
        struct Wrap {
            fork: ForkConfig,
        }
        let fork = ForkConfig::Spawn {
            chain_or_path: "polkadot".to_string(),
            build_mode: BuildMode::Manual,
            mock_signature_host: false,
        };
        let text = toml::to_string(&Wrap { fork: fork.clone() }).unwrap();
        let back: Wrap = toml::from_str(&text).unwrap();
        assert_eq!(back.fork, fork);
    }

    #[test]
    fn set_storage_req_round_trips_through_json() {
        let req = SetStorageReq {
            key_hex: "0x26aa".to_string(),
            value_hex: Some("0x0100".to_string()),
            label: "System.Account(Alice).data.free = 1 DOT".to_string(),
        };
        let json = serde_json::to_string(&req).expect("serialize");
        let back: SetStorageReq = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back, req);
    }
}
