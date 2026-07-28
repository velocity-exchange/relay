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
2. **Payment assert**: `crank_v0(offset, condition_index, keeper_index, data)` wrapper — reads the condition in place from the target account, CPIs the executor (every account `is_signer: false`), asserts the keeper's lamports grew by ≥ `min_payment`. This makes sim-only payment verification trivial for the turner (sim success ⇒ payment ok) and armors the sim-to-land race. Executors must not require signers (crank ixs are permissionless by design).

Turners MAY submit executor ixs directly (unwrapped) — the wrapper is optional armor, not a toll booth. There is no payment escrow: executors pay keepers from their own program's funds (treasury PDA, the condition account itself, wherever) — the target program prices its own cranks.

## Crank turner (`crank-turner`)

Generic daemon, patterned on tuktuk-crank-turner. Data access goes through a `ChainSource` trait (get accounts, simulate, send, clock), and the transports mirror the velocity keeper stack:

| Transport | What it does | Velocity equivalent |
|---|---|---|
| `rpc` | polls RPC for every read | the floor; no extra infra |
| `ws` | `programSubscribe` (watch registry) + `accountSubscribe` (targets, watched accounts, clock) | keeper-bots-v2 websocket mode, velocity-rs ws mode |
| `grpc` | Yellowstone/geyser: one stream, owner filter for watches + pubkey filter for the interest set, filter updates pushed on the live stream | keep-rs (`GRPC_ENDPOINT`/`GRPC_X_TOKEN`) |

`ws` and `grpc` are `CachedSource<RpcSource>`: the subscription feeds an account cache, and misses (plus a periodic `repoll_every` refetch, the tuktuk dual ws+poll insurance) fall through to RPC. Subscriptions only replace *reads* — simulation and submission always go to RPC. Loop per condition:

```
wake hint due? → sim resolver → work? → read staged payload from post-sim account state
  → build crank_v0(executor) tx with keeper injected
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
