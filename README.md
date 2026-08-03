# Relay

Generic condition-cranking for Solana programs. Programs embed **conditions** — "when this is worth doing, run that instruction, and it pays this much" — directly in their own account state. A generic crank turner discovers them, finds work by **simulation**, and submits payment-asserted cranks. No task queue, no scheduling CPI, no bespoke keeper bot per program.

See [DESIGN.md](./DESIGN.md) for the full design and rationale.

## How it works

1. A target program lays out a zero-copy condition block (`relay-spec`) in one of its accounts and registers `(account, offset)` with the relay program as a `WatchV0`.
2. Each `ConditionV0` names a **wake hint** (timestamp / slot / watched-bytes-changed / value-crossed / every-N-slots), a **resolver** instruction, and a `min_payment`. It does *not* name an executor: which instruction to run is the resolver's answer, so one resolver can serve a whole family of like-instructions from one condition.
3. The turner evaluates wake hints against its account feed. When one is due it *simulates* the resolver — telling it which condition fired, appended to the instruction data — and the resolver stages the executor (program, discriminator, accounts, args) in one of its own accounts and returns a 10-byte pointer; the turner reads the payload out of post-simulation account state. Account resolution is program code, so it can't drift — and since the resolver is only simulated, the staging write never touches chain state.
4. The turner submits the executor **directly**, bracketed by relay's payment guards (`begin_guard_v0` … executor … `assert_paid_v0`), simulates (success ⇒ payment verified), and sends. Guards are assertions, not a CPI wrapper, so they cost no CPI depth — the executor keeps all four levels for its own call stack. `--no-guard` drops them entirely.

The on-chain instruction is always the authoritative predicate — a stale turner costs itself a simulation or a failed-tx fee, never a wrong crank.

## Layout

| Path | What |
|---|---|
| `spec/` | `relay-spec` — pod wire types (bytemuck only); embed this in your program |
| `relay-anchor/` | `relay-anchor` — the block as one typed field in an Anchor 1.0 account (`Deref`, `Pod`, IDL type). Own workspace; the only crate here that depends on anchor |
| `programs/relay/` | watch registry + payment guard instructions (Anchor v2) |
| `programs/demo-book/` | reference target: a two-sided book with three conditions — expiry sweep, soft-cap eviction, and crossing |
| `chain-source/` | `relay-chain-source` — pluggable chain access: subscription-fed account cache + in-process lazy-fork simulation behind one trait. Protocol-agnostic; consumable on its own |
| `crank-turner/` | the generic turner daemon + litesvm end-to-end tests |

## Build & test

```bash
./scripts/build-programs.sh    # SBF-build both programs (needs cargo-build-sbf, tools v1.52)
cargo test                     # spec + turner tests (root workspace)
cd programs && cargo test      # program tests (litesvm, cross-program)
cargo test --manifest-path relay-anchor/Cargo.toml   # the anchor 1.0 host wrapper
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

## Metrics

Prometheus on `--metrics-port` (default 9899), `/metrics` and `/health`. A
dashboard covering all of it is in [`grafana/`](./grafana), along with the
alerts worth having and why some obvious-looking ones are not worth having.

Four questions, and the series that answer them:

- **What is generating load** — `relay_evaluations_total{wake, program}` counts
  every condition looked at, whether or not anything came of it, so it says
  what is making the turner busy rather than what it accomplished. Split by
  wake kind because that is usually the answer: a tight `EverySlots` or a
  change-wake on a hot account costs a resolve simulation every time it fires.
- **Why work is not happening** — `relay_skips_total{reason, program}`.
  `not_due` and `backoff` are the healthy baseline; `executor_named_signer`,
  `parse_failed`, and `no_safe_payout` should be flat zero and mean something
  is wrong when they are not.
- **Where the time goes** — `relay_tick_seconds{phase}` for the whole loop,
  `relay_stage_seconds{stage, program}` per condition (so one expensive
  resolver is visible instead of averaged away), `chain_rpc_seconds{method}`
  for time spent waiting on a provider, and `relay_saturated_ticks_total` for
  when the limit is `--concurrency` rather than the chain.
- **Whether it pays** — `relay_lamports_total{direction}`,
  `relay_compute_units{stage}`, `relay_transactions_total{result}`, and
  `relay_contention_delay_slots{program}`.

Latency is split deliberately: `relay_wake_lag_seconds` is the turner's own
decision latency, `relay_confirm_seconds` is the cluster's. "We were slow to
decide" and "the cluster was slow to confirm" want different fixes.

Label cardinality is bounded on purpose — programs by an 8-character prefix,
conditions not at all. Per-condition drilldown is the `relay` CLI's job.

## Debugging (`relay` CLI)

`cli/` ships a second binary, `relay`, for when a condition is not being cranked and you think it should be. Every verdict it prints comes from `Turner::explain`, which runs the daemon's own `decide` and crank path rather than a reimplementation — so the CLI cannot disagree with production about whether a condition is due.

```
relay watch list [--rejected]        # what is registered, and what this config threw away
relay watch get <TARGET>             # one watch, conditions decoded
relay condition list [--due]         # every condition, with what the turner would do
relay condition explain <TARGET>     # walk every gate: why is this cranking, or not
relay condition run <TARGET> [--send]  # resolve + simulate; --send actually cranks
relay guard <PAYOUT>                 # guard state
relay clock                          # what timestamp and slot wakes compare against
relay doctor                         # one-shot sweep: registry, conditions, rejects
```

`explain` is the one to reach for. It prints the gates in the order the daemon applies them, the two numbers whose comparison decides a wake (`waiting for unix_ts 1785374956, chain reads clock 1785374955`), the verdict, and what to do about it. `--json` on any command for a runbook.

Two classes of reason need different tools, and the CLI is explicit about which is which. **On chain** — inactive, below min payment, wake not due, filtered out of the registry, resolver reports no work, simulation fails, executor asks for a signature — is all visible directly. **Per-process** — failure backoff, post-send suppression, adaptive contention delay, the rolling profitability window — lives in the running daemon, so pass `--metrics-url http://host:9899/metrics` and `explain` reports those too instead of pretending they do not exist.

Mirror the daemon's config flags (`--min-crank-payment`, `--target-program`, `--trusted-program`, `--payout-address`, `--no-guard`) when you run it, or you are debugging a different turner than the one deployed. The most common trap is a watch rejected at refresh — an unreadable condition block, owner drift, a program not on the allowlist — which makes it invisible to every other view while being plainly present on chain. `relay watch list --rejected` and `relay doctor` name those with the reason and the fix; asking about one directly reports "IS registered … but this turner rejected it" rather than "not found".

Every flag also reads from env (`RELAY_RPC_URL`, `RELAY_KEEPER_KEYPAIR`, `RELAY_TRANSPORT`, `RELAY_GRPC_ENDPOINT`, `RELAY_MIN_CRANK_PAYMENT`, `RELAY_TICK_MS`, ...). Subscriptions only replace account reads; simulation and submission always go over RPC.
