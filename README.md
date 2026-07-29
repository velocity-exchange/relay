# Relay

Generic condition-cranking for Solana programs. Programs embed **conditions** — "when this is worth doing, run that instruction, and it pays this much" — directly in their own account state. A generic crank turner discovers them, finds work by **simulation**, and submits payment-asserted cranks. No task queue, no scheduling CPI, no bespoke keeper bot per program.

See [DESIGN.md](./DESIGN.md) for the full design and rationale.

## How it works

1. A target program lays out a zero-copy condition block (`relay-spec`) in one of its accounts and registers `(account, offset)` with the relay program as a `WatchV0`.
2. Each `ConditionV0` names a **wake hint** (timestamp / slot / watched-bytes-changed / every-N-slots), a **resolver** instruction, an **executor** instruction, and a `min_payment`.
3. The turner evaluates wake hints against its account feed. When one is due it *simulates* the resolver, which stages the executor's account list + args in one of its own accounts and returns a 10-byte pointer; the turner reads the payload out of post-simulation account state. Account resolution is program code, so it can't drift — and since the resolver is only simulated, the staging write never touches chain state.
4. The turner submits the executor **directly**, bracketed by relay's payment guards (`begin_guard_v0` … executor … `assert_paid_v0`), simulates (success ⇒ payment verified), and sends. Guards are assertions, not a CPI wrapper, so they cost no CPI depth — the executor keeps all four levels for its own call stack. `--no-guard` drops them entirely.

The on-chain instruction is always the authoritative predicate — a stale turner costs itself a simulation or a failed-tx fee, never a wrong crank.

## Layout

| Path | What |
|---|---|
| `spec/` | `relay-spec` — pod wire types (bytemuck only); embed this in your program |
| `programs/relay/` | watch registry + payment guard instructions (Anchor v2) |
| `programs/demo-book/` | reference target: a two-sided book with three conditions — expiry sweep, soft-cap eviction, and crossing |
| `crank-turner/` | the generic turner daemon + litesvm end-to-end tests |

## Build & test

```bash
./scripts/build-programs.sh    # SBF-build both programs (needs cargo-build-sbf, tools v1.52)
cargo test                     # spec + turner tests (root workspace)
cd programs && cargo test      # program tests (litesvm, cross-program)
./scripts/e2e.sh               # end-to-end against a real solana-test-validator
```

`e2e.sh` deploys both programs to a throwaway validator, runs a turner, posts orders from a bot, and checks that expiry, eviction, and crossing all get cranked on a chain that is genuinely moving.

## Run the turner

Scope a turner to your own programs so another protocol's watches cost you nothing (the allowlist is pushed down to the RPC/geyser provider, so they are never even transmitted):

```bash
--target-program <YOUR_PROGRAM>      # repeatable/comma-separated; default: everything
--min-crank-payment 5000             # drops watches with nothing worth cranking
--max-target-bytes 100000            # skip expensive-to-stream targets
--max-watches 500                    # hard ceiling
```

```bash
# RPC polling (no extra infra)
cargo run -p relay-crank-turner -- --rpc-url https://your-rpc --keypair ~/keeper.json

# websocket programSubscribe/accountSubscribe
cargo run -p relay-crank-turner -- --rpc-url https://your-rpc --keypair ~/keeper.json \
  --transport ws                        # --ws-url defaults from --rpc-url

# Yellowstone/geyser gRPC
cargo run -p relay-crank-turner -- --rpc-url https://your-rpc --keypair ~/keeper.json \
  --transport grpc --grpc-endpoint https://your-geyser --grpc-x-token "$TOKEN"
```

All simulation runs locally in an in-process SVM — pass `--watch-program <YOUR_PROGRAM>` so the accounts it needs are streamed into the cache and simulation stops touching the network (`--remote-sim` opts out).

Accounts are only served from cache without revalidation when a subscription actually covers them and the feed is alive; anything uncovered is refetched within `--max-age-uncovered-ms` so simulation never runs on stale state.

The natural payout is a **wrapped-SOL account** owned by your keeper: the SPL Token program owns it, so only the keeper's signature can move anything out, while any program can credit it. Add `--sync-native-payout` and the turner periodically rolls the accumulated lamports into spendable wSOL (cranks themselves never carry a `sync_native`, since the guard measures lamports).

Safety flags: `--payout-address <PUBKEY>` is where executors pay, and **must not be the fee payer** — signer status is transaction-global, so an untrusted executor handed the signing key could drain it. Untrusted programs are skipped without one. `--trusted-program <ID>` marks a program you wrote: its cranks skip the payment guards and may be paid to the fee payer directly, saving two instructions and their compute.

Operational flags: `--concurrency` (cranks in flight per tick), `--min-program-profit` (skip programs whose recent cranks lost money), `--contention-step-slots` / `--max-contention-slots` (hold a program's cranks back when its transactions keep reverting, so races lost to a faster turner cost nothing instead of a fee each; decays back to zero as cranks land again — `0` disables), `--metrics-port` (Prometheus `/metrics` + `/health`, default 9899), `--max-cranks-per-tx` (pack cranks into shared transactions, default 3), `--max-priority-fee`.

Every flag also reads from env (`RELAY_RPC_URL`, `RELAY_KEEPER_KEYPAIR`, `RELAY_TRANSPORT`, `RELAY_GRPC_ENDPOINT`, `RELAY_MIN_CRANK_PAYMENT`, `RELAY_TICK_MS`, ...). Subscriptions only replace account reads; simulation and submission always go over RPC.
