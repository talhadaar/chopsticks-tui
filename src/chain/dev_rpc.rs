//! `dev_*` raw RPC and extrinsic submission (ticket T12).
//!
//! Backs `SubxtChainClient::build_block` (`dev_newBlock`) and `submit`
//! (dev-signed or mock-signed extrinsics, decoded into a `TxOutcome`).
