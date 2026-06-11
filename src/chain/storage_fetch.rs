//! Dynamic storage key encoding, fetch, and decode (ticket T06).
//!
//! Backs `SubxtChainClient::fetch_pinned`. Holds the subxt-dynamic de-risking
//! spike (spec §10.1): build a dynamic storage address from a `PinnedItem`,
//! fetch at a block hash, and decode to `scale_value::Value` / `CellState`.
