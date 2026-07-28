# Relay

Generic condition-cranking for Solana programs. Successor to [tuktuk](https://github.com/helium/tuktuk) with an inverted data model: instead of CPI-ing into a task queue, target programs embed **conditions** in their own account state. A generic crank turner watches them, discovers work by **simulation**, and gets paid by the executed instruction.

## Why not a task queue

- Nothing to keep in sync: conditions are program state, written/updated/removed by the same handlers that change the state they describe. No queue-vs-truth divergence, no A=>B=>A requeue problem, no schedule CPI.
- No predicate language: the on-chain instruction is already the authoritative predicate (it fails when there's no work). Conditions only carry a **wake hint** — when it's worth re-simulating. Hints must be conservative (fire early/extra, never late); a slow fallback timer backstops bad hints. Simulation is the filter; the chain is the check.
- No account-list problem: an ix's account set can depend on which items get plucked (e.g. every expired order's owner). So account resolution is also program code: a **resolver** ix, simulated by the turner, stages the executor's account list + args and returns a pointer to them. On-chain twin of a `getRemainingAccounts` helper — compiled with the program, can't drift.

## Condition contract (spec crate)

A condition block lives at a fixed **8-aligned** offset in a target-program account, registered with the relay program as a `WatchV0`. The block is **zero-copy pod** (`#[repr(C)]`, bytemuck, no interior padding): `ConditionBlockHeaderV0` (16 bytes: `"RELAY-V0"` magic, version, count) followed by a fixed array of `ConditionV0` (280 bytes each):

```
ConditionV0 {
  min_payment: u64,          // lamports the executor must pay the keeper
  // wake inputs, flattened so updates are single field stores:
  wake_ts: i64,              // WakeKind::AtTimestamp
  wake_slot: u64,            // WakeKind::EverySlots (interval) | AtSlot (absolute)
  wake_account/offset/len,   // WakeKind::OnAccountChange (watched byte range)
  resolver_program + resolver_disc + resolver_accounts[4],
  executor_program + executor_disc,   // accounts/args come from the resolver
  num_resolver_accounts, wake_kind, active, _pad
}
```

Wake kinds:

- `AtTimestamp` / `AtSlot`: due once the chain clock reaches `wake_ts` / the chain reaches `wake_slot`. The program updates the literal in place (e.g. a min-over-inserts `next_expiry_ts`; the sweep executor recomputes the true min as it walks — self-repairing). With the pod layout that's `conditions[i].wake_ts = new_min`, nothing else.
- `OnAccountChange`: fire when the named byte range changes. The watched account may be the condition account itself or any other (e.g. an oracle).
- `EverySlots`: poll every N slots — the fallback that backstops a bad hint.

### Resolver output: staging, not return data

Return data is capped at 1024 bytes, which would bound how many accounts and args a resolver can name — exactly the wrong thing to bound, since batch cranks grow with the work (sweep every expired order, each with its own owner). So the resolver **stages** `[num_accounts: u8][data_len: u16][AccountRefV0; n][data]` in one of its own writable accounts and returns a 10-byte `ResponsePointerV0 { work, account_index, offset, len }`; `account_index` indexes the condition's `resolver_accounts`. The turner reads the byte range out of the simulation's **post-execution account state** (`simulateTransaction`'s `accounts` config; litesvm's `post_accounts`).

Because resolvers are only ever simulated, the staging write never lands on chain: no state bloat, no rent churn, and no write contention between competing turners. A no-work result stages nothing at all. `KEEPER_PLACEHOLDER` entries in the staged account list are replaced with the turner's keeper; `data` becomes the executor's args after the discriminator.

Programs embed the block as typed fields (`cond_header: ConditionBlockHeaderV0, conditions: [ConditionV0; N]`) — see demo-book — or via `relay_spec::read_block / read_block_mut / write_block` over a byte region.

## Scoping a turner (`WatchFilter`)

The registry is permissionless, so a turner that tracked everything could be made to work for free: another protocol registers thousands of multi-megabyte targets paying a lamport a crank, and your turner fetches, subscribes to, parses, and simulates all of them. `WatchFilter` is how an operator scopes a turner to work it wants, cheapest check first:

| Stage | Rule | Cost |
|---|---|---|
| Server-side | `allowed_target_programs` → `getProgramAccounts` / geyser memcmp on `WatchV0.target_program` | non-matching watches are never transmitted |
| Registry-only | `blocked_target_programs`, `allowed_registrars`, `allowed_targets` | decided from the 112-byte watch account |
| Post-fetch | `max_target_bytes`, owner-drift check | one fetch per refresh |
| Post-parse | `min_crank_payment` | one parse per refresh |
| Last | `max_watches` | — |

`target_program` is recorded **by the relay program from the target account's owner** at registration, so a registrar can't claim someone else's program to slip past an allowlist. It leads the `WatchV0` layout precisely so it is memcmp-able.

The fee bar drops the *whole watch*, not just the underachieving condition: a book with nothing worth cranking stops being fetched and subscribed until the next refresh, rather than costing a fetch every tick forever. Everything here is resource policy, never correctness — `crank_v0`'s payment assert is what guarantees a turner actually gets paid.

## Program (`programs/relay`)

Anchor v2 (anchor-next, same pinned rev as velocity's anchor-v2 workspace). Two jobs:

1. **Registry**: `register_watch_v0(target, offset)` / `close_watch_v0`. A `WatchV0` is `[disc][target_program][target][registrar][offset]` — discovery metadata only. `target_program` is read from the target account, not from args. Registration is permissionless (garbage watches parse-fail and get dropped at refresh); the registrar can close and reclaim rent.
2. **Payment guards**: `begin_guard_v0` / `assert_paid_v0`, bracketing the executor:

```
[ begin_guard_v0 ]  [ the executor, directly ]  [ assert_paid_v0(min_payment) ]
   snapshot keeper                                  revert unless keeper
   lamports                                         gained >= min_payment
```

**Not a CPI.** An earlier design wrapped the executor in a `crank_v0` invoke; guards replace it because wrapping cost a CPI level the executor's own call stack needs (a velocity crank calling into the CLOB is already two deep, against a limit of four) plus per-invoke overhead and account re-serialization. Guards are two ~1k-CU instructions that touch two accounts. This is the [Lighthouse](https://github.com/Jac0xb/lighthouse) shape: assert on state around a call instead of mediating the call.

Reading the balance *inside* execution at both ends makes the pair fee-agnostic (the fee is already deducted when the first guard runs) and immune to concurrent balance changes — it measures this transaction's delta, not an absolute floor. The guard account is a PDA seeded `["guard", keeper, nonce]`, created on first use and reused forever; nothing in it survives a failed transaction, and a successful one disarms it, so it is pure scratch that happens to need an address. The nonce lets one keeper run concurrent cranks without serializing on a single write lock.

`min_payment` is the **turner's** price, passed in the guard's args — not read from the condition on chain. A turner asserts what it is willing to work for; the target's advertised number is only an input to that decision.

Guards are optional (`--no-guard`): a turner that trusts its simulation can submit the bare executor, and relay is then not involved in execution at all. There is no payment escrow either — executors pay keepers from their own program's funds (treasury PDA, the condition account itself, wherever), so the target program prices its own cranks.

Note this means relay no longer validates conditions at execution time. That costs nothing: `active`, and every other field of a condition, was always advisory — executors are permissionless, so anyone could always call one directly, guard or no guard.

## Crank turner (`crank-turner`)

Generic daemon, patterned on tuktuk-crank-turner. Data access goes through a `ChainSource` trait (get accounts, simulate, send, clock), and the transports mirror the velocity keeper stack:

| Transport | What it does | Velocity equivalent |
|---|---|---|
| `rpc` | polls RPC for every read | the floor; no extra infra |
| `ws` | `programSubscribe` (watch registry) + `accountSubscribe` (targets, watched accounts, clock) | keeper-bots-v2 websocket mode, velocity-rs ws mode |
| `grpc` | Yellowstone/geyser: one stream, owner filter for watches + pubkey filter for the interest set, filter updates pushed on the live stream | keep-rs (`GRPC_ENDPOINT`/`GRPC_X_TOKEN`) |

### Operational shape

The turner is three cooperating pieces, following tuktuk's crank turner where it earned it:

- **Decision loop** (`turner.rs`) — each tick *decides* which wakes are due (sequential, no I/O), cranks the due ones **concurrently** (`--concurrency`, default 8; every crank is several RPC round trips, so this is the throughput knob), then folds the bookkeeping back in. Splitting it that way means the concurrent phase borrows nothing mutable: no locks, no channels between cranks.
- **Submitter** (`submit.rs`) — a channel-fed subsystem that owns send/confirm/resend so the decision loop never blocks on the cluster. One shared blockhash refreshed on a timer and published over a `watch` channel (instead of a `getLatestBlockhash` per transaction); unconfirmed signatures tracked and polled in batches; resend while the blockhash lives, then a distinct `Expired` outcome so the caller retries promptly instead of counting it as the condition's fault. The turner signs, so it knows the signature immediately and returns without waiting.
- **Local simulation** (`local_sim.rs`) — every resolver and executor simulation runs in an in-process `LiteSVM`, not at the RPC provider. A transaction declares its whole account set upfront, so a *lazy fork* suffices: collect the transaction's account keys, populate them from the cache (falling back to the inner source only for genuinely cold ones), sync the clock, execute. Pooled banks retain their loaded program ELFs, which is the expensive part. `--watch-program <ID>` streams **every** account owned by a program into the cache, so a turner cranking its own protocol names that program and then almost never fetches. This is what makes loose wake hints and frequent `EverySlots` fallbacks affordable — a no-work resolve costs microseconds and nothing. `--remote-sim` reverts to provider simulation for cross-checking.
- **Packing** — verified cranks share a transaction (`--max-cranks-per-tx`, default 3), splitting one signature fee. Guard triples stay contiguous, which is what makes reusing one guard account within a transaction safe: each `begin_guard` re-arms before its own executor and its own `assert_paid` consumes it. Size is checked by serializing, not estimating; a pack that fails to simulate falls back to individual submission, which is cheap because every member already simulated alone. Compute-unit limits come from the simulation (fees are billed on the limit you *request*), and the priority fee is sampled by the submitter.
- **Profitability + metrics** — the submitter books each outcome into a rolling per-program net-lamports window, published over another `watch` channel; `--min-program-profit` skips programs that keep losing money rather than retrying them forever. Prometheus at `/metrics`, liveness at `/health`. Two labels are there for otherwise-invisible failures: `relay_update_source` (subscription vs repoll — a dead stream shows up as the poll counter climbing alone) and `relay_wake_lag_seconds` (how late cranks are, i.e. whether the turner is oversubscribed).

`ws` and `grpc` are `CachedSource<RpcSource>`: the subscription feeds an account cache, and misses (plus a periodic `repoll_every` refetch, the tuktuk dual ws+poll insurance) fall through to RPC. Subscriptions only replace *reads* — simulation and submission always go to RPC. Loop per condition:

```
wake hint due? → sim resolver → work? → read staged payload from post-sim account state
  → build [begin_guard, executor, assert_paid] with the keeper injected
  → sim (success ⇒ pays ≥ min_payment) → send → backoff/dedup bookkeeping
```

Failure modes accepted by design: stale local view ⇒ tx lands-and-fails (tx fee, same exposure as any keeper), hint fires early ⇒ wasted sim (cheap). Hints firing late is the only design bug; conditions should include an `EverySlots` fallback when in doubt.

## What stays out

Discovery over unbounded account sets (finding liquidatable users) is not expressible as a resolver — resolvers enumerate from watched accounts; searching stays in bespoke bots. That boundary is intentional.

## Repo layout

- `spec/` — `relay-spec`: the wire types. Zero-copy pod; depends only on bytemuck.
- `programs/` — separate cargo workspace (anchor-v2 pinned rev, own lockfile + target): `relay` program, `demo-book` (reference target embedding conditions: timestamp sweep with self-repairing hint, change-wake threshold evict; hosts the cross-program tests).
- `crank-turner/` — root-workspace client crate (solana 3.x), litesvm tests drive the full loop against built `.so` fixtures.

Program keypairs are never committed; `declare_id!` pubkeys only.
