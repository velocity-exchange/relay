# CLAUDE.md

Guidance for Claude Code in this repository.

## What this is

Relay: generic condition-cranking for Solana programs — see [DESIGN.md](./DESIGN.md) first. Three parts:

- `spec/` — `relay-spec`: the condition wire format. **Zero-copy pod** (`#[repr(C)]` bytemuck structs, fixed sizes, no interior padding, **alignment 1** — every scalar is a LE byte array behind an accessor) read in place by every reader, on chain or off. bytemuck is the only allowed dependency.
- `relay-anchor/` — a **third** workspace (own lockfile + target): `relay-anchor` wraps a spec block for hosting in an Anchor 1.0 (crates.io) account — `Deref` to the spec type, `Pod`, `ConditionBlock` by delegation, and an `IdlBuild` impl describing the region as opaque bytes. It is generic over the spec version (`RelayBlockHost<V: RelayBlockVersion>`, with `RelayBlock<C, R>` the v0 alias) so a future `RelayBlockV1` is a new alias rather than a breaking change; the intended migration is documented in its module docs. anchor-lang is an **optional** dep behind `idl-build`, and a host must forward its own `idl-build` feature to ours. Separate from both other workspaces because it is the one crate here that depends on anchor at all — `relay-spec` can be a member of everything only because it is zero-dep. Velocity consumes it as a git dep.
- `programs/` — **separate cargo workspace** (own lockfile, own `target/` via `.cargo/config.toml`): the `relay` program (watch registry + payment guard instructions) and `demo-book` (reference target embedding a condition block as typed pod fields, with one `resolve_v0` serving all three of its conditions; also hosts the cross-program tests). Anchor v2 = the `anchor-next` alpha, git-pinned to otter-sec/anchor rev `abacd0e...` — the SAME rev as `spec`'s optional `idl-build-v2` dependency and as velocity's anchor-v2 workspace; do not bump one without the others. Three things track that pin and break the build when they drift from it: the package is published as `anchor-lang` and is aliased here to `anchor-lang-v2`, which needs `extern crate anchor_lang_v2 as anchor_lang;` at each program root because the derives emit `anchor_lang::` paths; instruction args serialize with `wincode`, which must be the version anchor itself uses (0.6); and the test `litesvm` must be the version `anchor-v2-testing` pins (`=0.13.1`), or the tests see two litesvm crates and its types stop matching.
- `crank-turner/` — root-workspace client crate: the generic turner daemon (solana 3.x tree). Three pieces: the decision loop (`turner.rs`, decide → concurrent crank → apply), the channel-fed submitter (`submit.rs`, shared blockhash + confirm/resend + profitability), and metrics (`metrics.rs`, Prometheus on `/metrics`). Transports: `RpcSource` (polling) and `CachedSource<RpcSource>` fed by either `ws.rs` (`programSubscribe`/`accountSubscribe`) or `grpc.rs` (Yellowstone, pinned to the same git rev as velocity's rust workspace). Its litesvm tests hand-roll all client-side encoding on purpose (ABI check; the root workspace must not depend on the anchor-v2 git tree).

## Build / test

```bash
./scripts/build-programs.sh          # SBF build both programs (cargo-build-sbf --tools-version v1.54 — v1.52 miscompiles both programs into runtime access violations, same pathology velocity documents)
cd programs && cargo test            # program litesvm tests (need the SBF build first)
cargo test                           # root workspace: spec + crank-turner (turner tests also need the SBF build)
cargo test --manifest-path relay-anchor/Cargo.toml   # the anchor 1.0 host wrapper (and again with --features idl-build)
./scripts/e2e.sh                     # end-to-end on a real validator (needs solana-test-validator 4.2+, for the txv1 feature);
                                     # includes a scenario that spawns the shipped binary over websocket
cargo fmt && cargo clippy            # run in ALL THREE workspaces before declaring work done
```

macOS: if the SBF build fails on missing `assert.h`, `export SDKROOT="$(xcrun --show-sdk-path)"`.

## CI / deploys

- `.github/workflows/ci.yml` runs on every push/PR: fmt across both workspaces, the SBF fixture build (v1.54 via `scripts/build-programs.sh`), root-workspace tests, programs-workspace tests, clippy. The programs suite is a hard gate — it is what was missing when a spec API change broke demo-book's tests silently.
- Deploys mirror velocity's Squads flow and only ever PROPOSE: `manual-devnet-deploy.yaml` (workflow_dispatch, pick a branch) and `release-program.yaml` (`program-relay-<version>` tag) build relay.so with platform-tools v1.54, stage it in a buffer owned by the multisig vault, and propose the upgrade — signers approve + execute in the Squads UI. Required repo secrets: `DEVNET_RPC_ENDPOINT`, `DEVNET_DEPLOYER_KEYPAIR`, `DEVNET_MULTISIG`, `DEVNET_MULTISIG_VAULT`, and the `MAINNET_*` equivalents.
- The deploy build is plain `cargo-build-sbf --tools-version v1.54` (the exact toolchain every test fixture validates), NOT the solana-verifiable-build image: the image's bundled cargo-build-sbf picks its own default platform-tools, and v1.52 miscompiles both programs here. Vet an image's default toolchain against that before moving to verifiable builds.

## Rust style

Prefer declarative iterator chains (`map`/`filter`/`fold`/`try_fold`/`find`/`collect`) over imperative `for`/`while` loops wherever the two are performance-equivalent. Explicit loops are fine when they are genuinely better: hot paths where the imperative form saves real work, or indexed mutation across parallel structures that the borrow checker won't allow through closures. Avoid redundant recomputation in loops — hoist values that don't change across iterations.

## Invariants that must not drift

- The pod layouts are ABI: `CONDITION_LEN = 192`, `BLOCK_HEADER_LEN = 16`, `ACCOUNT_REF_LEN = 33`, `RESPONSE_POINTER_LEN = 10`, `RESOLVED_HEADER_LEN = 43`, `FIRED_CONDITION_LEN = 37`, `WATCH_V0_LEN = 112` are compile-asserted in the spec and re-asserted in tests. Never reorder/resize fields of a `V0` type — add a `V1`.
- **Everything on the evaluation path is alignment 1**, which is why there is exactly one reader (`read_block`) and no copying sibling: it casts a block out of any buffer at any offset. Keep it that way — a multi-byte scalar stored as anything other than a LE byte array behind an accessor reintroduces the unaligned path and the per-condition copy the turner used to pay on every registry read.
- **The resolver names the executor.** A condition carries no executor program or discriminator; `ResolvedCrankV0` does, and the turner submits what came back. The turner tells the resolver which condition fired by appending a `FiredConditionV0` (target, block offset, index) to the resolver's instruction data — a resolver serving several conditions cannot work without it. That identity is an argument, not a capability: resolvers must validate it against the accounts they hold (demo-book's `resolve_v0` is the reference).
- `WatchV0.target_program` must stay at `WATCH_TARGET_PROGRAM_OFFSET` (8) and must keep being read from the target account's owner, never from instruction args — turners memcmp-filter the registry on it, and a forgeable value would let anyone bypass an operator's allowlist.
- Resolvers stage their payload in a writable account and return a pointer; they must never rely on raw return data for the payload (1024-byte cap). Staging is simulation-only — a resolver that a program lets *land* would commit scratch bytes, which is harmless but pointless.
- Condition blocks conventionally sit at an **8-aligned** account-data offset and `demo-book` compile-asserts its own, but reads no longer require it: alignment 1 means any offset casts. Do not remove a host's assertion to "fix" a layout — the convention keeps host structs tidy — but do not add alignment plumbing for the spec's sake either.
- Spec constants (`BEGIN_GUARD_V0_DISCRIMINATOR`, `ASSERT_PAID_V0_DISCRIMINATOR`, `WATCH_V0_DISCRIMINATOR`, `GUARD_SEED`) are pinned copies of program-generated values; `programs/relay/tests/relay_tests.rs` asserts they match. If you change an instruction/account name, update the spec constant AND the test.
- Wake hints must be conservative: a program may let a hint fire early (costs a simulation) but never late (liveness bug). The demo's `next_expiry_ts` min-over-inserts + executor-repair pattern is the reference.
- The binding safety check is `Turner::signer_leak`, run inside `sign_for_submission` on the exact list about to be signed. The earlier check in `try_crank` exists only for a clean skip reason. If you add a submission path, route it through `sign_for_submission`, never `signed_tx` directly. Its one exemption is keyed on the *instruction* (`is_own_guard`: relay's program id AND a guard discriminator), never on the program id alone — a resolver names the executor, so it may name relay, and relay's own `begin_guard_v0` spends the fee payer's lamports on rent.
- **Signer status is transaction-global.** A compiled message has no per-instruction signer flag: `is_signer` comes from the account's position in the message's signer section, so `AccountMeta { is_signer: false }` demotes nothing for an account that signs elsewhere in the transaction. Executors naming the *payout* is expected and allowed; naming a *signer*, or asking for one, is what is refused. `hostile_drain_succeeds_with_is_signer_false` proves the flag is not a defense — read it before touching any of this. The payout account must therefore be separate from the fee payer, and untrusted executors must never name the fee payer (`names_signer` enforces this). Never relax either without understanding that an executor can CPI a System transfer with any account that signs.
- Trusted programs (`trusted_programs`) skip guards and payout separation. That list is a loaded gun: only programs the operator controls belong on it.
- **Never reintroduce a CPI wrapper around executors.** Relay asserts around the call (guards), it does not mediate the call: a wrapper would consume one of the four CPI levels the executor's own stack needs (velocity → CLOB is already two deep) and add per-invoke cost. If a guard needs more context, extend the guard instructions, not the call path.
- Program keypairs live OUTSIDE the repo (`~/.config/solana/velocity-keys/relay.json`, `relay-demo-book.json`); only `declare_id!` pubkeys are committed. `.gitignore` blocks `**/*keypair*.json`.

## Test layout mirrors

`crank-turner/tests/*` hand-pin demo-book's offsets and sizes (`BOOK_ACCOUNT_LEN`, `CONDITIONS_OFFSET`, `ENTRY_COUNT_OFFSET`, `STAGING_OFFSET`, ...) so the turner crates never depend on the anchor-v2 tree. **Any change to `BookV0` breaks them**, usually as `InvalidInstructionData` or a nonsense assertion rather than a clean failure — re-read `programs/demo-book/src/state.rs` and update both test files. The e2e test guards itself with a per-side/total consistency check for exactly this reason. One further dependency runs the other way: the e2e polls for a state to *change* (`entry_count` falling), and a quote that never rested satisfies those polls as well as one that was swept. What keeps them honest is that every post confirms on chain, which is only equivalent because `add_entry_v0` appends and never fills. If demo-book gains a matching path on insert, landing and resting stop being one event and those polls need an explicit precondition instead.

## Commitment and forks

Reads, subscriptions, and simulation run at `processed`; the blockhash and signature outcomes require `confirmed`. Do not "fix" the first by raising it — stale state is what makes a keeper's simulation wrong — and do not lower the second two: a `processed` blockhash can be abandoned, and a `processed` signature status is not yet an outcome.

`processed` means a cached write can be taken back with **no correcting notification**, because the canonical chain never writes that account. Fork detection (slot subscriptions → `SlotUpdate` → `CachedSource::drain_slots`) is the only thing covering that, so treat it as load-bearing. Two traps, both pinned by tests: only the *processed* slot status may move the fork tip (confirmed and finalized repeat slots already passed, and would read as a switch every slot, throwing the cache away continuously); and ordinary skipped slots are not switches, so the predicate must key on the parent being below the tip rather than on gaps. There is no way to make a single-node test validator fork, so the state machine is pinned by unit tests in `cached.rs`, and the e2e only asserts the subscription is live and never fires.

## Metrics

Metric names and label values are an API: `grafana/relay-dashboard.json` and the alerts in `grafana/README.md` consume them, and a rename that compiles fine silently empties a panel — which nobody notices until they are staring at the dashboard during an incident. So the label strings are spelled out in `skip_label`, `wake_label`, `stage_label`, and `filter::reject_label` rather than derived from enum variant names, and `shipped_daemon_cranks_over_websocket` asserts the dashboard's series are present with their labels.

Cardinality is bounded deliberately. Programs are labelled by an 8-character prefix (`metrics::program_label`); conditions are never labelled, because a registry of 10,000 watches would become 30,000 series. Per-condition drilldown is the CLI's job — it reads the chain and has no cardinality budget. Do not add a pubkey-valued label.

One asymmetry worth keeping: skips are counted in *both* the decide phase and the crank path, because the vast majority happen in decide (not due, backoff) and were previously invisible — `relay_cranks_total{outcome="skipped"}` only ever saw the handful that got as far as a simulation.

## The CLI

`cli/` is a presentation layer over `Turner::explain`, and it must stay one. The value of the tool is that its verdicts come from the daemon's own `decide` and crank path, so a reimplementation of any gate — however small — is a bug even when it agrees today. `explain` stops at a prepared transaction (submission lives in `submit_packs`), which is what makes it read-only; `send_explained` submits exactly what was shown rather than re-deriving it.

Three things to know before changing it. A fresh process has no tick history, so change-wakes always read as due and `Backoff`/`ContentionDelay` never fire — say so in output rather than implying the condition is clear. A watch rejected at refresh is absent from `Turner::watches()` entirely, so any command that answers "why isn't this cranking" must consult `RefreshSummary::rejected` first or it will report "not registered" about something plainly on chain. And `LocalSimConfig::synthetic_fee_payer_lamports` exists only for read-only inspection, where the caller holds no key; it must stay off in the turner, where a keeper that has run out of SOL has to fail loudly.

The CLI's own e2e (`scripts/cli-e2e.sh`) deliberately does not use demo-book — it registers a watch against a system-owned account, which is both cheaper and the exact shape of the unreadable-block failure. The one scenario that needs a real due condition lives in the turner's suite instead (`the_cli_explains_and_cranks_a_real_condition`), invoking the binary by path from the shared target dir; `scripts/e2e.sh` builds `relay-cli` first for that reason, and running that test directly with plain `cargo test` will silently use a stale binary.

## What the e2e is for

`scripts/e2e.sh` exists to catch what litesvm structurally cannot: real RPC limits and encodings, commitment lag, on-chain atomicity, and the daemon's own plumbing. It has already found five defects the unit suites passed clean on — an unchunked `getMultipleAccounts` (RPC caps at 100), a compute limit summed from probes that never saw the appended instruction, a duplicate submission from too-short post-send suppression, a websocket backoff that never reset after a working session, and — the worst — a clock served from cache with no freshness check, which froze every timestamp and slot wake the moment the feed died while the turner still looked healthy.

Two properties the fleet scenario pinned down, both worth knowing before running more than one turner. On-chain failures are counted as `relay_transactions_total{result="failed"}`, **not** `relay_crank_failures_total` — that one covers pre-submission stages only, so alerting on it alone would miss every reverted crank. And uncoordinated turners duplicate everything: two of them each crank every ready condition, so one of the pair always burns a fee for a revert. That is inherent to permissionless racing, not a bug, but it means fleet cost scales with turner count while revenue does not. Losing is at least self-limiting — the loser's next tick re-reads the target, finds the work done, and resolves to no-work rather than resubmitting. When adding turner behavior, ask whether it can only be wrong against a real cluster; if so it belongs here.

Known gaps, in rough priority: the gRPC transport is never executed (needs a geyser plugin in the validator); the submitter's resend / re-sign / `Expired` path never fires because blockhashes do not expire in a short test; nothing restarts the validator itself (only the websocket is severed, via the proxy in `daemon_survives_losing_its_subscription`).

Three scenarios are load-bearing enough to be worth naming. `a_losing_turner_delays_itself_and_recovers_when_the_rival_dies` covers the adaptive contention delay in both directions. Two points about it, both learned by getting them wrong: the delay only moves once the submitter's confirm pass observes a reverted transaction, so a test that stops sampling the instant the books go empty measures it before the losses are accounted for; and decay is driven by cranks *landing*, so recovery needs work to feed it — an idle registry leaves the delay frozen where contention put it, which is correct behaviour and not something to assert against. `daemon_handles_a_registry_larger_than_one_rpc_call` runs 120 books, past the 100-key ceiling on a single account read, and asserts nothing fails at either stage. `two_turners_share_one_registry_without_wedging` runs two independent daemons on one registry over two rounds of work: the second round is the assertion, since a turner that treated a lost race as fatal would never finish it. Both tests batch their setup against the 1232-byte packet limit (two books per transaction, twelve quotes per transaction); if you add a field to `BookV0` or `WatchV0`, those batch sizes are what breaks first.

## Turner invariants

- Simulation is **local** (`local_sim.rs`, an in-process LiteSVM lazy-fork). Do not add code paths that simulate over RPC; `--remote-sim` exists only as a cross-check. Accounts come cache-first, so keep `--watch-program` coverage in mind when adding account reads.
- Everything the turner reads goes through the freshness rule, **including the clock** — it is an account like any other, and serving it blind is what froze all time-based wakes when the feed died. Do not add a read path that bypasses `needs_revalidation`.
- Cache freshness is a **correctness** invariant, not a tuning knob: an account may only be served from cache without revalidation when a backend has published live `Coverage` for it *and* the feed is healthy. Never widen that (e.g. "trust anything we once fetched") — it feeds stale state into simulation. Backends must publish `Coverage::default()` the moment a session drops. The rule covers `get_program_accounts` too: the *membership* of a program's account set goes stale exactly like an account's contents (a watch registered after the warm start arrives over the subscription and nowhere else), so a warm start is good only for as long as coverage says it is.
- Packed transactions must keep each crank's `[begin_guard, executor, assert_paid]` triple **contiguous** — that is the only reason one guard account can serve a whole pack. A pack also holds **one target program**: a transaction has a single outcome, and the submitter books the payment, the burned fee, and the contention delay against one program, so a mixed pack misattributes all three.

- The turner builds **transaction v1** messages (SIMD-0385) and nothing else. Three things follow. The compute budget lives in `v1::TransactionConfig` on the message, not in `ComputeBudgetInstruction`s, so nothing is prepended to the instruction list and a guard triple keeps the position it was packed at. **A v1 field left unset is zero, not a default** — a path that signs a message without naming a compute limit and a loaded-accounts data size builds a transaction that cannot land, which is how the resolver probe and `send_explained` broke when the budget stopped being an instruction. And the priority fee is a total in lamports on the message, while `getRecentPrioritizationFees` reports micro-lamports per compute unit; `priority_fee_lamports` is the one conversion, shared with `required_payment` so the fee the turner bids and the fee it charges the crank cannot drift apart.
- Packing is bounded by **four** v1 caps, not by bytes alone: 4096 bytes, 64 addresses, 64 instructions, 12 signatures. `fits` checks all four because each is a sanitization failure — the transaction never executes, so it reports no error to act on.
- The cluster must have the `txv1` feature active, which means agave 4.2 or later. A validator without it rejects every transaction the turner signs.

- `tick()`'s concurrent phase must stay `&self`-only: decisions produce `StateUpdate`s that are applied afterwards. If you find yourself wanting a lock or a channel inside the crank path, the phase split is being violated.
- The submitter, not the turner, owns send/confirm/resend. The decision loop must never await a confirmation. Resends are for transactions that are unconfirmed but whose blockhash can *still* land (bounded by `max_resends`, idempotent because the bytes and therefore the signature are identical); past `last_valid_block_height` there is nothing to retry, so the transaction is retired as `Expired` and the turner re-cranks the condition. Do not conflate the two — resending a dead blockhash only delays the retry.
- litesvm's `latest_blockhash` in tests must NOT call `expire_blockhash` — it races concurrent signers and surfaces as spurious `BlockhashNotFound`.

## Git conventions

Never add Claude (or any AI assistant) as a `Co-Authored-By` on commits or PRs. No `🤖 Generated with …` footers.

# Code Style

### Instruction module layout

Preferred layout for instruction code: each instruction domain is a **folder** under `src/instructions/` with **one file per instruction** and a `mod.rs` that holds the domain-level doc comment and re-exports. Within each instruction file, the `#[derive(Accounts)]` context struct goes at the **top**, the handler below it. Use this pattern for new instruction domains and when an existing domain is being substantially reworked anyway.

**Constraints over in-handler validates — when trivial.** Account _identity_ checks belong on the accounts struct, not in the handler: PDA `seeds`/`bump` derivation (including deriving one account's seeds from another's loaded field, e.g. `seeds = [b"spot_market", perp_market.load()?.quote_spot_market_index.to_le_bytes().as_ref()]`), `has_one` for top-level pubkey fields (e.g. `has_one = oracle`), and `address =` locks. Only keep a check in the handler when it is genuinely non-trivial as a constraint: multi-account/stateful logic, math on loaded data, or a _data invariant_ rather than an account identity. Don't contort complex logic into constraint expressions just to move it.

### Function and module shape

**Size limits.** A function body stays around 60 lines or under, and takes six arguments or fewer.
These are not arbitrary: a body you cannot see at once hides its own control flow, and a long
argument list is almost always a context struct that has not been written yet. Exceeding either is
allowed, but say why in a comment at the definition.

**Carry context in a struct; put the validating on it.** When several steps need the same maps,
market, clock and seats, that is a type. Give it a constructor in the shape of `AccountMaps::new`
and make each step a method, so a step takes the context and its own few arguments and nothing
else. Prefer several small contexts over one wide one: a struct that accumulates every lifetime in
the call graph becomes its own obstacle. 

**Name a layer for what it governs, not for what it does to the data.** A chain that read
`fill_perp_order` to `fulfill_perp_order` to `route_and_settle_perp_fill` told a reader nothing:
three synonyms, and each layer had exactly one caller, so the layering carried no reuse either.
Those layers govern the order, the taker's risk limits, and liquidity, and they say so now. If two
functions in a chain could swap names without anybody noticing, the names are wrong.

**One subject per file.** A file is the right size when a reader who opens it finds one subject. A
module root holds the doc, the imports, the `mod` declarations, the re-exports, and only what
several subjects genuinely share. Re-export every public name the old file exported, so a split
changes no caller's imports.


### Verifying a refactor

A refactor that changes behaviour is a rewrite, and the unit test count is the proof it did not:
**the count must be identical before and after.**

- **`cargo check` and `cargo clippy` without `--all-targets` build only the lib.** They report clean
  over a test tree that does not compile. Always pass `--all-targets`.
- **Run each verification command on its own.** Chaining them has produced output that reported
  compile errors at line numbers which did not exist in the file, and has hidden a real `fmt`
  failure that was then reported as clean.
- **Diff the compiler's warning population against a clean `HEAD` worktree**, bucketed by
  `(level, file, message)` so line numbers do not matter. This has caught several real regressions
  that the tests did not, including a settle path reading the raw book instead of the clamped
  ladder a quoter was allocated against, and a shared maker-seat step using the strict position
  lookup where one caller needs the creating one.
- **Measure every function, not the one that improved.** A report that the inner pass reached 155
  lines was true and useless while its three siblings sat at 571, 196 and 331.
- **Verify a re-export by compiling it.** Generate a temporary module that imports every public
  item of the old file by path, compile it under the feature flavors that include the gated names
  and through the path `lib.rs` actually uses, then delete it. Reading the `pub use` list proves
  nothing.

### Refactoring against an audit

A split moves code, so it destroys the feature diff. An auditor reading `master..<branch>` for a
file that was split sees the file deleted and new files appear, with the branch's own changes
scattered inside them. Weigh that before restructuring a file the branch already changed: the cost
is not the new code, it is the diff that can no longer be read. Land readability work only on files that are majorly changed or created in the feature. For example, if you only slightly change liquidation, you should not break up the entire file into modules. But if you overhaul orders, you break it into modules.

### Rust style

Prefer declarative iterator chains (`map`/`filter`/`fold`/`try_fold`/`collect`) over imperative `for`/`while` loops wherever the two are performance-equivalent. Explicit loops are fine when they are genuinely better: hot paths where the imperative form saves real work, or indexed mutation across parallel structures that the borrow checker won't allow through closures. Also avoid redundant recomputation in loops — hoist or precompute values that don't change (or change predictably) across iterations.

### Doc comments

All modules have doc comments. When making feature or refactor changes, update any module-level doc comments that would be invalidated by the change.

**No plan codenames in comments.** A comment must never point at a design doc, plan, spec section,
work phase, or review round as its justification — not `S1`–`S7` / "the S5 rule", not "Phase 2",
not "the plan settles this", "per the sync log", "as the spec warns", "deferred to a later phase". A
reader has the code, not the plan; those labels expire the moment the doc is renamed, reorganized, or
merged, and they encode nothing a reader can act on. Write the reason itself instead:

```rust
// BAD:  the S5 rule applied to the taker flow
// GOOD: an unfilled place_and_take remainder rests on the book instead of
//       cancelling, so the taker keeps queue position at its limit price
```

Referring to a *named, stable artifact* is fine — a crate (`relay-spec`), a type
(`relay_spec::ConditionV0`), a module path, a durable doc that explains a whole subsystem
(`docs/alignment-and-native-offsets.md`) — because those are things the reader can go read and that
change with the code. `docs/propamm-plan.md` is where S1–S7 are *defined*; that document may use
them, code may not. Local step labels inside one function ("first pass … second pass") are fine too,
as long as they describe that function rather than a project timeline.

**Version new event structs.** An `#[event]`'s discriminator is derived from its struct name, so
adding a field to an existing record silently changes the payload under a discriminator consumers
already decode. New velocity events therefore end in `V0` (e.g. `ProtocolUserWithdrawRecordV0`), and
a field addition ships as `…V1` with its own discriminator rather than mutating the `V0` shape. The
records inherited from upstream Drift keep their unversioned names — don't rename those. Wire every
new event into the SDK's subscriber surface in the same change: `EventMap`, the `eventTypes` default
list, and the `VelocityEvent` union in `packages/sdk/src/events/types.ts`, plus the record's type
mirror in `packages/sdk/src/types.ts`. A type mirror without the `EventMap` entry compiles fine and
is simply never decoded.
