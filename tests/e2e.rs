//! End-to-end harness validating MVP-1's definition of done (spec §9) against a
//! real Chopsticks fork (ticket T15).
//!
//! All chain-touching tests are `#[ignore]`d so the default `cargo test` stays
//! offline. Run them explicitly (single-threaded, since each spawns a fork and a
//! couple share the default port via the supervisor):
//!
//! ```sh
//! cargo test --test e2e -- --ignored --test-threads=1
//! ```
//!
//! Requires Node + network. Each test pins a known-good Chopsticks version.

mod common;

use chopsticks_tui::contracts::{
    ChainClient, ChopsticksSupervisor, DevAccount, ForkConfig, PreparedTx, TxSigner,
};
use chopsticks_tui::render::diff::diff_columns;
use chopsticks_tui::render::value::DefaultRenderer;
use common::*;

/// DoD: one command boots a fork and reaches a connected client with metadata.
/// Exercises the real `Supervisor` (spawn path) end to end.
#[tokio::test]
#[ignore = "live: spawns a Chopsticks fork"]
async fn boot_to_connected_with_metadata() {
    let sup = chopsticks_tui::chopsticks::Supervisor::new();
    let endpoint = sup
        .start(&ForkConfig::Spawn {
            chain_or_path: "polkadot".into(),
            build_mode: chopsticks_tui::contracts::BuildMode::Manual,
            mock_signature_host: true,
        })
        .await
        .expect("supervisor starts a fork");

    let client = chopsticks_tui::chain::client::SubxtChainClient::connect(&endpoint)
        .await
        .expect("client connects");

    assert!(
        client.metadata().pallets().count() > 0,
        "metadata loaded with pallets"
    );
    assert_eq!(client.render_ctx().token_symbol, "DOT");

    sup.shutdown().await.ok();
}

/// DoD: a pinned plain entry updates across ≥3 built blocks (diffs Changed).
#[tokio::test]
#[ignore = "live: spawns a Chopsticks fork"]
async fn plain_entry_updates_across_three_blocks() {
    let fork = spawn_fork("polkadot", 8101, false).await.unwrap();
    let client = connect(&fork).await.unwrap();
    let ctx = client.render_ctx().clone();
    let renderer = DefaultRenderer;

    let number = system_number(1);
    let mut cols = Vec::new();
    for _ in 0..3 {
        let blk = client.build_block().await.expect("build block");
        cols.push(
            fetch_column(&client, std::slice::from_ref(&number), blk.hash)
                .await
                .expect("fetch column"),
        );
    }

    // Each consecutive pair shows the block number changing.
    for pair in cols.windows(2) {
        let d = diff_columns(&pair[0], &pair[1], &renderer, &ctx);
        assert!(
            matches!(
                d.get(&number.id),
                Some(chopsticks_tui::contracts::CellDiff::Changed { .. })
            ),
            "System.Number changes every block"
        );
    }
}

/// DoD: a dev-signed transfer lands in a built block and the pinned balance diffs.
#[tokio::test]
#[ignore = "live: spawns a Chopsticks fork"]
async fn dev_transfer_lands_and_balance_diffs() {
    let fork = spawn_fork("polkadot", 8102, true).await.unwrap();
    fund(&fork.rpc, &alice(), 1_000_000_000_000_000_000)
        .await
        .unwrap();
    let client = connect(&fork).await.unwrap();
    let ctx = client.render_ctx().clone();
    let renderer = DefaultRenderer;

    let free = account_free(1, &alice(), "System.Account(Alice).data.free");

    // Baseline.
    let b0 = client.build_block().await.unwrap();
    let col0 = fetch_column(&client, std::slice::from_ref(&free), b0.hash)
        .await
        .unwrap();

    // Submit Alice -> Bob and build the including block concurrently (Manual mode).
    let tx = PreparedTx {
        pallet: "Balances".into(),
        call: "transfer_keep_alive".into(),
        args: vec![dest(&bob()), scale_value::Value::u128(100_000_000_000_000)],
        signer: TxSigner::Dev(DevAccount::Alice),
        encoded_preview: String::new(),
    };
    let submit = client.submit(tx);
    let build = async {
        tokio::time::sleep(std::time::Duration::from_millis(600)).await;
        client.build_block().await
    };
    let (outcome, included) = tokio::join!(submit, build);
    let outcome = outcome.expect("submit resolves");
    let included = included.expect("block builds");

    assert!(outcome.success, "tx succeeded: {:?}", outcome.error);
    assert!(
        outcome
            .events
            .iter()
            .any(|e| e.pallet == "Balances" && e.variant == "Transfer"),
        "a Balances.Transfer event was emitted"
    );

    // The pinned free balance changed.
    let col1 = fetch_column(&client, std::slice::from_ref(&free), included.hash)
        .await
        .unwrap();
    let d = diff_columns(&col0, &col1, &renderer, &ctx);
    assert!(
        matches!(
            d.get(&free.id),
            Some(chopsticks_tui::contracts::CellDiff::Changed { .. })
        ),
        "Alice's free balance diffed after the transfer"
    );
}

/// DoD: an impersonated transfer (mock-signature-host) lands in a built block.
#[tokio::test]
#[ignore = "live: spawns a Chopsticks fork"]
async fn impersonated_transfer_lands() {
    let fork = spawn_fork("polkadot", 8103, true).await.unwrap();
    fund(&fork.rpc, &charlie(), 1_000_000_000_000_000_000)
        .await
        .unwrap();
    let client = connect(&fork).await.unwrap();

    let tx = PreparedTx {
        pallet: "Balances".into(),
        call: "transfer_keep_alive".into(),
        args: vec![dest(&alice()), scale_value::Value::u128(100_000_000_000_000)],
        signer: TxSigner::Impersonate(charlie()),
        encoded_preview: String::new(),
    };
    let submit = client.submit(tx);
    let build = async {
        tokio::time::sleep(std::time::Duration::from_millis(600)).await;
        client.build_block().await
    };
    let (outcome, _) = tokio::join!(submit, build);
    let outcome = outcome.expect("submit resolves");

    assert!(
        outcome.success,
        "impersonated tx succeeded: {:?}",
        outcome.error
    );
    assert!(
        outcome
            .events
            .iter()
            .any(|e| e.pallet == "Balances" && e.variant == "Transfer"),
        "a Balances.Transfer event was emitted"
    );
}

/// DoD: the same loop works against a second (parachain) fork.
#[tokio::test]
#[ignore = "live: spawns a Chopsticks fork"]
async fn parachain_plain_entry_updates() {
    let fork = spawn_fork("acala", 8104, false).await.unwrap();
    let client = connect(&fork).await.unwrap();
    let ctx = client.render_ctx().clone();
    let renderer = DefaultRenderer;

    let number = system_number(1);
    let a = client.build_block().await.unwrap();
    let col_a = fetch_column(&client, std::slice::from_ref(&number), a.hash)
        .await
        .unwrap();
    let b = client.build_block().await.unwrap();
    let col_b = fetch_column(&client, std::slice::from_ref(&number), b.hash)
        .await
        .unwrap();

    let d = diff_columns(&col_a, &col_b, &renderer, &ctx);
    assert!(
        matches!(
            d.get(&number.id),
            Some(chopsticks_tui::contracts::CellDiff::Changed { .. })
        ),
        "System.Number advances on the parachain fork too"
    );
}

/// DoD: panic safety — the terminal-restore guard is owned by `main`. This checks
/// the contract marker (a full raw-mode test needs a TTY, which CI lacks).
#[test]
fn panic_guard_contract_holds() {
    assert!(chopsticks_tui::app::resilience::panic_guard_installed());
}
