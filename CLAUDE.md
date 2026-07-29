# CLAUDE.md

Guidance for Claude Code in this repository.

## What this is

Relay: generic condition-cranking for Solana programs — see [DESIGN.md](./DESIGN.md) first. Three parts:

- `spec/` — `relay-spec`: the condition wire format. **Zero-copy pod** (`#[repr(C)]` bytemuck structs, fixed sizes, no interior padding) read in place on-chain. bytemuck is the only allowed dependency.
- `programs/` — **separate cargo workspace** (own lockfile, own `target/` via `.cargo/config.toml`): the `relay` program (watch registry + payment guard instructions) and `demo-book` (reference target embedding a condition block as typed pod fields; also hosts the cross-program tests). Anchor v2 = the `anchor-next` alpha, git-pinned to otter-sec/anchor rev `4fbe613...` — the SAME rev as velocity's anchor-v2 workspace; do not bump one without the other.
- `crank-turner/` — root-workspace client crate: the generic turner daemon (solana 3.x tree). Three pieces: the decision loop (`turner.rs`, decide → concurrent crank → apply), the channel-fed submitter (`submit.rs`, shared blockhash + confirm/resend + profitability), and metrics (`metrics.rs`, Prometheus on `/metrics`). Transports: `RpcSource` (polling) and `CachedSource<RpcSource>` fed by either `ws.rs` (`programSubscribe`/`accountSubscribe`) or `grpc.rs` (Yellowstone, pinned to the same git rev as velocity's rust workspace). Its litesvm tests hand-roll all client-side encoding on purpose (ABI check; the root workspace must not depend on the anchor-v2 git tree).

## Build / test

```bash
./scripts/build-programs.sh          # SBF build both programs (cargo-build-sbf --tools-version v1.52)
cd programs && cargo test            # program litesvm tests (need the SBF build first)
cargo test                           # root workspace: spec + crank-turner (turner tests also need the SBF build)
./scripts/e2e.sh                     # end-to-end on a real validator (needs solana-test-validator);
                                     # includes a scenario that spawns the shipped binary over websocket
cargo fmt && cargo clippy            # run in BOTH workspaces before declaring work done
```

macOS: if the SBF build fails on missing `assert.h`, `export SDKROOT="$(xcrun --show-sdk-path)"`.

## Rust style

Prefer declarative iterator chains (`map`/`filter`/`fold`/`try_fold`/`find`/`collect`) over imperative `for`/`while` loops wherever the two are performance-equivalent. Explicit loops are fine when they are genuinely better: hot paths where the imperative form saves real work, or indexed mutation across parallel structures that the borrow checker won't allow through closures. Avoid redundant recomputation in loops — hoist values that don't change across iterations.

## Invariants that must not drift

- The pod layouts are ABI: `CONDITION_LEN = 280`, `BLOCK_HEADER_LEN = 16`, `ACCOUNT_REF_LEN = 33`, `RESPONSE_POINTER_LEN = 10`, `WATCH_V0_LEN = 112` are compile-asserted in the spec and re-asserted in tests. Never reorder/resize fields of a `V0` type — add a `V1`.
- `WatchV0.target_program` must stay at `WATCH_TARGET_PROGRAM_OFFSET` (8) and must keep being read from the target account's owner, never from instruction args — turners memcmp-filter the registry on it, and a forgeable value would let anyone bypass an operator's allowlist.
- Resolvers stage their payload in a writable account and return a pointer; they must never rely on raw return data for the payload (1024-byte cap). Staging is simulation-only — a resolver that a program lets *land* would commit scratch bytes, which is harmless but pointless.
- Condition blocks must sit at an **8-aligned** account-data offset (zero-copy reads require it); `demo-book` compile-asserts its offset.
- Spec constants (`BEGIN_GUARD_V0_DISCRIMINATOR`, `ASSERT_PAID_V0_DISCRIMINATOR`, `WATCH_V0_DISCRIMINATOR`, `GUARD_SEED`) are pinned copies of program-generated values; `programs/relay/tests/relay_tests.rs` asserts they match. If you change an instruction/account name, update the spec constant AND the test.
- Wake hints must be conservative: a program may let a hint fire early (costs a simulation) but never late (liveness bug). The demo's `next_expiry_ts` min-over-inserts + executor-repair pattern is the reference.
- The binding safety check is `Turner::signer_leak`, run inside `sign_for_submission` on the exact list about to be signed. The earlier check in `try_crank` exists only for a clean skip reason. If you add a submission path, route it through `sign_for_submission`, never `signed_tx` directly.
- **Signer status is transaction-global.** A compiled message has no per-instruction signer flag: `is_signer` comes from the account's position in the message's signer section, so `AccountMeta { is_signer: false }` demotes nothing for an account that signs elsewhere in the transaction. Executors naming the *payout* is expected and allowed; naming a *signer*, or asking for one, is what is refused. `hostile_drain_succeeds_with_is_signer_false` proves the flag is not a defense — read it before touching any of this. The payout account must therefore be separate from the fee payer, and untrusted executors must never name the fee payer (`names_signer` enforces this). Never relax either without understanding that an executor can CPI a System transfer with any account that signs.
- Trusted programs (`trusted_programs`) skip guards and payout separation. That list is a loaded gun: only programs the operator controls belong on it.
- **Never reintroduce a CPI wrapper around executors.** Relay asserts around the call (guards), it does not mediate the call: a wrapper would consume one of the four CPI levels the executor's own stack needs (velocity → CLOB is already two deep) and add per-invoke cost. If a guard needs more context, extend the guard instructions, not the call path.
- Program keypairs live OUTSIDE the repo (`~/.config/solana/velocity-keys/relay.json`, `relay-demo-book.json`); only `declare_id!` pubkeys are committed. `.gitignore` blocks `**/*keypair*.json`.

## Test layout mirrors

`crank-turner/tests/*` hand-pin demo-book's offsets and sizes (`BOOK_ACCOUNT_LEN`, `CONDITIONS_OFFSET`, `ENTRY_COUNT_OFFSET`, `STAGING_OFFSET`, ...) so the turner crates never depend on the anchor-v2 tree. **Any change to `BookV0` breaks them**, usually as `InvalidInstructionData` or a nonsense assertion rather than a clean failure — re-read `programs/demo-book/src/state.rs` and update both test files. The e2e test guards itself with a per-side/total consistency check for exactly this reason.

## What the e2e is for

`scripts/e2e.sh` exists to catch what litesvm structurally cannot: real RPC limits and encodings, commitment lag, on-chain atomicity, and the daemon's own plumbing. It has already found five defects the unit suites passed clean on — an unchunked `getMultipleAccounts` (RPC caps at 100), a compute limit summed from probes that never saw the appended instruction, a duplicate submission from too-short post-send suppression, a websocket backoff that never reset after a working session, and — the worst — a clock served from cache with no freshness check, which froze every timestamp and slot wake the moment the feed died while the turner still looked healthy.

Two properties the fleet scenario pinned down, both worth knowing before running more than one turner. On-chain failures are counted as `relay_transactions_total{result="failed"}`, **not** `relay_crank_failures_total` — that one covers pre-submission stages only, so alerting on it alone would miss every reverted crank. And uncoordinated turners duplicate everything: two of them each crank every ready condition, so one of the pair always burns a fee for a revert. That is inherent to permissionless racing, not a bug, but it means fleet cost scales with turner count while revenue does not. Losing is at least self-limiting — the loser's next tick re-reads the target, finds the work done, and resolves to no-work rather than resubmitting. When adding turner behavior, ask whether it can only be wrong against a real cluster; if so it belongs here.

Known gaps, in rough priority: the gRPC transport is never executed (needs a geyser plugin in the validator); the submitter's resend / re-sign / `Expired` path never fires because blockhashes do not expire in a short test; nothing restarts the validator itself (only the websocket is severed, via the proxy in `daemon_survives_losing_its_subscription`).

Two scenarios are load-bearing enough to be worth naming. `daemon_handles_a_registry_larger_than_one_rpc_call` runs 120 books, past the 100-key ceiling on a single account read, and asserts nothing fails at either stage. `two_turners_share_one_registry_without_wedging` runs two independent daemons on one registry over two rounds of work: the second round is the assertion, since a turner that treated a lost race as fatal would never finish it. Both tests batch their setup against the 1232-byte packet limit (two books per transaction, twelve quotes per transaction); if you add a field to `BookV0` or `WatchV0`, those batch sizes are what breaks first.

## Turner invariants

- Simulation is **local** (`local_sim.rs`, an in-process LiteSVM lazy-fork). Do not add code paths that simulate over RPC; `--remote-sim` exists only as a cross-check. Accounts come cache-first, so keep `--watch-program` coverage in mind when adding account reads.
- Everything the turner reads goes through the freshness rule, **including the clock** — it is an account like any other, and serving it blind is what froze all time-based wakes when the feed died. Do not add a read path that bypasses `needs_revalidation`.
- Cache freshness is a **correctness** invariant, not a tuning knob: an account may only be served from cache without revalidation when a backend has published live `Coverage` for it *and* the feed is healthy. Never widen that (e.g. "trust anything we once fetched") — it feeds stale state into simulation. Backends must publish `Coverage::default()` the moment a session drops.
- Packed transactions must keep each crank's `[begin_guard, executor, assert_paid]` triple **contiguous** — that is the only reason one guard account can serve a whole pack.

- `tick()`'s concurrent phase must stay `&self`-only: decisions produce `StateUpdate`s that are applied afterwards. If you find yourself wanting a lock or a channel inside the crank path, the phase split is being violated.
- The submitter, not the turner, owns send/confirm/resend. The decision loop must never await a confirmation.
- litesvm's `latest_blockhash` in tests must NOT call `expire_blockhash` — it races concurrent signers and surfaces as spurious `BlockhashNotFound`.

## Git conventions

Never add Claude (or any AI assistant) as a `Co-Authored-By` on commits or PRs. No `🤖 Generated with …` footers.
