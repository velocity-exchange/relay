# Relay

Generic condition-cranking for Solana programs. Successor to [tuktuk](https://github.com/helium/tuktuk) with an inverted data model: instead of CPI-ing into a task queue, target programs embed **conditions** in their own account state. A generic crank turner watches them, discovers work by **simulation**, and gets paid by the executed instruction.

## Why not a task queue

- Nothing to keep in sync: conditions are program state, written/updated/removed by the same handlers that change the state they describe. No queue-vs-truth divergence, no A=>B=>A requeue problem, no schedule CPI.
- No predicate language: the on-chain instruction is already the authoritative predicate (it fails when there's no work). Conditions only carry a **wake hint** — when it's worth re-simulating. Hints must be conservative (fire early/extra, never late); a slow fallback timer backstops bad hints. Simulation is the filter; the chain is the check.
- No account-list problem: an ix's account set can depend on which items get plucked (e.g. every expired order's owner). So account resolution is also program code: a read-only **resolver** ix, simulated by the turner, returns the executor's account list + args in return data. On-chain twin of a `getRemainingAccounts` helper — compiled with the program, can't drift.

## Condition contract (spec crate)

A condition block lives at a fixed **8-aligned** offset in a target-program account, registered with the relay program as a `WatchV0`. The block is **zero-copy pod** (`#[repr(C)]`, bytemuck, no interior padding): `ConditionBlockHeaderV0` (16 bytes: `"RELAY-V0"` magic, version, count) followed by a fixed array of `ConditionV0` (280 bytes each):

```
ConditionV0 {
  min_payment: u64,          // lamports the executor must pay the keeper
  // wake inputs, flattened so updates are single field stores:
  wake_ts: i64,              // WakeKind::AtTimestamp
  wake_slots: u64,           // WakeKind::EverySlots
  wake_account/offset/len,   // WakeKind::OnAccountDirty (watched byte range)
  resolver_program + resolver_disc + resolver_accounts[4],
  executor_program + executor_disc,   // accounts/args come from the resolver
  num_resolver_accounts, wake_kind, active, _pad
}
```

- `AtTimestamp`: the program updates the literal in place (e.g. a min-over-inserts `next_expiry_ts`; the sweep executor recomputes the true min as it walks — self-repairing). With the pod layout that's `conditions[i].wake_ts = new_min`, nothing else.
- `OnAccountDirty`: fire when the named byte range changes. The watched account may be the condition account itself or any other (e.g. an oracle).
- The resolver returns `[work: u8][num_accounts: u8][data_len: u16][AccountRefV0; n][data]` (align-1) via return data. `KEEPER_PLACEHOLDER` entries are replaced with the turner's keeper. `data` becomes the executor's args after the discriminator.

Programs embed the block as typed fields (`cond_header: ConditionBlockHeaderV0, conditions: [ConditionV0; N]`) — see demo-book — or via `relay_spec::read_block / read_block_mut / write_block` over a byte region.

## Program (`programs/relay`)

Anchor v2 (anchor-next, same pinned rev as velocity's anchor-v2 workspace). Two jobs:

1. **Registry**: `register_watch_v0(target, offset)` / `close_watch_v0`. A `WatchV0` account is discovery metadata only — the turner scans the registry to find condition blocks. Registration is permissionless (garbage watches parse-fail and get ignored); the registrar can close.
2. **Payment assert**: `crank_v0(offset, condition_index, keeper_index, data)` wrapper — reads the condition in place from the target account, CPIs the executor (every account `is_signer: false`), asserts the keeper's lamports grew by ≥ `min_payment`. This makes sim-only payment verification trivial for the turner (sim success ⇒ payment ok) and armors the sim-to-land race. Executors must not require signers (crank ixs are permissionless by design).

Turners MAY submit executor ixs directly (unwrapped) — the wrapper is optional armor, not a toll booth. There is no payment escrow: executors pay keepers from their own program's funds (treasury PDA, the condition account itself, wherever) — the target program prices its own cranks.

## Crank turner (`crank-turner`)

Generic daemon, patterned on tuktuk-crank-turner. Data access goes through a `ChainSource` trait (get accounts, simulate, send, clock) so transports are pluggable: RPC polling ships first; geyser/websocket sources slot in behind the trait; tests drive the full loop against litesvm. Loop per condition:

```
wake hint due? → sim resolver → work? → build crank_v0(executor) tx with keeper injected
  → sim (success ⇒ pays ≥ min_payment) → send → backoff/dedup bookkeeping
```

Failure modes accepted by design: stale local view ⇒ tx lands-and-fails (tx fee, same exposure as any keeper), hint fires early ⇒ wasted sim (cheap). Hints firing late is the only design bug; conditions should include an `EverySlots` fallback when in doubt.

## What stays out

Discovery over unbounded account sets (finding liquidatable users) is not expressible as a resolver — resolvers enumerate from watched accounts; searching stays in bespoke bots. That boundary is intentional.

## Repo layout

- `spec/` — `relay-spec`: the wire types. Zero-copy pod; depends only on bytemuck.
- `programs/` — separate cargo workspace (anchor-v2 pinned rev, own lockfile + target): `relay` program, `demo-book` (reference target embedding conditions: timestamp sweep with self-repairing hint, dirty-wake threshold evict; hosts the cross-program tests).
- `crank-turner/` — root-workspace client crate (solana 3.x), litesvm tests drive the full loop against built `.so` fixtures.

Program keypairs are never committed; `declare_id!` pubkeys only.
