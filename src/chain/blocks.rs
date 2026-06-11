//! Block subscription stream feeding the grid (ticket T04).
//!
//! Backs `SubxtChainClient::subscribe_blocks`: maps subxt's best-block
//! subscription to `BlockRef`s. Each `dev_newBlock` in Chopsticks `Manual` mode
//! produces one block here.
