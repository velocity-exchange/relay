# Relay

Generic condition-cranking for Solana programs. Programs embed **conditions** — "when this is worth doing, run that instruction, and it pays this much" — directly in their own account state. A generic crank turner discovers them, finds work by **simulation**, and submits payment-asserted cranks. No task queue, no scheduling CPI, no bespoke keeper bot per program.

See [DESIGN.md](./DESIGN.md) for the full design and rationale.

## How it works

1. A target program lays out a zero-copy condition block (`relay-spec`) in one of its accounts and registers `(account, offset)` with the relay program as a `WatchV0`.
2. Each `ConditionV0` names a **wake hint** (timestamp / watched-bytes-dirty / every-N-slots), a read-only **resolver** instruction, an **executor** instruction, and a `min_payment`.
3. The turner evaluates wake hints against its account feed. When one is due it *simulates* the resolver, which returns the executor's account list + args in return data (account resolution is program code — it can't drift).
4. The turner wraps the executor in relay's `crank_v0`, which asserts the keeper got paid `min_payment`, simulates (success ⇒ payment verified), and sends.

The on-chain instruction is always the authoritative predicate — a stale turner costs itself a simulation or a failed-tx fee, never a wrong crank.

## Layout

| Path | What |
|---|---|
| `spec/` | `relay-spec` — pod wire types (bytemuck only); embed this in your program |
| `programs/relay/` | watch registry + `crank_v0` payment-assert wrapper (Anchor v2) |
| `programs/demo-book/` | reference target: expiring entries swept for a reward, soft-cap eviction |
| `crank-turner/` | the generic turner daemon + litesvm end-to-end tests |

## Build & test

```bash
./scripts/build-programs.sh    # SBF-build both programs (needs cargo-build-sbf, tools v1.52)
cargo test                     # spec + turner tests (root workspace)
cd programs && cargo test      # program tests (litesvm, cross-program)
```

## Run the turner

```bash
cargo run -p relay-crank-turner -- \
  --rpc-url https://your-rpc \
  --keypair ~/keeper.json \
  --min-crank-payment 5000
```

Flags also read from env (`RELAY_RPC_URL`, `RELAY_KEEPER_KEYPAIR`, `RELAY_MIN_CRANK_PAYMENT`, `RELAY_TICK_MS`, ...). The RPC-polling `ChainSource` is the reference transport; geyser/websocket sources implement the same trait.
