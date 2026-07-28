# CLAUDE.md

Guidance for Claude Code in this repository.

## What this is

Relay: generic condition-cranking for Solana programs — see [DESIGN.md](./DESIGN.md) first. Three parts:

- `spec/` — `relay-spec`: the condition wire format. **Zero-copy pod** (`#[repr(C)]` bytemuck structs, fixed sizes, no interior padding) read in place on-chain. bytemuck is the only allowed dependency.
- `programs/` — **separate cargo workspace** (own lockfile, own `target/` via `.cargo/config.toml`): the `relay` program (watch registry + `crank_v0` payment-assert wrapper) and `demo-book` (reference target embedding a condition block as typed pod fields; also hosts the cross-program tests). Anchor v2 = the `anchor-next` alpha, git-pinned to otter-sec/anchor rev `4fbe613...` — the SAME rev as velocity's anchor-v2 workspace; do not bump one without the other.
- `crank-turner/` — root-workspace client crate: the generic turner daemon (solana 3.x tree). Transports: `RpcSource` (polling) and `CachedSource<RpcSource>` fed by either `ws.rs` (`programSubscribe`/`accountSubscribe`) or `grpc.rs` (Yellowstone, pinned to the same git rev as velocity's rust workspace). Its litesvm tests hand-roll all client-side encoding on purpose (ABI check; the root workspace must not depend on the anchor-v2 git tree).

## Build / test

```bash
./scripts/build-programs.sh          # SBF build both programs (cargo-build-sbf --tools-version v1.52)
cd programs && cargo test            # program litesvm tests (need the SBF build first)
cargo test                           # root workspace: spec + crank-turner (turner tests also need the SBF build)
cargo fmt && cargo clippy            # run in BOTH workspaces before declaring work done
```

macOS: if the SBF build fails on missing `assert.h`, `export SDKROOT="$(xcrun --show-sdk-path)"`.

## Rust style

Prefer declarative iterator chains (`map`/`filter`/`fold`/`try_fold`/`find`/`collect`) over imperative `for`/`while` loops wherever the two are performance-equivalent. Explicit loops are fine when they are genuinely better: hot paths where the imperative form saves real work, or indexed mutation across parallel structures that the borrow checker won't allow through closures. Avoid redundant recomputation in loops — hoist values that don't change across iterations.

## Invariants that must not drift

- The pod layouts are ABI: `CONDITION_LEN = 280`, `BLOCK_HEADER_LEN = 16`, `ACCOUNT_REF_LEN = 33`, `RESPONSE_POINTER_LEN = 10`, `WATCH_V0_LEN = 80` are compile-asserted in the spec and re-asserted in tests. Never reorder/resize fields of a `V0` type — add a `V1`.
- Resolvers stage their payload in a writable account and return a pointer; they must never rely on raw return data for the payload (1024-byte cap). Staging is simulation-only — a resolver that a program lets *land* would commit scratch bytes, which is harmless but pointless.
- Condition blocks must sit at an **8-aligned** account-data offset (zero-copy reads require it); `demo-book` compile-asserts its offset.
- Spec constants (`CRANK_V0_DISCRIMINATOR`, `WATCH_V0_DISCRIMINATOR`) are pinned copies of program-generated values; `programs/relay/tests/relay_tests.rs` asserts they match. If you change an instruction/account name, update the spec constant AND the test.
- Wake hints must be conservative: a program may let a hint fire early (costs a simulation) but never late (liveness bug). The demo's `next_expiry_ts` min-over-inserts + executor-repair pattern is the reference.
- Executors take no signers, and `crank_v0` forwards every CPI account `is_signer: false`. Nothing in this system may ever forward signer privilege.
- Program keypairs live OUTSIDE the repo (`~/.config/solana/velocity-keys/relay.json`, `relay-demo-book.json`); only `declare_id!` pubkeys are committed. `.gitignore` blocks `**/*keypair*.json`.

## Git conventions

Never add Claude (or any AI assistant) as a `Co-Authored-By` on commits or PRs. No `🤖 Generated with …` footers.
