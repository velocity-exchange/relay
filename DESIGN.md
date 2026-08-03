# Relay

Generic condition-cranking for Solana programs. Successor to [tuktuk](https://github.com/helium/tuktuk) with an inverted data model: instead of CPI-ing into a task queue, target programs embed **conditions** in their own account state. A generic crank turner watches them, discovers work by **simulation**, and gets paid by the executed instruction.

## Why not a task queue

- Nothing to keep in sync: conditions are program state, written/updated/removed by the same handlers that change the state they describe. No queue-vs-truth divergence, no A=>B=>A requeue problem, no schedule CPI.
- No predicate language: the on-chain instruction is already the authoritative predicate (it fails when there's no work). Conditions only carry a **wake hint** — when it's worth re-simulating. Hints must be conservative (fire early/extra, never late); a slow fallback timer backstops bad hints. Simulation is the filter; the chain is the check.
- No account-list problem: an ix's account set can depend on which items get plucked (e.g. every expired order's owner). So account resolution is also program code: a **resolver** ix, simulated by the turner, stages the executor's account list + args and returns a pointer to them. On-chain twin of a `getRemainingAccounts` helper — compiled with the program, can't drift.

## Condition contract (spec crate)

A condition block lives at a fixed offset in a target-program account, registered with the relay program as a `WatchV0`. The block is **zero-copy pod** (`#[repr(C)]`, bytemuck, no interior padding): `ConditionBlockHeaderV0` (16 bytes: `"RELAY-V0"` magic, version, count) followed by a fixed array of `ConditionV0` (192 bytes each):

```
ConditionV0 {
  min_payment: u64,          // lamports the executor must pay the keeper
  // wake inputs, flattened so updates are single field stores:
  wake_ts: i64,              // WakeKind::AtTimestamp | OnValueCross threshold
  wake_slot: u64,            // WakeKind::EverySlots (interval) | AtSlot (absolute)
  wake_account/offset/len,   // watched byte range (OnAccountChange, OnValueCross)
  resolver_program + resolver_disc,   // the executor is the resolver's answer
  resolver_list_offset + num_resolver_accounts,
  wake_kind, active, wake_cmp, wake_value_unsigned, _reserved
}
```

Every field is stored as a little-endian byte array behind an accessor, so the whole evaluation path is **alignment 1**. That is what makes one reader enough: `read_block` casts a block in place out of any buffer at any offset — a program's own account data, a turner's RPC response — with no copy and no second, copying code path to keep in step. (Hosts still put blocks at 8-aligned offsets by convention, because it keeps their own structs tidy; nothing in the read path needs it.)

Wake kinds:

- `AtTimestamp` / `AtSlot`: due once the chain clock reaches `wake_ts` / the chain reaches `wake_slot`. The program updates the literal in place (e.g. a min-over-inserts `next_expiry_ts`; the sweep executor recomputes the true min as it walks — self-repairing). With the pod layout that's `conditions[i].wake_ts = new_min`, nothing else.
- `OnAccountChange`: fire when the named byte range changes. The watched account may be the condition account itself or any other (e.g. an oracle).
- `EverySlots`: poll every N slots — the fallback that backstops a bad hint.

### Trust model

A turner runs instructions chosen by target programs it may not control, so two rules bound what a hostile one can do.

This is not theoretical: `hostile_drain_succeeds_with_is_signer_false` builds a System transfer that marks the fee payer `is_signer: false` and drains it anyway, and asserts that it **works**. That test is the justification for everything below — if a future runtime ever honoured per-instruction signer flags it would start failing, and these rules could be relaxed.

**Use a wrapped-SOL account as the payout.** The natural choice, and the one that removes the operational burden: a wSOL token account owned by the keeper. The SPL Token program owns the account, so only *it* can debit the lamports, and it only does so for `transfer` / `close_account` / `set_authority` — every one of which requires the token account's authority (the keeper) to sign. The keeper is never in an executor's account list, so a hostile executor can credit that account and do nothing else. Meanwhile any program may credit lamports to any account, so payment works unchanged.

Payment therefore lands as raw lamports, which is exactly what the guard measures — nothing about correctness waits on the token `amount` field. `sync_native` (SPL Token instruction 17, which takes **no signer at all** and sets `amount = lamports - rent_exempt_reserve`) is what makes the proceeds spendable as wSOL, and the turner runs it **on a timer** rather than per crank: bundling it into every transaction would buy an instruction, its compute, and its bytes to keep a number fresh that nobody reads in between. `--sync-native-payout` enables it, `--sync-every-ticks` paces it, and `Turner::sync_payout` is the call.

**The payout must never sign.** Signer status on Solana is transaction-global: an account that signs the message is a signer inside *every* instruction of it, whatever the per-instruction `AccountMeta` says. A malicious executor handed the fee payer would therefore see `is_signer: true` and could CPI a System transfer to drain it — marking the meta `is_signer: false` is necessary but nowhere near sufficient. So `--payout-address` names a separate account that receives payment and never signs; the fee payer signs and is never handed to an executor. On top of that the turner **searches the finished instruction list** and refuses to sign a transaction where any instruction that is not relay's own, and not trusted, either **asks for a signature** (any `is_signer: true` meta — nothing legitimate needs one, since executors are permissionless) or **names one of the transaction's signers** (`Turner::signer_leak`, reading the signer set off the compiled message rather than assuming it). The second is the one that actually bites, since the meta flag is not honoured per instruction. Note what this does *not* forbid: an executor must name the account it pays, and that is fine — a non-signing account can only be credited. The rule is "don't name a signer", not "don't name the payee", and the separate payout is what makes those two different accounts. That is the binding rule, and it deliberately lives at signing time rather than at build time: instructions get rebuilt, re-priced, and concatenated into packs afterwards, so checking the list about to be signed is the only version no later transformation can bypass. A resolver that slips the fee payer in directly instead of through `KEEPER_PLACEHOLDER` is caught the same way. Without a payout configured, untrusted programs are skipped entirely rather than risked.

**Trusted programs opt out of all of it.** `--trusted-program <ID>` says "I wrote this, I don't need a condom": its executors run with no guard instructions (two fewer instructions, their compute, ~100 bytes) and may be paid straight to the fee payer. Only list programs you control — the guards and the payout separation are the only things standing between a turner and a malicious executor. And note that the program deciding this is the one the *resolver* named, which is not authenticated (the literal a condition used to carry never was either): any target program can have its cranks run against a listed program by naming it, so the bar is "I am happy for anyone to invoke its permissionless surface with my fee payer in the account list", not merely "I wrote it".

### Resolver output: staging, not return data

Return data is capped at 1024 bytes, which would bound how many accounts and args a resolver can name — exactly the wrong thing to bound, since batch cranks grow with the work (sweep every expired order, each with its own owner). So the resolver **stages** `[num_accounts: u8][data_len: u16][AccountRefV0; n][data]` in one of its own writable accounts and returns a 10-byte `ResponsePointerV0 { work, account_index, offset, len }`; `account_index` indexes the condition's `resolver_accounts`. The turner reads the byte range out of the simulation's **post-execution account state** (`simulateTransaction`'s `accounts` config; litesvm's `post_accounts`).

Because resolvers are only ever simulated, the staging write never lands on chain: no state bloat, no rent churn, and no write contention between competing turners. A no-work result stages nothing at all. `KEEPER_PLACEHOLDER` entries in the staged account list are replaced with the turner's keeper; `data` becomes the executor's args after the discriminator.

### The resolver names the executor

The staged payload leads with the executor's **program and discriminator**, and a condition carries neither. A condition used to name the instruction to run as a literal, which meant one slot could only ever run one instruction: a program with a family of like-instructions — settle this kind of position, sweep that kind of order — needed a condition, and a wake, for each. Returning the identity costs 40 staged bytes and adds no trust, because the resolver already decides the accounts and the args: a turner willing to run what a resolver picked out is willing to run *which* instruction it picked, and the guards bound the damage identically either way. demo-book is the reference — its three conditions share one `resolve_v0`.

That only works if the resolver knows which condition it is answering for, so the turner appends a `FiredConditionV0` — target account, block offset, condition index — to the resolver's instruction data after the discriminator (45 bytes total). Instruction data is the cheapest faithful channel: 37 bytes, no extra account, no extra read, and nothing new for the turner to track, since the identity is exactly the `WatchV0` coordinates plus the slot index. The alternatives are worse in kind, not degree — an extra account is a 32-byte key plus a load to carry 5 bytes of context, and a discriminator per condition is the duplication this removes.

The identity is not authenticated and does not need to be: it names the resolver's *own* state, which the resolver re-reads from the accounts it was handed. So a resolver validates it like any argument — is this the account I hold, is this an index I serve — rather than trusting it. A wrong identity can only produce an answer about a condition that is not due, which costs a simulation.

Programs embed the block as typed fields (`cond_header: ConditionBlockHeaderV0, conditions: [ConditionV0; N]`) — see demo-book — or via `relay_spec::read_block / read_block_mut / write_block` over a byte region.

## Scoping a turner (`WatchFilter`)

The registry is permissionless, so a turner that tracked everything could be made to work for free: another protocol registers thousands of multi-megabyte targets paying a lamport a crank, and your turner fetches, subscribes to, parses, and simulates all of them. `WatchFilter` is how an operator scopes a turner to work it wants, cheapest check first:

| Stage | Rule | Cost |
|---|---|---|
| Server-side | `allowed_target_programs` → `getProgramAccounts` / geyser memcmp on `WatchV0.target_program` | non-matching watches are never transmitted |
| Registry-only | `blocked_target_programs`, `allowed_creators`, `allowed_targets` | decided from the 112-byte watch account |
| Post-fetch | `max_target_bytes`, owner-drift check | one fetch per refresh |
| Post-parse | `min_crank_payment` | one parse per refresh |
| Last | `max_watches` | — |

`target_program` is recorded **by the relay program from the target account's owner** at registration, so a watch's creator can't claim someone else's program to slip past an allowlist. It leads the `WatchV0` layout precisely so it is memcmp-able.

The fee bar drops the *whole watch*, not just the underachieving condition: a book with nothing worth cranking stops being fetched and subscribed until the next refresh, rather than costing a fetch every tick forever. Everything here is resource policy, never correctness — `crank_v0`'s payment assert is what guarantees a turner actually gets paid.

## Program (`programs/relay`)

Anchor v2 (anchor-next, same pinned rev as velocity's anchor-v2 workspace). Two jobs:

1. **Registry**: `register_watch_v0(target, offset)` / `close_watch_v0`. A `WatchV0` is `[disc][target_program][target][creator][offset]` — discovery metadata only. `target_program` is read from the target account, not from args. Registration is permissionless (garbage watches parse-fail and get dropped at refresh); the creator can close and reclaim rent.
2. **Payment guards**: `begin_guard_v0` / `assert_paid_v0`, bracketing the executor. `begin_guard_v0` takes a signing `payer` (which funds the guard account) and a non-signing `payout` (whose balance is measured) — deliberately different accounts, see the trust model above:

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
- **Commitment** — reads, subscriptions, and simulation all run at **`processed`**. A keeper's simulation should see the state the next leader will see, not state a slot or two old, and the exposure is bounded by design: every action is simulated before it is sent, and a wrong guess costs a reverted transaction's fee (which the adaptive delay above then damps). The one place `confirmed` is used deliberately is the **blockhash** — a `processed` blockhash can be on a fork that is abandoned, and the transaction would then never land. Signature outcomes also require `confirmed`: a `processed`-only status is not an outcome yet, because the fork carrying it can still be dropped, and the caller acts on these (booking payment, ramping a delay off a revert). Leaving it pending means a dropped fork surfaces as a retryable `Expired` rather than a landed crank that never happened.
- **Fork detection** — `processed` reads need this, and the account stream cannot provide it. If a write lands on a fork that is then abandoned, the canonical chain never writes that account, so **no correcting notification ever arrives** — and nothing in the stream distinguishes that from "unchanged", which is precisely the silence the cache is designed to trust. The value would stand until the age ceiling expired, feeding simulations a world that never happened. So both transports also subscribe to slots (`slotSubscribe`, which carries parent and root; yellowstone's slot filter, where only the *processed* status may move the tip — confirmed and finalized repeat slots already passed and would read as a switch every slot). A fork switch is a processed slot at or below the tip, or one built on a parent below it; ordinary skipped slots are neither, since they leave the tip advancing and the parent at the previous tip. On a switch, cached entries above the fork point are dropped and refetched on next use. Invalidation runs after account updates in the same drain pass, so a write from the fork being abandoned cannot slip in behind it — that also drops the new fork's fresh writes, which is conservative in the direction that costs a refetch rather than a wrong simulation. Failing to subscribe to slots is non-fatal: fork detection degrades to age-based revalidation rather than losing the session. `chain_reorgs_total` counts switches and the accounts they invalidated.
- **Cache freshness** — a cache in front of a simulator is a correctness problem, not a performance one: a stale account makes the simulation decide about a world that no longer exists. The rule turns on *why* an account has been quiet. Backends publish **coverage** (what they hold live subscriptions for); for a covered account on a healthy feed, silence means unchanged and the cached value is authoritative. For an uncovered one, silence means nothing, so it may be served for at most `--max-age-uncovered-ms` (default 400ms, about a slot) before it is refetched. Two backstops: the clock sysvar is always subscribed and ticks every slot, so total silence past `--feed-silence-s` proves the *feed* is dead rather than the chain quiet, and coverage is disbelieved wholesale; and even covered accounts are revalidated after `--max-age-covered-s`. Losing a session revokes coverage immediately rather than waiting for a timeout. `relay_cache_reads_total{coverage}` climbing on `uncovered` is the signal to add a `--watch-program`.
- **Packing** — verified cranks share a transaction (`--max-cranks-per-tx`, default 3), splitting one signature fee. Guard triples stay contiguous, which is what makes reusing one guard account within a transaction safe: each `begin_guard` re-arms before its own executor and its own `assert_paid` consumes it. Size is checked by serializing, not estimating; a pack that fails to simulate falls back to individual submission, which is cheap because every member already simulated alone. Compute-unit limits come from the simulation (fees are billed on the limit you *request*), and the priority fee is sampled by the submitter.
- **Profitability + metrics** — the submitter books each outcome into a rolling per-program net-lamports window, published over another `watch` channel; `--min-program-profit` skips programs that keep losing money rather than retrying them forever.
- **Adaptive contention delay** — the graduated version of that, and the one that matters in a competitive fleet. A turner that is simply slower than a rival loses every race, and each loss costs a fee for a *reverted* crank: it does the work, arrives second, and pays for the privilege. Nothing about losing tells it to stop, so it loses forever. The fix is to lose deliberately — hold the program's cranks back a few slots, and the rival's transaction lands before this turner even resolves, so its simulation reports nothing to do and no transaction is ever built. A loss that costs nothing is sustainable.

  The signal is a reverted transaction, not overall profit: it is the one outcome that means a fee bought nothing, and both of its causes (a rival landed first, or state moved under a simulation that had already passed) have the same fix. Each revert adds `--contention-step-slots` (default 4, ~1.6s) up to `--max-contention-slots` (default 12, ~5s); each landed crank halves it. Additive increase converges on the smallest delay that actually clears the rival instead of overshooting; multiplicative decay makes recovery take a handful of wins rather than one, so a single landed crank does not snap the delay to zero and immediately pay for another revert.

  The delay is applied in `decide`, ahead of resolve and simulate, and that placement is the mechanism — sleeping after simulation and before submission would be worse than not waiting, since it would submit a transaction built on state already known to be stale. The wait is re-measured against the *live* delay each tick rather than frozen when it began, so a rival going away releases held work on the next tick instead of serving out a sentence handed down under the old conditions. `relay_contention_delay_slots{program}` is the gauge; nonzero means a rival is winning.

  Recovery is the half worth designing for. A turner that backed off permanently would leave the protocol uncranked the moment its rival died, so decay is driven by cranks landing: the rival stops, this turner's simulations start finding real work, its cranks land, and the delay walks back to zero. The lasting cost of a competitor disappearing is a few seconds of lateness. Note that decay needs work to land — on an idle registry the delay freezes where contention left it, which is harmless but means the gauge is only meaningful alongside throughput.

  When to turn it off (`--contention-step-slots 0`): work where a win is worth far more than a transaction fee and only one turner can have it — a liquidation, say. There, trying and losing still has positive expected value, so paying for reverts is rational. The dynamics partly handle this on their own, since occasional wins decay the delay, but a low ceiling or zero step is the explicit answer. Prometheus at `/metrics`, liveness at `/health`. Two labels are there for otherwise-invisible failures: `relay_update_source` (subscription vs repoll — a dead stream shows up as the poll counter climbing alone) and `relay_wake_lag_seconds` (how late cranks are, i.e. whether the turner is oversubscribed).

`ws` and `grpc` are `CachedSource<RpcSource>`: the subscription feeds an account cache, and misses (plus a periodic `repoll_every` refetch, the tuktuk dual ws+poll insurance) fall through to RPC. Subscriptions only replace *reads* — simulation and submission always go to RPC. Loop per condition:

```
wake hint due? → sim resolver (told which condition fired) → work?
  → read staged payload from post-sim account state (executor id, accounts, args)
  → build [begin_guard, executor, assert_paid] with the keeper injected
  → sim (success ⇒ pays ≥ min_payment) → send → backoff/dedup bookkeeping
```

Failure modes accepted by design: stale local view ⇒ tx lands-and-fails (tx fee, same exposure as any keeper — and the adaptive delay above is what stops it repeating indefinitely), hint fires early ⇒ wasted sim (cheap). Hints firing late is the only design bug; conditions should include an `EverySlots` fallback when in doubt.

## Observability

Four questions, and the metrics exist to answer them rather than to describe the code: what is generating load (`relay_evaluations_total{wake, program}` — every condition looked at, not every crank done), why work is not happening (`relay_skips_total{reason}` — because `outcome="skipped"` lumped nine unrelated reasons into one series, and "nothing is due" and "a target program tried to take our signature" are not the same event), where the time goes (`relay_tick_seconds` per loop, `relay_stage_seconds` per condition, `chain_rpc_seconds` per provider call, `relay_saturated_ticks_total` for when `--concurrency` is the limit), and whether it pays (`relay_lamports_total`, `relay_compute_units`, `relay_contention_delay_slots`).

Two splits are deliberate. Decision latency (`relay_wake_lag_seconds`, wake due → submitted) is separate from cluster latency (`relay_confirm_seconds`, submitted → settled), because they have different fixes and averaging them hides both. And per-condition stage timing is separate from per-tick timing, because a single expensive resolver is otherwise averaged into a tick that looks fine.

Cardinality is capped by construction: programs are labelled by an 8-character prefix, conditions are not labelled at all. Per-condition drilldown belongs to the CLI, which reads the chain directly — a registry of 10,000 watches must not become 30,000 time series. `grafana/relay-dashboard.json` is the consumer, and the e2e asserts its series exist so a rename cannot silently empty a panel.

## Debugging surface

`Turner::explain` is the whole of it: it runs `decide` and the crank path for one condition and reports where it got to, stopping at a prepared transaction so it never sends. Everything the `relay` CLI prints is a rendering of that, which is deliberate — a debugging tool that reimplements the predicate it is debugging will eventually disagree with production, and the disagreement will land exactly when someone is relying on it. Two limits are inherent and are surfaced rather than hidden: a fresh process has no `last_seen`, so change-wakes read as due, and per-process suppression (backoff, contention delay, profitability) is invisible to it, so the CLI scrapes the daemon's metrics endpoint for those.

The failure mode worth designing for is a watch rejected at refresh. It never enters the tracked set — not fetched, not subscribed, not cranked — so it is absent from every view while being plainly registered on chain, which is the one situation where an operator is right and the tool looks wrong. `RefreshSummary` carries `(Watch, RejectReason)` for exactly this, and every command that answers "why isn't this cranking" checks it before concluding anything.

## What stays out

Discovery over unbounded account sets (finding liquidatable users) is not expressible as a resolver — resolvers enumerate from watched accounts; searching stays in bespoke bots. That boundary is intentional.

## Repo layout

- `spec/` — `relay-spec`: the wire types. Zero-copy pod, alignment 1; depends only on bytemuck.
- `relay-anchor/` — `relay-anchor`: the block as one typed field in an Anchor 1.0 account (`Deref` to the spec type, `Pod`, the condition surface by delegation, and an `IdlBuild` impl describing the region as opaque bytes). Its own workspace, because it is the one crate here that depends on anchor at all; generic over the spec version, so a `RelayBlockV1` is a new alias rather than a break for hosts.
- `programs/` — separate cargo workspace (anchor-v2 pinned rev, own lockfile + target): `relay` program, `demo-book` (reference target: a two-sided book carrying one condition per wake kind — `AtTimestamp` expiry sweep with a self-repairing hint, `OnAccountChange` soft-cap eviction on `entry_count`, and `OnAccountChange` crossing on a `version` counter every mutation bumps, i.e. "whenever the book changes at all, look for a cross" — all three served by one `resolve_v0`). Hosts the cross-program tests.
- `crank-turner/` — root-workspace client crate (solana 3.x), litesvm tests drive the full loop against built `.so` fixtures.

`crank-turner/tests/validator_e2e.rs` runs three scenarios against a real `solana-test-validator` (`./scripts/e2e.sh`). Two drive `tick()` directly — good for precise assertions, but they bypass everything the binary adds. The third, `shipped_daemon_cranks_over_websocket`, **spawns the actual `relay-crank-turner` process** with `--transport ws` against the validator's pubsub port and never touches it again: the test only creates a book and posts orders, and asserts the cranks happened, that the log shows the websocket path was taken rather than a silent fallback, and that the daemon's own `/metrics` shows reads served from subscription coverage. That is the one that answers "does the thing we ship work".

The older description below still applies to the first scenario. `crank-turner/tests/validator_e2e.rs` runs the whole thing against a real `solana-test-validator` (`./scripts/e2e.sh`): both programs deployed, a turner on its normal loop, a bot posting orders, and assertions that expiry, eviction, and crossing each get cranked and paid for. It is `#[ignore]`d in the default run because it needs the validator binary.

Program keypairs are never committed; `declare_id!` pubkeys only.
