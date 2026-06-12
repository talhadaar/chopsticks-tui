# chopsticks-tui

A terminal UI for poking at a forked Polkadot chain and watching state evolve:
spawn (or attach to) a [Chopsticks](https://github.com/AcalaNetwork/chopsticks)
fork, pin storage items, build blocks and send guided transactions, and watch the
pinned values change column-by-column with diffs.

> **Status:** MVP-1 in progress. The module layer (chain client, storage catalog,
> rendering, diff, and the TUI views) is built and tested; the app orchestration
> that wires them into the interactive loop (ticket T14) is not merged yet, so
> `cargo run` currently shows the scaffold screen. See
> [`docs/superpowers/MVP-1/`](docs/superpowers/MVP-1/) for the spec, contracts, and
> tickets.

## Prerequisites

| Tool | Version | Notes |
|------|---------|-------|
| **Rust** | 1.96+ (edition 2024) | Install via [rustup](https://rustup.rs). `rustup update stable`. |
| **Node.js + npx** | Node 18+ (tested on 24) | Needed to run Chopsticks. Install from [nodejs.org](https://nodejs.org) or via `nvm`. |
| **Chopsticks** | `@acala-network/chopsticks@1.4.2` | Not installed manually — it is fetched on demand by `npx` the first time a fork is spawned. |

A network connection is required the first time you spawn a fork: Chopsticks lazily
pulls state from a live upstream RPC, and `npx` downloads the Chopsticks package.

## Build

```sh
cargo build            # debug build
cargo build --release  # optimized build
```

## Test

```sh
cargo test             # runs the offline unit/integration tests
```

The default test run is fully offline (it uses a captured Polkadot metadata fixture
in `src/fixtures/`). Tests that require a live Chopsticks fork are marked
`#[ignore]`; run them explicitly (and ensure Node/network are available):

```sh
cargo test -- --ignored
```

Lint with the same gate CI uses:

```sh
cargo clippy --all-targets -- -D warnings
```

## Run

```sh
cargo run
```

### Running against a fork

Once the connection screen (T13) and app loop (T14) are wired, you will choose to
**spawn** a fork or **attach** to a running one from inside the TUI. To run a fork
manually for development or for the `--ignored` tests, start Chopsticks in manual
build mode:

```sh
# Polkadot fork on ws://localhost:8000, blocks built only on demand
npx @acala-network/chopsticks@1.4.2 -c polkadot --build-block-mode Manual

# add --mock-signature-host to allow impersonating arbitrary accounts
npx @acala-network/chopsticks@1.4.2 -c polkadot --build-block-mode Manual --mock-signature-host
```

`-c` accepts a bundled chain name (e.g. `polkadot`) or a path to a Chopsticks YAML
config. In `Manual` mode the chain only produces a block when one is built
explicitly (`dev_newBlock` / the `[b]` key), which is the intended "poke state,
watch it evolve" rhythm.

## Project layout

```
src/
  contracts.rs       # frozen shared types + traits (Command/Event, ChainClient, ...)
  main.rs            # terminal lifecycle + panic guard
  app/               # AppState + event loop (T14)
  chopsticks.rs      # Chopsticks process supervisor
  chain/             # connection, metadata, block subscription, storage, dev_* rpc
  render/            # value rendering + column diff
  views/             # grid, storage picker, tx builder, connection screen
  fixtures/          # captured metadata blob + sample values for offline tests
docs/superpowers/MVP-1/   # spec, contracts, and per-ticket build plan
```
