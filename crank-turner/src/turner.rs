//! The condition loop: wake evaluation → resolver simulation (told which
//! condition fired) → read the staged payload out of post-simulation account
//! state → build the executor **it names** → simulation → send. Deliberately a pull-style `tick()` state machine so
//! tests (and alternative runtimes) drive it deterministically; `main.rs`
//! wraps it in a timer.
//!
//! The executor goes into the transaction **directly**, bracketed by
//! relay's payment guards rather than wrapped in a CPI: `begin_guard_v0`
//! snapshots the keeper's balance, the executor runs, `assert_paid_v0`
//! reverts everything unless the keeper gained at least the turner's
//! price. That keeps all four CPI levels available to the executor's own
//! call stack (a velocity crank that calls into the CLOB needs them) and
//! costs two ~1k-CU instructions instead of an invoke.

use std::collections::{HashMap, HashSet};
use std::time::Instant;

use anyhow::{Context, Result};
use futures_util::stream::{FuturesUnordered, StreamExt};
use relay_spec as spec;
use solana_compute_budget_interface::ComputeBudgetInstruction;
use solana_sdk::account::Account;
use solana_sdk::instruction::{AccountMeta, Instruction};
use solana_sdk::message::Message;
use solana_sdk::pubkey::Pubkey;
use solana_sdk::signature::{Keypair, Signature};
use solana_sdk::signer::Signer;
use solana_sdk::transaction::Transaction;

use tracing::warn;

use crate::filter::{RefreshSummary, WatchFilter};
use crate::metrics;
use crate::submit::{PendingTx, SubmitterHandle};
use relay_chain_source::{ChainSource, ClockSnapshot};

/// One registered watch, parsed from a `WatchV0` account.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Watch {
    /// Owner program of `target`, recorded on chain at registration.
    pub target_program: Pubkey,
    pub target: Pubkey,
    /// Who registered the watch, and the only key that may close it.
    pub creator: Pubkey,
    pub offset: u32,
}

/// Identity of a condition across ticks.
pub type CondKey = (Pubkey, u32, u8);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SkipReason {
    /// Wake hint not due.
    NotDue,
    /// Suppressed by a prior no-work result or failure backoff.
    Backoff,
    /// Condition marked inactive.
    Inactive,
    /// Advertised payment below the turner's configured minimum.
    BelowMinPayment,
    /// Target data at the watch offset is not a parseable condition block
    /// (or a condition has an unknown wake kind) — inert, ignored.
    ParseFailed,
    /// The target program's recent cranks have cost more than they paid.
    Unprofitable,
    /// Held back deliberately: this program's cranks keep losing races, so
    /// the turner is arriving late on purpose to let a rival's transaction
    /// land and be caught by simulation instead of by a burned fee.
    ContentionDelay,
    /// The program is untrusted and no non-signing payout account is
    /// configured, so there is nowhere safe for it to pay.
    NoSafePayout,
    /// The resolver named the fee payer (or another transaction signer) in
    /// an untrusted executor's account list — a drain attempt.
    ExecutorNamedSigner,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stage {
    ResolveSim,
    ExecuteSim,
    Send,
}

#[derive(Debug, Clone)]
pub enum Outcome {
    Skipped(CondKey, SkipReason),
    /// Wake was due; the resolver reported nothing to do. This is the cheap
    /// path stale-early hints are allowed to take.
    NoWork(CondKey),
    Sent {
        condition: CondKey,
        signature: Signature,
        min_payment: u64,
    },
    Failed {
        condition: CondKey,
        stage: Stage,
        error: String,
    },
}

#[derive(Debug, Clone)]
pub struct TurnerConfig {
    /// The relay program (watch registry + `crank_v0`).
    pub relay_program: Pubkey,
    /// Skip conditions advertising less than this (the original tuktuk
    /// `min_crank_fee` pattern — a fleet of turners self-selects by fee).
    pub min_crank_payment: u64,
    /// Suppress re-evaluating a condition for this many slots after a
    /// no-work resolve, so stale-early hints don't re-simulate every tick.
    pub no_work_backoff_slots: u64,
    /// Base for exponential failure backoff (doubles per consecutive
    /// failure, capped at 2^6).
    pub failure_backoff_slots: u64,
    /// Slots to hold a condition after submitting a crank for it.
    ///
    /// A submitted crank is not a confirmed one: for a slot or two the
    /// chain still shows the old state, so the resolver still reports work
    /// and the turner re-sends. The duplicate is harmless — identical
    /// instructions over the same blockhash produce an identical
    /// signature, which the cluster dedups — but it burns an RPC call per
    /// tick until the first one lands. Keep this short enough that a
    /// batched crank (a sweep that only clears part of its backlog) still
    /// re-fires promptly.
    pub sent_backoff_slots: u64,
    /// Where executors pay. **Must not be the fee payer**: signer status
    /// is transaction-global on Solana, so any account that signs the
    /// transaction is a signer inside every instruction of it — including
    /// an untrusted executor's, which could then CPI a System transfer and
    /// drain it. A payout account that never signs cannot be touched.
    ///
    /// `None` means "pay the fee payer", which is only allowed for
    /// programs on `trusted_programs`.
    pub payout: Option<Pubkey>,
    /// Periodically roll a wrapped-SOL payout's lamports into its token
    /// balance via [`Turner::sync_payout`].
    ///
    /// A wSOL account is the natural payout: the SPL Token program owns
    /// it, so only *it* can debit the lamports, and it only does so for
    /// `transfer`/`close_account`, which need the authority — the keeper —
    /// to sign. The keeper is never in an executor's account list, so a
    /// hostile executor can credit the account and nothing else. Payment
    /// arrives as raw lamports (which is what the guard measures), and
    /// `sync_native` — an instruction with no signer at all — is what
    /// turns those lamports into spendable token balance.
    pub sync_native_payout: bool,
    /// Programs whose executors are run without payment guards and may be
    /// paid directly to the fee payer. This is the "I wrote this program,
    /// I do not need a condom" setting: it saves two instructions, their
    /// compute, and ~100 bytes of transaction, at the cost of every
    /// protection below. Only ever list programs you control.
    ///
    /// Note what decides it: the program the **resolver named**, which is not
    /// authenticated — nor was the literal a condition used to carry, so this
    /// is not new. Any target program can therefore have its cranks run
    /// against a listed program by naming it. The bar for listing one is
    /// consequently not "I wrote it" but "I am happy for anyone to invoke its
    /// permissionless surface with my fee payer in the account list".
    pub trusted_programs: HashSet<Pubkey>,
    /// Bracket untrusted executors with relay's payment guards. Off means
    /// trusting simulation alone: cheaper, but nothing catches a payment
    /// that shrinks between simulating and landing.
    pub guard_payments: bool,
    /// Which of the keeper's guard accounts to use. Turners running cranks
    /// concurrently should vary this per in-flight transaction so they
    /// don't serialize on one write lock.
    pub guard_nonce: u8,
    /// Ceiling on the account data a crank transaction may load, in bytes.
    ///
    /// Billed like the compute limit: on the figure the transaction asks for,
    /// not on what it loads. Asking for nothing takes the 64 MiB default and
    /// pays for all of it, which for a crank that loads a program and a
    /// handful of accounts is most of the transaction's cost.
    ///
    /// The target program and its program data count, because a crank names
    /// the program — so this has to clear the largest program this turner
    /// cranks, with room for it to grow. Too low and the transaction is
    /// refused before it runs.
    pub loaded_accounts_data_size: u32,
    /// Which watches this turner is willing to track at all. Default is
    /// unrestricted; scope it to your own programs when other protocols
    /// share the registry.
    pub filter: WatchFilter,
    /// How many conditions to resolve and submit at once. Every crank is
    /// several RPC round trips, so a sequential turner spends nearly all
    /// its time waiting; this is the single biggest throughput knob.
    pub concurrency: usize,
    /// Skip programs whose rolling net lamports are below this (negative
    /// allows some loss). Only applies when a submitter is attached, since
    /// that is what observes outcomes.
    pub min_program_profit: i64,
    /// Pack up to this many cranks into one transaction. Each crank costs
    /// its own guard pair and account set, but they share the 5000-lamport
    /// signature fee — the saving that makes packing worth the complexity
    /// once cranks are small and frequent. 1 disables packing.
    pub max_cranks_per_tx: usize,
    /// Ceiling on the priority fee in micro-lamports per compute unit.
    pub max_priority_fee: u64,
}

/// Simulation probe budget: the per-transaction ceiling, so the probe is
/// never the thing that fails.
const MAX_COMPUTE_UNITS: u32 = 1_400_000;

/// Solana's packet limit.
const MAX_TRANSACTION_BYTES: usize = 1232;

/// The condition an outcome belongs to.
fn outcome_key(outcome: &Outcome) -> CondKey {
    match outcome {
        Outcome::Skipped(key, _) | Outcome::NoWork(key) => *key,
        Outcome::Sent { condition, .. } | Outcome::Failed { condition, .. } => *condition,
    }
}

/// Headroom over the simulated cost, for state that moved since.
fn compute_limit(units_consumed: u64) -> u32 {
    ((units_consumed as f64 * 1.2) as u64).clamp(1_000, MAX_COMPUTE_UNITS as u64) as u32
}

/// SPL Token, and its `SyncNative` instruction tag. Hand-rolled rather
/// than pulling in the token crate for one 1-byte instruction.
static TOKEN_PROGRAM_ID: std::sync::LazyLock<Pubkey> = std::sync::LazyLock::new(|| {
    "TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA"
        .parse()
        .expect("valid program id")
});
const SYNC_NATIVE_TAG: u8 = 17;

/// `ComputeBudget111111111111111111111111111111`.
static COMPUTE_BUDGET_PROGRAM_ID: std::sync::LazyLock<Pubkey> = std::sync::LazyLock::new(|| {
    "ComputeBudget111111111111111111111111111111"
        .parse()
        .expect("valid program id")
});

/// System program id (the all-zero address), required by
/// `begin_guard_v0`'s lazy guard-account creation.
const SYSTEM_PROGRAM_ID: Pubkey = Pubkey::new_from_array([0u8; 32]);

/// Default relay program id (`declare_id!` in `programs/relay`).
pub const RELAY_PROGRAM_ID: &str = "4D5tPhw9sqkdkR5CpmP427TH6y9p9AMuKUukUEHn3Mpu";

impl Default for TurnerConfig {
    fn default() -> Self {
        Self {
            relay_program: RELAY_PROGRAM_ID.parse().unwrap(),
            min_crank_payment: 0,
            no_work_backoff_slots: 8,
            failure_backoff_slots: 16,
            sent_backoff_slots: 2,
            payout: None,
            sync_native_payout: false,
            trusted_programs: HashSet::new(),
            guard_payments: true,
            guard_nonce: 0,
            loaded_accounts_data_size: 12 * 1024 * 1024,
            filter: WatchFilter::default(),
            concurrency: 8,
            min_program_profit: i64::MIN,
            max_cranks_per_tx: 3,
            max_priority_fee: 1_000_000,
        }
    }
}

#[derive(Debug, Default)]
struct CondState {
    /// Last-seen bytes of a change-wake's watched range. `None` = never
    /// evaluated (counts as changed).
    last_seen: Option<Vec<u8>>,
    /// Slot before which this condition is not re-evaluated.
    suppress_until: u64,
    /// Consecutive failures (drives exponential backoff).
    failures: u32,
    /// Last slot an `EverySlots` wake fired.
    last_fired: Option<u64>,
    /// Slot at which this condition was first seen due, when a contention
    /// delay is holding it back. Cleared once it is acted on, so each new
    /// piece of work is delayed afresh rather than once per condition.
    deferred_since: Option<u64>,
}

pub struct Turner<S: ChainSource> {
    source: S,
    keeper: Keypair,
    config: TurnerConfig,
    watches: Vec<Watch>,
    state: HashMap<CondKey, CondState>,
    /// Where signed transactions go. Without one the turner sends inline
    /// through the source and forgets — fine for tests and small
    /// deployments, but it neither confirms nor resends.
    submitter: Option<SubmitterHandle>,
}

/// How a condition's bookkeeping should change after an attempt. Collected
/// during the concurrent phase and applied afterwards, so the execution
/// path needs no shared mutable state at all — no locks, no channels.
#[derive(Debug)]
enum StateUpdate {
    /// Resolver reported nothing to do; settle the wake and back off.
    NoWork { last_seen: Option<Vec<u8>> },
    /// Crank submitted.
    Sent,
    /// Attempt failed; extend the exponential backoff.
    Failed,
    /// Due, but deliberately held back to avoid paying for a race this
    /// program keeps losing. Records when the wait started; how long it
    /// lasts is re-read from the live delay each tick.
    Deferred,
}

impl<S: ChainSource> Turner<S> {
    pub fn new(source: S, keeper: Keypair, config: TurnerConfig) -> Self {
        Self {
            source,
            keeper,
            config,
            watches: Vec::new(),
            state: HashMap::new(),
            submitter: None,
        }
    }

    /// Route submissions through a [`SubmitterHandle`], which owns the
    /// shared blockhash, confirmation tracking, resends, and profitability
    /// accounting.
    pub fn with_submitter(mut self, submitter: SubmitterHandle) -> Self {
        self.submitter = Some(submitter);
        self
    }

    /// The registry as this turner sees it, after filtering.
    pub fn watches(&self) -> &[Watch] {
        &self.watches
    }

    /// Everything the turner concludes about one condition, and why.
    ///
    /// The verdict comes from the same `decide` and crank path the daemon
    /// runs — not a reimplementation — so a debugging tool built on this
    /// cannot disagree with production about whether a condition is due.
    /// The crank path stops at a prepared transaction, which is why this is
    /// a read-only operation: submission lives in `submit_packs`, not here.
    ///
    /// Two limits are inherent rather than incidental, and callers should
    /// surface them. A turner with no tick history has no `last_seen` to
    /// compare a change-wake against, so those always read as due; and
    /// `Backoff` and `ContentionDelay` are per-process state, so a fresh
    /// process never reports them however backed-off the daemon is.
    pub async fn explain(&self, key: CondKey) -> Result<Explanation> {
        let (target, offset, index) = key;
        let watch = *self
            .watches
            .iter()
            .find(|w| w.target == target && w.offset == offset)
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "no watch registered for {target} at offset {offset} \
                     (or it was filtered out by this turner's config)"
                )
            })?;
        let clock = self.source.clock().await?;
        let account = self
            .load_map(&[target])
            .await?
            .remove(&target)
            .ok_or_else(|| anyhow::anyhow!("target account {target} does not exist"))?;
        let (_, conditions) = spec::read_block(&account.data, offset as usize).map_err(|err| {
            anyhow::anyhow!("condition block at offset {offset} is unreadable: {err:?}")
        })?;
        let condition = *conditions.get(index as usize).ok_or_else(|| {
            anyhow::anyhow!(
                "condition {index} is past the end of the block ({} present)",
                conditions.len()
            )
        })?;

        // The change-wake's watched bytes may live on another account.
        let watched_now = match condition.wake() {
            Ok(spec::WakeView::OnAccountChange {
                address,
                offset,
                len,
            })
            | Ok(spec::WakeView::OnValueCross {
                address,
                offset,
                len,
                ..
            }) => {
                let watched = Pubkey::from(address);
                let data = if watched == target {
                    Some(account.data.clone())
                } else {
                    self.load_map(&[watched])
                        .await?
                        .remove(&watched)
                        .map(|acc| acc.data)
                };
                Some(data.map_or_else(Vec::new, |data| slice_or_empty(&data, offset, len)))
            }
            _ => None,
        };

        let resolver_accounts = materialize_resolver_accounts(&condition, &account.data)
            .ok_or_else(|| anyhow::anyhow!("condition's indirect resolver list is unreadable"))?;
        let verdict = match self.decide(
            key,
            &watch,
            &condition,
            &clock,
            watched_now.clone(),
            resolver_accounts,
        ) {
            Err(Outcome::Skipped(_, reason)) => Verdict::Skipped(reason),
            Err(other) => Verdict::Failed {
                stage: Stage::ResolveSim,
                error: format!("unexpected decide outcome: {other:?}"),
            },
            Ok(due) => match self.try_crank(&due, &clock).await {
                Ok(CrankResult::Ready(prepared)) => Verdict::WouldSend {
                    min_payment: prepared.min_payment,
                    units: prepared.units,
                    instructions: prepared.ixs,
                },
                Ok(CrankResult::Done(Outcome::NoWork(_), _)) => Verdict::NoWork,
                Ok(CrankResult::Done(Outcome::Failed { stage, error, .. }, _)) => {
                    Verdict::Failed { stage, error }
                }
                Ok(CrankResult::Done(Outcome::Skipped(_, reason), _)) => Verdict::Skipped(reason),
                Ok(CrankResult::Done(outcome, _)) => Verdict::Failed {
                    stage: Stage::Send,
                    error: format!("unexpected crank outcome: {outcome:?}"),
                },
                Err(err) => Verdict::Failed {
                    stage: Stage::ResolveSim,
                    error: format!("{err:#}"),
                },
            },
        };

        Ok(Explanation {
            key,
            program: watch.target_program,
            creator: watch.creator,
            condition,
            clock,
            watched_now,
            stateless: self.state.is_empty(),
            verdict,
        })
    }

    /// Submit a prepared crank. Split out from [`Self::explain`] so a
    /// debugging tool can show what would be sent and then send exactly
    /// that, rather than re-deriving it.
    pub async fn send_explained(&self, explanation: &Explanation) -> Result<Signature> {
        let Verdict::WouldSend { instructions, .. } = &explanation.verdict else {
            anyhow::bail!("nothing to send: {:?}", explanation.verdict);
        };
        let (tx, _units) = self.sign_for_submission(instructions).await?;
        self.source.send_transaction(&tx).await
    }

    pub fn keeper_pubkey(&self) -> Pubkey {
        self.keeper.pubkey()
    }

    pub fn source(&self) -> &S {
        &self.source
    }

    /// Re-scan the registry and re-apply the watch filter. Returns what
    /// was admitted and why anything else was dropped.
    ///
    /// Filters run cheapest-first: the provider is asked only for allowed
    /// programs (server-side memcmp), registry-only rules are decided from
    /// the watch account alone, and only survivors have their target
    /// fetched for the size / owner / fee checks. A watch dropped here
    /// stops being fetched and subscribed entirely until the next refresh.
    pub async fn refresh_watches(&mut self) -> Result<RefreshSummary> {
        let started = Instant::now();
        let filter = self.config.filter.clone();
        let accounts = self
            .source
            .get_program_accounts(
                &self.config.relay_program,
                &crate::watches::watch_filter_sets(&filter.server_side_programs()),
            )
            .await?;
        let mut candidates: Vec<Watch> = accounts
            .iter()
            .filter_map(|(_, account)| spec::WatchV0::read_from_account(&account.data).ok())
            .map(|w| Watch {
                target_program: Pubkey::from(w.target_program),
                target: Pubkey::from(w.target),
                creator: Pubkey::from(w.creator),
                offset: w.offset,
            })
            .collect();
        candidates.sort_by_key(|w| (w.target.to_bytes(), w.offset));
        candidates.dedup();

        let mut summary = RefreshSummary::default();
        let (registry_ok, registry_rejected): (Vec<_>, Vec<_>) = candidates
            .into_iter()
            .map(|w| {
                let verdict = filter.check_registry(&w);
                (w, verdict)
            })
            .partition(|(_, verdict)| verdict.is_ok());
        summary.rejected.extend(
            registry_rejected
                .into_iter()
                .map(|(w, verdict)| (w, verdict.unwrap_err())),
        );

        // One fetch of the survivors covers the size, owner, and fee gates.
        let targets: Vec<Pubkey> = registry_ok.iter().map(|(w, _)| w.target).collect();
        let fetched = self.load_map(&targets).await?;
        let admitted: Vec<Watch> = registry_ok
            .into_iter()
            .map(|(w, _)| w)
            .filter(|w| {
                let account = fetched.get(&w.target);
                match filter
                    .check_target(w, account)
                    .and_then(|()| self.check_conditions(w, account))
                {
                    Ok(()) => true,
                    Err(reason) => {
                        summary.rejected.push((*w, reason));
                        false
                    }
                }
            })
            .collect();

        let (kept, over_capacity) = match self.config.filter.max_watches {
            Some(max) if admitted.len() > max => admitted.split_at(max),
            _ => (admitted.as_slice(), &[][..]),
        };
        summary.rejected.extend(
            over_capacity
                .iter()
                .map(|w| (*w, crate::filter::RejectReason::OverCapacity)),
        );
        summary.admitted = kept.len();
        metrics::WATCHES
            .with_label_values(&["admitted"])
            .set(kept.len() as i64);
        metrics::WATCHES
            .with_label_values(&["rejected"])
            .set(summary.rejected.len() as i64);
        // Counters as well as the gauges: a watch that flaps in and out of
        // admission is invisible in a gauge sampled every 15 seconds.
        summary.rejected.iter().for_each(|(_, reason)| {
            metrics::REGISTRY_REJECTED
                .with_label_values(&[crate::filter::reject_label(reason)])
                .inc();
        });
        metrics::REFRESH_SECONDS
            .with_label_values(&["total"])
            .observe(started.elapsed().as_secs_f64());
        self.watches = kept.to_vec();
        // Forget state for watches we no longer track, so a re-admitted
        // watch starts clean rather than inheriting a stale backoff.
        let tracked: std::collections::HashSet<(Pubkey, u32)> =
            self.watches.iter().map(|w| (w.target, w.offset)).collect();
        self.state
            .retain(|(target, offset, _), _| tracked.contains(&(*target, *offset)));
        Ok(summary)
    }

    /// Fee gate: a watch earns its place only if some active condition pays
    /// at least `min_crank_payment`.
    fn check_conditions(
        &self,
        watch: &Watch,
        account: Option<&Account>,
    ) -> Result<(), crate::filter::RejectReason> {
        let account = account.ok_or(crate::filter::RejectReason::TargetMissing)?;
        let (_, conditions) = spec::read_block(&account.data, watch.offset as usize)
            .map_err(|_| crate::filter::RejectReason::Unparseable)?;
        conditions
            .iter()
            .any(|c| c.is_active() && c.min_payment() >= self.config.min_crank_payment)
            .then_some(())
            .ok_or(crate::filter::RejectReason::PaysTooLittle)
    }

    /// One pass over every known condition.
    ///
    /// Three phases: decide which wakes are due (cheap, sequential, no
    /// I/O), crank the due ones **concurrently**, then fold the resulting
    /// bookkeeping back in. Splitting it this way is what makes the
    /// concurrent phase lock-free — it borrows nothing mutable — and keeps
    /// one slow simulation from stalling every other condition.
    pub async fn tick(&mut self) -> Result<Vec<Outcome>> {
        let started = Instant::now();
        let clock = self.source.clock().await?;

        // Load all targets, parse their condition blocks.
        let targets: Vec<Pubkey> = self.watches.iter().map(|w| w.target).collect();
        let target_accounts = self.load_map(&targets).await?;
        // Conditions are borrowed straight out of the fetched account data:
        // every wire type is alignment 1, so a block is a cast rather than a
        // copy per condition — which matters on a registry of thousands
        // read every tick.
        let parsed: Vec<(Watch, Option<&[spec::ConditionV0]>)> = self
            .watches
            .iter()
            .map(|w| {
                let conditions = target_accounts
                    .get(&w.target)
                    .and_then(|acc| spec::read_block(&acc.data, w.offset as usize).ok())
                    .map(|(_, conditions)| conditions);
                (*w, conditions)
            })
            .collect();

        // Change wakes may watch accounts other than the target; fetch
        // those too (deduped, skipping ones already loaded).
        let watched_extras: Vec<Pubkey> = parsed
            .iter()
            .filter_map(|(_, conditions)| *conditions)
            .flat_map(|conditions| conditions.iter())
            .filter_map(|c| match c.wake() {
                Ok(spec::WakeView::OnAccountChange { address, .. })
                | Ok(spec::WakeView::OnValueCross { address, .. }) => {
                    let pk = Pubkey::from(address);
                    (!target_accounts.contains_key(&pk)).then_some(pk)
                }
                _ => None,
            })
            .collect();
        let extra_accounts = self.load_map(&watched_extras).await?;
        let account_of = |pk: &Pubkey| -> Option<&Account> {
            target_accounts.get(pk).or_else(|| extra_accounts.get(pk))
        };

        // Phase 1 — decide. Pure function of cached state and accounts.
        let mut outcomes: Vec<Outcome> = Vec::new();
        let mut due: Vec<Due> = Vec::new();
        let mut updates: Vec<(CondKey, StateUpdate)> = Vec::new();
        for (watch, conditions) in &parsed {
            let Some(conditions) = conditions else {
                metrics::SKIPS
                    .with_label_values(&[
                        skip_label(&SkipReason::ParseFailed),
                        &metrics::program_label(&watch.target_program),
                    ])
                    .inc();
                outcomes.push(Outcome::Skipped(
                    (watch.target, watch.offset, 0),
                    SkipReason::ParseFailed,
                ));
                continue;
            };
            for (index, condition) in conditions.iter().enumerate() {
                let key: CondKey = (watch.target, watch.offset, index as u8);
                let watched_now: Option<Vec<u8>> = match condition.wake() {
                    Ok(spec::WakeView::OnAccountChange {
                        address,
                        offset,
                        len,
                    })
                    | Ok(spec::WakeView::OnValueCross {
                        address,
                        offset,
                        len,
                        ..
                    }) => Some(
                        account_of(&Pubkey::from(address))
                            .map(|acc| slice_or_empty(&acc.data, offset, len))
                            .unwrap_or_default(),
                    ),
                    _ => None,
                };
                let program_label = metrics::program_label(&watch.target_program);
                metrics::EVALUATIONS
                    .with_label_values(&[wake_label(condition), &program_label])
                    .inc();
                let Some(resolver_accounts) = target_accounts
                    .get(&watch.target)
                    .and_then(|acc| materialize_resolver_accounts(condition, &acc.data))
                else {
                    metrics::SKIPS
                        .with_label_values(&[skip_label(&SkipReason::ParseFailed), &program_label])
                        .inc();
                    outcomes.push(Outcome::Skipped(key, SkipReason::ParseFailed));
                    continue;
                };
                match self.decide(
                    key,
                    watch,
                    condition,
                    &clock,
                    watched_now,
                    resolver_accounts,
                ) {
                    Ok(ready) => due.push(ready),
                    Err(outcome) => {
                        if let Outcome::Skipped(_, reason) = &outcome {
                            metrics::SKIPS
                                .with_label_values(&[skip_label(reason), &program_label])
                                .inc();
                        }
                        // The one skip with bookkeeping: record when the
                        // wait started, so the delay is measured from
                        // first sight rather than from the latest tick.
                        if matches!(outcome, Outcome::Skipped(_, SkipReason::ContentionDelay)) {
                            updates.push((key, StateUpdate::Deferred));
                        }
                        outcomes.push(outcome);
                    }
                }
            }
        }
        metrics::TICK_SECONDS
            .with_label_values(&["decide"])
            .observe(started.elapsed().as_secs_f64());

        // Phase 2 — crank the due conditions concurrently. Bounded so a
        // large registry cannot open an unbounded number of RPC calls.
        let executing = Instant::now();
        metrics::IN_FLIGHT
            .with_label_values(&["cranks"])
            .set(due.len() as i64);
        if !due.is_empty() {
            // Bucketed per program so one busy protocol is distinguishable
            // from a registry-wide surge.
            let mut per_program: HashMap<Pubkey, u64> = HashMap::new();
            due.iter()
                .for_each(|d| *per_program.entry(d.program).or_default() += 1);
            per_program.iter().for_each(|(program, count)| {
                metrics::DUE_PER_TICK
                    .with_label_values(&[&metrics::program_label(program)])
                    .observe(*count as f64);
            });
            if due.len() > self.config.concurrency {
                metrics::SATURATED_TICKS
                    .with_label_values(&["concurrency"])
                    .inc();
            }
        }
        // Explicit loops: the closure forms would need `&mut running`
        // alongside the `&self` the in-flight futures hold.
        let mut prepared: Vec<Prepared> = Vec::new();
        {
            let mut running = FuturesUnordered::new();
            let mut queued = due.into_iter();
            for _ in 0..self.config.concurrency {
                match queued.next() {
                    Some(next) => running.push(self.crank(next, &clock)),
                    None => break,
                }
            }
            // Refill as each finishes, so the pipeline stays full rather
            // than draining in lockstep batches.
            while let Some(result) = running.next().await {
                if let Some(next) = queued.next() {
                    running.push(self.crank(next, &clock));
                }
                match result {
                    CrankResult::Ready(ready) => prepared.push(*ready),
                    CrankResult::Done(outcome, update) => {
                        updates.push((outcome_key(&outcome), update));
                        outcomes.push(outcome);
                    }
                }
            }
        }
        // Verified cranks share transactions where they fit: each carries
        // its own guards and accounts, but they split one signature fee.
        self.submit_packs(prepared, &mut outcomes, &mut updates)
            .await;
        metrics::IN_FLIGHT.with_label_values(&["cranks"]).set(0);
        metrics::TICK_SECONDS
            .with_label_values(&["execute"])
            .observe(executing.elapsed().as_secs_f64());

        // Phase 3 — apply bookkeeping.
        updates
            .into_iter()
            .for_each(|(key, update)| self.apply(key, update, &clock));
        metrics::TICK_SECONDS
            .with_label_values(&["total"])
            .observe(started.elapsed().as_secs_f64());
        Ok(outcomes)
    }

    /// Is this condition due? `Err` carries the skip outcome.
    fn decide(
        &self,
        key: CondKey,
        watch: &Watch,
        condition: &spec::ConditionV0,
        clock: &ClockSnapshot,
        watched_now: Option<Vec<u8>>,
        resolver_accounts: Vec<spec::AccountRefV0>,
    ) -> Result<Due, Outcome> {
        if !condition.is_active() {
            return Err(Outcome::Skipped(key, SkipReason::Inactive));
        }
        if condition.min_payment() < self.config.min_crank_payment {
            return Err(Outcome::Skipped(key, SkipReason::BelowMinPayment));
        }
        let Ok(wake) = condition.wake() else {
            return Err(Outcome::Skipped(key, SkipReason::ParseFailed));
        };
        // A program that keeps losing us money is deprioritized rather
        // than retried forever.
        if let Some(submitter) = &self.submitter {
            if submitter.profit_for(&watch.target_program) < self.config.min_program_profit {
                return Err(Outcome::Skipped(key, SkipReason::Unprofitable));
            }
        }
        let default = CondState::default();
        let state = self.state.get(&key).unwrap_or(&default);
        if clock.slot < state.suppress_until {
            return Err(Outcome::Skipped(key, SkipReason::Backoff));
        }
        let due = match wake {
            spec::WakeView::AtTimestamp { unix_ts } => unix_ts <= clock.unix_timestamp,
            spec::WakeView::AtSlot { slot } => slot <= clock.slot,
            spec::WakeView::EverySlots { slots } => state
                .last_fired
                .is_none_or(|last| clock.slot >= last.saturating_add(slots)),
            spec::WakeView::OnAccountChange { .. } => {
                state.last_seen.as_deref() != watched_now.as_deref()
            }
            spec::WakeView::OnValueCross { threshold, cmp, .. } => watched_now
                .as_deref()
                .and_then(|bytes| spec::read_watched_value(bytes, threshold.is_unsigned()))
                .is_some_and(|value| spec::value_crossed(value, threshold, cmp)),
        };
        if !due {
            return Err(Outcome::Skipped(key, SkipReason::NotDue));
        }
        // Contention delay: this program's cranks keep getting beaten, so
        // hold this one back and let the rival's transaction land first.
        //
        // The wait has to happen here, ahead of resolve and simulate, and
        // that placement is the whole mechanism. Arriving late means the
        // resolver reports nothing to do and no transaction is ever built,
        // so a lost race costs nothing. Sleeping later — after simulation,
        // before submission — would be worse than not waiting at all: it
        // would submit a transaction built on state already known to be
        // stale.
        let delay = self.contention_delay(&watch.target_program);
        if delay > 0 {
            // Measured against the delay as it stands now, not as it stood
            // when the wait began. That is what makes recovery prompt: when
            // the rival stops and the published delay decays, work already
            // being held is released on the next tick instead of serving
            // out a sentence handed down under the old conditions.
            let waiting_since = state.deferred_since.unwrap_or(clock.slot);
            if clock.slot < waiting_since.saturating_add(delay) {
                return Err(Outcome::Skipped(key, SkipReason::ContentionDelay));
            }
        }

        // How late we are, relative to when the wake actually came due.
        let lag = match wake {
            spec::WakeView::AtTimestamp { unix_ts } => {
                (clock.unix_timestamp - unix_ts).max(0) as f64
            }
            _ => 0.0,
        };
        metrics::WAKE_LAG
            .with_label_values(&[&metrics::program_label(&watch.target_program)])
            .observe(lag);
        Ok(Due {
            key,
            program: watch.target_program,
            condition: *condition,
            watched_now,
            resolver_accounts,
        })
    }

    /// Fold one attempt's bookkeeping back into the condition state.
    fn apply(&mut self, key: CondKey, update: StateUpdate, clock: &ClockSnapshot) {
        match update {
            StateUpdate::NoWork { last_seen } => {
                let backoff = self.config.no_work_backoff_slots;
                let state = self.state.entry(key).or_default();
                state.last_seen = last_seen;
                state.last_fired = Some(clock.slot);
                state.suppress_until = clock.slot + backoff;
                state.failures = 0;
                state.deferred_since = None;
            }
            StateUpdate::Sent => {
                let backoff = self.config.sent_backoff_slots;
                let state = self.state.entry(key).or_default();
                // The crank mutates watched state; force a fresh change
                // evaluation next tick rather than diffing against a
                // pre-crank snapshot.
                state.last_seen = None;
                state.last_fired = Some(clock.slot);
                state.suppress_until = clock.slot + backoff;
                state.failures = 0;
                state.deferred_since = None;
            }
            StateUpdate::Failed => {
                let base = self.config.failure_backoff_slots;
                let state = self.state.entry(key).or_default();
                state.failures += 1;
                // Saturating throughout: the base is operator-configured and
                // the shift is what makes a large one overflow.
                let backoff = base.saturating_mul(1u64 << state.failures.min(6));
                state.suppress_until = clock.slot.saturating_add(backoff);
                state.deferred_since = None;
            }
            StateUpdate::Deferred => {
                let state = self.state.entry(key).or_default();
                // Idempotent: this arrives on every tick of the wait, and
                // resetting the start each time would defer forever.
                //
                // Deliberately touching nothing else. The wake must still
                // read as due when the delay elapses, and `suppress_until`
                // stays out of it so the wait is re-measured against the
                // live delay rather than frozen at its starting value.
                state.deferred_since.get_or_insert(clock.slot);
            }
        }
    }

    /// Resolve → build → simulate → submit, for one due condition.
    ///
    /// Takes `&self`: everything it learns comes back as a [`StateUpdate`]
    /// for the caller to apply, which is what lets a whole tick's worth of
    /// cranks run concurrently without a lock between them.
    async fn crank(&self, due: Due, clock: &ClockSnapshot) -> CrankResult {
        let key = due.key;
        let program = metrics::program_label(&due.program);
        match self.try_crank(&due, clock).await {
            Ok(CrankResult::Ready(prepared)) => CrankResult::Ready(prepared),
            Ok(CrankResult::Done(outcome, update)) => {
                let label = match &outcome {
                    Outcome::NoWork(_) => "no_work",
                    Outcome::Failed { stage, .. } => {
                        metrics::FAILURES
                            .with_label_values(&[stage_label(stage), &program])
                            .inc();
                        "failed"
                    }
                    Outcome::Skipped(_, reason) => {
                        metrics::SKIPS
                            .with_label_values(&[skip_label(reason), &program])
                            .inc();
                        "skipped"
                    }
                    _ => "skipped",
                };
                metrics::CRANKS.with_label_values(&[label, &program]).inc();
                CrankResult::Done(outcome, update)
            }
            Err(err) => {
                metrics::CRANKS
                    .with_label_values(&["failed", &program])
                    .inc();
                metrics::FAILURES
                    .with_label_values(&["build", &program])
                    .inc();
                CrankResult::Done(
                    Outcome::Failed {
                        condition: key,
                        stage: Stage::Send,
                        error: format!("{err:#}"),
                    },
                    StateUpdate::Failed,
                )
            }
        }
    }

    async fn try_crank(&self, due: &Due, clock: &ClockSnapshot) -> Result<CrankResult> {
        let key = due.key;
        let condition = &due.condition;

        // Simulate the resolver, asking for its accounts back so the staged
        // payload can be read out of post-execution state.
        let resolver_accounts: Vec<Pubkey> = due
            .resolver_accounts
            .iter()
            .map(|a| Pubkey::from(a.address))
            .collect();
        // Tell the resolver which condition it is answering for: its
        // discriminator followed by the fired condition's coordinates. A
        // resolver serving several conditions cannot work without this, and
        // it costs 37 transaction bytes and no accounts.
        let resolver_ix = Instruction {
            program_id: Pubkey::from(condition.crank_spec().resolver_program),
            accounts: due.resolver_accounts.iter().map(account_ref_meta).collect(),
            data: spec::encode_resolver_data(
                condition.crank_spec().resolver_disc,
                spec::FiredConditionV0::new(key.0.to_bytes(), key.1, key.2),
            )
            .to_vec(),
        };
        let program_label =
            metrics::program_label(&Pubkey::from(condition.crank_spec().resolver_program));
        let sim = {
            let started = Instant::now();
            let (tx, _) = self.signed_tx(&[resolver_ix]).await?;
            let sim = self
                .source
                .simulate_transaction(&tx, &resolver_accounts)
                .await?;
            metrics::STAGE_SECONDS
                .with_label_values(&["resolve_sim", &program_label])
                .observe(started.elapsed().as_secs_f64());
            metrics::COMPUTE_UNITS
                .with_label_values(&["resolve", &program_label])
                .observe(sim.units_consumed as f64);
            sim
        };
        if let Some(err) = sim.err {
            return Ok(CrankResult::Done(
                Outcome::Failed {
                    condition: key,
                    stage: Stage::ResolveSim,
                    error: err,
                },
                StateUpdate::Failed,
            ));
        }

        let pointer = spec::ResponsePointerV0::read(sim.return_data.as_deref().unwrap_or_default())
            .map_err(|e| anyhow::anyhow!("resolver return data: {e:?}"))?;
        if !pointer.has_work() {
            return Ok(CrankResult::Done(
                Outcome::NoWork(key),
                StateUpdate::NoWork {
                    last_seen: due.watched_now.clone(),
                },
            ));
        }
        let resolved = read_staged(&sim.accounts, &pointer).context("staged resolver payload")?;

        // Build the executor itself — no CPI wrapper. Which instruction it
        // is comes from the resolver, not from the condition: if the turner
        // is willing to run the accounts and args a resolver chose, it is
        // willing to run the instruction it chose, and the guards bound the
        // damage either way.
        let program = Pubkey::from(resolved.executor_program);
        let Some(payout) = self.payout_for(&program) else {
            return Ok(CrankResult::Done(
                Outcome::Skipped(key, SkipReason::NoSafePayout),
                StateUpdate::Failed,
            ));
        };
        require_keeper_placeholder(&resolved)?;
        let executor_ix = Instruction {
            program_id: program,
            accounts: resolved
                .accounts
                .iter()
                .map(|a| AccountMeta {
                    pubkey: if a.address == spec::KEEPER_PLACEHOLDER {
                        payout
                    } else {
                        Pubkey::from(a.address)
                    },
                    // Belt: marking this false is right for every account
                    // that is not already a transaction signer. It is not
                    // sufficient on its own — see `names_a_signer`.
                    is_signer: false,
                    is_writable: a.is_writable(),
                })
                .collect(),
            data: resolved
                .executor_disc
                .iter()
                .copied()
                .chain(resolved.data.iter().copied())
                .collect(),
        };
        // Braces: an untrusted executor must not name any account that
        // signs this transaction, because signer status is
        // transaction-global — see `names_signer`.
        if !self.trusts(&program) && names_transaction_signer(&executor_ix, &[self.keeper.pubkey()])
        {
            warn!(
                %program,
                "untrusted executor named the fee payer; refusing to submit"
            );
            return Ok(CrankResult::Done(
                Outcome::Skipped(key, SkipReason::ExecutorNamedSigner),
                StateUpdate::Failed,
            ));
        }
        let ixs = self.guarded(executor_ix, payout, &program, condition.min_payment());

        // Simulate with a generous budget to learn the real cost, then let
        // the packing phase re-sign with a tight limit — the fee is charged
        // on the limit you request, not the units you burn.
        let executor_label = metrics::program_label(&program);
        let started = Instant::now();
        let probe = self.with_compute_budget(ixs.clone(), MAX_COMPUTE_UNITS, 0);
        let (probe_tx, _) = self.signed_tx(&probe).await?;
        let sim = self.source.simulate_transaction(&probe_tx, &[]).await?;
        metrics::STAGE_SECONDS
            .with_label_values(&["execute_sim", &executor_label])
            .observe(started.elapsed().as_secs_f64());
        metrics::COMPUTE_UNITS
            .with_label_values(&["execute", &executor_label])
            .observe(sim.units_consumed as f64);
        if let Some(err) = sim.err {
            return Ok(CrankResult::Done(
                Outcome::Failed {
                    condition: key,
                    stage: Stage::ExecuteSim,
                    error: err,
                },
                StateUpdate::Failed,
            ));
        }
        let _ = clock;
        Ok(CrankResult::Ready(Box::new(Prepared {
            key,
            program: due.program,
            min_payment: condition.min_payment(),
            units: sim.units_consumed,
            ixs,
        })))
    }

    /// Group verified cranks into transactions and submit them.
    ///
    /// Every crank here already passed simulation on its own, so a pack
    /// that fails to simulate is a packing artifact (too many accounts,
    /// conflicting state) rather than a bad crank — fall back to sending
    /// its members individually rather than dropping work. That fallback
    /// is affordable precisely because simulation is local.
    ///
    /// Guard triples stay contiguous, which is what makes sharing one
    /// guard account across a pack safe: each `begin_guard` re-arms before
    /// its own executor, and its own `assert_paid` consumes it.
    async fn submit_packs(
        &self,
        prepared: Vec<Prepared>,
        outcomes: &mut Vec<Outcome>,
        updates: &mut Vec<(CondKey, StateUpdate)>,
    ) {
        for pack in self.pack(prepared).await {
            // Measure the *assembled* transaction rather than summing the
            // per-crank estimates: the body picks up instructions the
            // individual probes never saw (the appended sync_native), and
            // billing on a limit that excludes them starves the tail.
            let body: Vec<Instruction> = pack.iter().flat_map(|p| p.ixs.iter().cloned()).collect();
            let measured = self.measure(&body).await;
            let Some(units) = measured else {
                // The assembly does not simulate, though every member did
                // on its own: a packing artifact. Send them individually
                // rather than dropping the work.
                metrics::PACKS.with_label_values(&["split"]).inc();
                for single in &pack {
                    self.submit_single(single, outcomes, updates).await;
                }
                continue;
            };
            let ixs = self.with_compute_budget(body, units, self.priority_fee());
            match self.sign_for_submission(&ixs).await {
                Ok((tx, expiry)) => self.finish_pack(&pack, tx, expiry, outcomes, updates).await,
                Err(err) => pack.iter().for_each(|p| {
                    outcomes.push(Outcome::Failed {
                        condition: p.key,
                        stage: Stage::Send,
                        error: format!("{err:#}"),
                    });
                    updates.push((p.key, StateUpdate::Failed));
                }),
            }
        }
    }

    /// Simulate an instruction body with a generous budget, returning the
    /// compute limit to actually request. `None` means it did not simulate.
    async fn measure(&self, body: &[Instruction]) -> Option<u32> {
        let probe = self.with_compute_budget(body.to_vec(), MAX_COMPUTE_UNITS, 0);
        let (tx, _) = self.signed_tx(&probe).await.ok()?;
        let sim = self.source.simulate_transaction(&tx, &[]).await.ok()?;
        sim.err.is_none().then(|| compute_limit(sim.units_consumed))
    }

    /// Fall-back path: one verified crank, on its own.
    async fn submit_single(
        &self,
        single: &Prepared,
        outcomes: &mut Vec<Outcome>,
        updates: &mut Vec<(CondKey, StateUpdate)>,
    ) {
        let body: Vec<Instruction> = single.ixs.clone();
        let units = self
            .measure(&body)
            .await
            .unwrap_or_else(|| compute_limit(single.units));
        let ixs = self.with_compute_budget(body, units, self.priority_fee());
        match self.sign_for_submission(&ixs).await {
            Ok((tx, expiry)) => {
                self.finish_pack(std::slice::from_ref(single), tx, expiry, outcomes, updates)
                    .await
            }
            Err(err) => {
                outcomes.push(Outcome::Failed {
                    condition: single.key,
                    stage: Stage::Send,
                    error: format!("{err:#}"),
                });
                updates.push((single.key, StateUpdate::Failed));
            }
        }
    }

    /// Submit one built transaction and record an outcome per member.
    async fn finish_pack(
        &self,
        pack: &[Prepared],
        tx: Transaction,
        expiry: u64,
        outcomes: &mut Vec<Outcome>,
        updates: &mut Vec<(CondKey, StateUpdate)>,
    ) {
        // One program per pack, by construction in `pack`, so the outcome
        // is booked against the program that actually earned or lost it.
        let program = pack[0].program;
        // Saturating: `min_payment` is a number a target program chose, and
        // an absurd one must not panic the turner on the way past.
        let payment: u64 = pack
            .iter()
            .fold(0u64, |sum, p| sum.saturating_add(p.min_payment));
        match self.submit(tx, program, payment, expiry).await {
            Ok(signature) => {
                metrics::PACKS
                    .with_label_values(&[if pack.len() > 1 { "packed" } else { "single" }])
                    .inc();
                pack.iter().for_each(|p| {
                    metrics::CRANKS
                        .with_label_values(&["sent", &metrics::program_label(&p.program)])
                        .inc();
                    outcomes.push(Outcome::Sent {
                        condition: p.key,
                        signature,
                        min_payment: p.min_payment,
                    });
                    updates.push((p.key, StateUpdate::Sent));
                });
            }
            Err(err) => pack.iter().for_each(|p| {
                outcomes.push(Outcome::Failed {
                    condition: p.key,
                    stage: Stage::Send,
                    error: format!("{err:#}"),
                });
                updates.push((p.key, StateUpdate::Failed));
            }),
        }
    }

    /// Split verified cranks into transaction-sized groups: bounded by
    /// `max_cranks_per_tx` and by the packet limit, measured by actually
    /// serializing rather than estimating.
    ///
    /// Cranks are grouped by target program first, and a pack never mixes
    /// two. A transaction has one outcome, and the submitter books that
    /// outcome — the payment earned, the fee burned, the contention delay it
    /// ramps — against a single program: mixing them credits one protocol's
    /// earnings and reverts to whichever member happened to be first out of
    /// the concurrent phase, which is what both the profitability floor and
    /// the adaptive delay steer on.
    async fn pack(&self, prepared: Vec<Prepared>) -> Vec<Vec<Prepared>> {
        let max = self.config.max_cranks_per_tx.max(1);
        let mut packs: Vec<Vec<Prepared>> = Vec::new();
        for group in group_by_program(prepared) {
            let mut current: Vec<Prepared> = Vec::new();
            for crank in group {
                if current.len() >= max {
                    packs.push(std::mem::take(&mut current));
                }
                current.push(crank);
                if current.len() > 1 && !self.fits(&current).await {
                    // Over the limit with the newest member: close the pack
                    // without it and start the next one with it.
                    let overflow = current.pop().expect("just pushed");
                    packs.push(std::mem::take(&mut current));
                    current.push(overflow);
                }
            }
            if !current.is_empty() {
                packs.push(current);
            }
        }
        packs
    }

    /// Exact size check: build and serialize, no estimating. Signed with
    /// the worst-case compute-budget values so the measurement bounds the
    /// real transaction.
    async fn fits(&self, pack: &[Prepared]) -> bool {
        let ixs = self.with_compute_budget(
            pack.iter().flat_map(|p| p.ixs.iter().cloned()).collect(),
            MAX_COMPUTE_UNITS,
            u64::MAX,
        );
        match self.signed_tx(&ixs).await {
            Ok((tx, _)) => bincode::serialize(&tx)
                .map(|bytes| bytes.len() <= MAX_TRANSACTION_BYTES)
                .unwrap_or(false),
            Err(_) => false,
        }
    }

    /// Hand a signed transaction to the submitter, or send it inline when
    /// none is attached. Either way the signature is already known — the
    /// turner signed it — so nothing here waits on the cluster.
    async fn submit(
        &self,
        tx: Transaction,
        program: Pubkey,
        expected_payment: u64,
        last_valid_block_height: u64,
    ) -> Result<Signature> {
        let signature = tx.signatures[0];
        match &self.submitter {
            Some(submitter) => {
                submitter
                    .outbox
                    .send(PendingTx {
                        transaction: tx,
                        signature,
                        program,
                        expected_payment,
                        last_valid_block_height,
                    })
                    .map_err(|_| anyhow::anyhow!("submitter stopped"))?;
                Ok(signature)
            }
            None => self.source.send_transaction(&tx).await,
        }
    }

    /// Bracket an executor with the payment guards (or pass it through
    /// untouched when guarding is disabled).
    fn guarded(
        &self,
        executor: Instruction,
        payout: Pubkey,
        program: &Pubkey,
        min_payment: u64,
    ) -> Vec<Instruction> {
        // Trusted programs skip the guards entirely: two fewer
        // instructions, their compute, and their bytes.
        if !self.config.guard_payments || self.trusts(program) {
            return vec![executor];
        }
        let payer = self.keeper.pubkey();
        let nonce = self.config.guard_nonce;
        let guard = self.guard_address(payout, nonce);
        vec![
            Instruction {
                program_id: self.config.relay_program,
                accounts: vec![
                    AccountMeta::new(payer, true),
                    AccountMeta::new_readonly(payout, false),
                    AccountMeta::new(guard, false),
                    AccountMeta::new_readonly(SYSTEM_PROGRAM_ID, false),
                ],
                data: spec::encode_begin_guard_v0_data(nonce).to_vec(),
            },
            executor,
            Instruction {
                program_id: self.config.relay_program,
                accounts: vec![
                    AccountMeta::new_readonly(payout, false),
                    AccountMeta::new(guard, false),
                ],
                data: spec::encode_assert_paid_v0_data(min_payment, nonce).to_vec(),
            },
        ]
    }

    /// Roll a wrapped-SOL payout's accumulated lamports into its token
    /// balance, as a standalone transaction.
    ///
    /// Deliberately *not* part of a crank. Payment arrives as lamports and
    /// the guard measures lamports, so nothing about correctness or safety
    /// waits on this — it only decides when the proceeds become spendable
    /// as tokens. Bundling it into every crank would buy an instruction,
    /// its compute, and its bytes on every single transaction to keep a
    /// number fresh that nobody reads in between. Call it on a timer.
    pub async fn sync_payout(&self) -> Result<Option<Signature>> {
        let Some(payout) = self.config.payout else {
            return Ok(None);
        };
        if !self.config.sync_native_payout {
            return Ok(None);
        }
        let sync = Instruction {
            program_id: *TOKEN_PROGRAM_ID,
            accounts: vec![AccountMeta::new(payout, false)],
            data: vec![SYNC_NATIVE_TAG],
        };
        let units = self
            .measure(std::slice::from_ref(&sync))
            .await
            .unwrap_or(10_000);
        let ixs = self.with_compute_budget(vec![sync], units, self.priority_fee());
        let (tx, expiry) = self.sign_for_submission(&ixs).await?;
        let signature = self.submit(tx, *TOKEN_PROGRAM_ID, 0, expiry).await?;
        Ok(Some(signature))
    }

    /// A payout account's guard PDA for a given nonce.
    pub fn guard_address(&self, payout: Pubkey, nonce: u8) -> Pubkey {
        Pubkey::find_program_address(
            &[spec::GUARD_SEED, payout.as_ref(), &[nonce]],
            &self.config.relay_program,
        )
        .0
    }

    /// Prepend compute-budget instructions: an explicit unit limit (fees
    /// are billed on the *requested* limit, so the default 200k×ixs is
    /// both wasteful and, for a multi-crank transaction, too small) and a
    /// priority fee.
    ///
    /// The loaded-accounts data size limit goes at the *end*. It is billed on
    /// the requested figure for the same reason as the unit limit, and its
    /// default is 64 MiB — but the runtime finds compute-budget instructions
    /// by program id wherever they sit, while an instruction added at the
    /// front shifts every index behind it. A guard triple is addressed by
    /// position within the pack, so the tail is the safe end to grow.
    fn with_compute_budget(
        &self,
        ixs: Vec<Instruction>,
        units: u32,
        price: u64,
    ) -> Vec<Instruction> {
        [
            ComputeBudgetInstruction::set_compute_unit_limit(units),
            ComputeBudgetInstruction::set_compute_unit_price(price),
        ]
        .into_iter()
        .chain(ixs)
        .chain(std::iter::once(
            ComputeBudgetInstruction::set_loaded_accounts_data_size_limit(
                self.config.loaded_accounts_data_size,
            ),
        ))
        .collect()
    }

    /// Slots to hold a program's cranks back by. Zero without a submitter,
    /// which is also the no-feedback case: nothing has observed a lost race.
    fn contention_delay(&self, program: &Pubkey) -> u64 {
        self.submitter
            .as_ref()
            .map_or(0, |submitter| submitter.lag_for(program))
    }

    /// Is this program exempt from guards and payout separation?
    fn trusts(&self, program: &Pubkey) -> bool {
        self.config.trusted_programs.contains(program)
    }

    /// Where a given program's executor should pay.
    ///
    /// Trusted programs may pay the fee payer directly. Untrusted ones
    /// must pay a configured account that never signs — otherwise there is
    /// no safe answer and the condition is skipped.
    fn payout_for(&self, program: &Pubkey) -> Option<Pubkey> {
        match self.config.payout {
            Some(payout) => Some(payout),
            None => self.trusts(program).then(|| self.keeper.pubkey()),
        }
    }

    /// Priority fee the submitter observed, clamped to the configured
    /// ceiling. Zero when no submitter is attached.
    fn priority_fee(&self) -> u64 {
        self.submitter
            .as_ref()
            .map(|s| s.priority_fee())
            .unwrap_or(0)
            .min(self.config.max_priority_fee)
    }

    /// The authoritative safety gate, run on the exact instruction list
    /// about to be signed and sent.
    ///
    /// A malicious executor is harmless as long as it never receives an
    /// account that signs the transaction — so rather than reason about
    /// where the account list came from, search the finished list. Returns
    /// the offending program if any instruction that is not ours, and not
    /// trusted, names the fee payer. Public so an operator embedding the
    /// turner can pre-flight their own instruction lists with the same
    /// rule the turner enforces.
    ///
    /// This sits at signing time on purpose. The same check runs earlier
    /// for a clean skip reason, but instructions are rebuilt, re-priced
    /// and concatenated into packs after that point; putting the binding
    /// check here means no later transformation can reintroduce a leak.
    pub fn signer_leak(&self, ixs: &[Instruction]) -> Option<Pubkey> {
        // Compile to learn which accounts this transaction will actually
        // present as signers, rather than assuming it is just the payer.
        let message = Message::new(ixs, Some(&self.keeper.pubkey()));
        let signers = &message.account_keys[..message.header.num_required_signatures as usize];
        ixs.iter()
            .filter(|ix| {
                !is_own_guard(ix, &self.config.relay_program)
                    && ix.program_id != *COMPUTE_BUDGET_PROGRAM_ID
                    && !self.trusts(&ix.program_id)
            })
            .find(|ix| {
                // Two ways an executor can end up holding a signature.
                // First, asking for one outright — nothing legitimate
                // needs it, since executors are permissionless.
                ix.accounts.iter().any(|meta| meta.is_signer)
                    // Second, and the one that actually bites: naming an
                    // account that signs the transaction for another
                    // reason. `is_signer: false` on the meta does not help
                    // there — see `hostile_drain_succeeds_with_is_signer_false`.
                    || names_transaction_signer(ix, signers)
            })
            .map(|ix| ix.program_id)
    }

    /// Sign a transaction that is about to be submitted, refusing to sign
    /// one that would hand a signing account to an untrusted program.
    async fn sign_for_submission(&self, ixs: &[Instruction]) -> Result<(Transaction, u64)> {
        if let Some(program) = self.signer_leak(ixs) {
            metrics::FAILURES
                .with_label_values(&["signer_leak", &metrics::program_label(&program)])
                .inc();
            anyhow::bail!(
                "refusing to sign: untrusted program {program} would receive the fee payer, \
                 which signs this transaction"
            );
        }
        self.signed_tx(ixs).await
    }

    /// Sign against the shared blockhash the submitter keeps refreshed,
    /// falling back to a direct fetch when running without one. Returns
    /// the block height past which the transaction can no longer land.
    async fn signed_tx(&self, ixs: &[Instruction]) -> Result<(Transaction, u64)> {
        let info = match self.submitter.as_ref().and_then(|s| s.cached_blockhash()) {
            Some(info) => info,
            None => self.source.latest_blockhash().await?,
        };
        Ok((
            Transaction::new_signed_with_payer(
                ixs,
                Some(&self.keeper.pubkey()),
                &[&self.keeper],
                info.hash,
            ),
            info.last_valid_block_height,
        ))
    }

    async fn load_map(&self, pubkeys: &[Pubkey]) -> Result<HashMap<Pubkey, Account>> {
        if pubkeys.is_empty() {
            return Ok(HashMap::new());
        }
        let accounts = self.source.get_multiple_accounts(pubkeys).await?;
        Ok(pubkeys
            .iter()
            .zip(accounts)
            .filter_map(|(pk, acc)| acc.map(|a| (*pk, a)))
            .collect())
    }
}

/// Split cranks into one group per target program, keeping the order they
/// arrived in within each group. See [`Turner::pack`] for why a transaction
/// must not mix two.
fn group_by_program(prepared: Vec<Prepared>) -> Vec<Vec<Prepared>> {
    prepared.into_iter().fold(Vec::new(), |mut groups, crank| {
        match groups
            .iter_mut()
            .find(|group: &&mut Vec<Prepared>| group[0].program == crank.program)
        {
            Some(group) => group.push(crank),
            None => groups.push(vec![crank]),
        }
        groups
    })
}

/// Is this one of the turner's own payment guards?
///
/// The guards are the one place the fee payer legitimately appears in an
/// instruction the turner did not write itself — `begin_guard_v0` funds the
/// guard account out of it — so they are exempt from the signer check. The
/// exemption is by *instruction*, keyed on the discriminator, not by
/// program: an executor's identity comes from a resolver, which is free to
/// name the relay program like any other, and a blanket
/// `program_id == relay_program` exemption would wave that straight through
/// to signing with the fee payer in its account list.
fn is_own_guard(ix: &Instruction, relay_program: &Pubkey) -> bool {
    ix.program_id == *relay_program
        && ix.data.first_chunk::<8>().is_some_and(|disc| {
            *disc == spec::BEGIN_GUARD_V0_DISCRIMINATOR
                || *disc == spec::ASSERT_PAID_V0_DISCRIMINATOR
        })
}

/// Does this instruction name any account from `signers`?
///
/// The distinction that matters: an executor **must** name the account it
/// pays, and that is fine — a non-signing account can only be credited.
/// What it must never name is an account that signs the transaction,
/// because signer status on Solana is transaction-global. A compiled
/// message has no per-instruction signer flag at all: `is_signer` is
/// decided by an account's position in the message's signer section, so
/// setting `AccountMeta { is_signer: false }` on an account that signs
/// elsewhere in the transaction demotes nothing. The executor receives it
/// as a signer and can CPI a System transfer to drain it.
///
/// So the rule is not "don't name the payee", it is "don't name a
/// signer" — which, since the turner's only signer is its fee payer, is
/// why payment goes to a separate non-signing payout account.
pub fn names_transaction_signer(ix: &Instruction, signers: &[Pubkey]) -> bool {
    ix.accounts
        .iter()
        .any(|meta| signers.contains(&meta.pubkey))
}

/// Executors must name the payout placeholder somewhere, or there is
/// nothing to be paid into and the guard would assert against a
/// stranger's balance.
fn require_keeper_placeholder(resolved: &spec::ResolvedCrankV0) -> Result<()> {
    resolved
        .accounts
        .iter()
        .any(|a| a.address == spec::KEEPER_PLACEHOLDER)
        .then_some(())
        .context("resolver output names no keeper placeholder")
}

/// A crank that resolved to real work and passed its own simulation,
/// waiting to be packed into a transaction.
struct Prepared {
    key: CondKey,
    program: Pubkey,
    min_payment: u64,
    /// Compute units this crank alone consumed in simulation.
    units: u64,
    /// `[begin_guard, executor, assert_paid]`, without compute budget.
    ixs: Vec<Instruction>,
}

/// What the concurrent phase produced for one condition.
enum CrankResult {
    /// Verified and ready to submit.
    Ready(Box<Prepared>),
    /// Finished without submitting (no work, or a failure).
    Done(Outcome, StateUpdate),
}

/// A step-by-step account of what the turner decided about one condition.
#[derive(Debug, Clone)]
pub struct Explanation {
    pub key: CondKey,
    pub program: Pubkey,
    /// Who registered the watch this condition rides on.
    pub creator: Pubkey,
    pub condition: spec::ConditionV0,
    pub clock: ClockSnapshot,
    /// The change-wake's watched bytes as they read now. `None` for wakes
    /// that are not change-based.
    pub watched_now: Option<Vec<u8>>,
    /// This turner has no tick history, so change-wakes read as due and
    /// per-process suppression cannot be reported.
    pub stateless: bool,
    pub verdict: Verdict,
}

/// Where a condition got to.
#[derive(Debug, Clone)]
pub enum Verdict {
    /// Never attempted, and why.
    Skipped(SkipReason),
    /// Attempted; the resolver reported nothing to do.
    NoWork,
    /// Attempted; simulated clean and would be submitted.
    WouldSend {
        min_payment: u64,
        units: u64,
        instructions: Vec<Instruction>,
    },
    /// Attempted and failed.
    Failed { stage: Stage, error: String },
}

/// A condition whose wake came due, carried into the concurrent phase.
#[derive(Debug)]
struct Due {
    key: CondKey,
    program: Pubkey,
    condition: spec::ConditionV0,
    watched_now: Option<Vec<u8>>,
    /// The resolver's account list, materialized at decide time — inline
    /// refs copied out, or an indirect list read from the block's own
    /// account (`resolver_list_offset`), so downstream stages never need
    /// the account bytes again.
    resolver_accounts: Vec<spec::AccountRefV0>,
}

/// Materialize a condition's resolver account list: `num_resolver_accounts`
/// refs read from the block account's data at `resolver_list_offset`.
/// `None` when that region is unreadable (out of bounds / over the cap).
fn materialize_resolver_accounts(
    condition: &spec::ConditionV0,
    block_account_data: &[u8],
) -> Option<Vec<spec::AccountRefV0>> {
    let count = condition.resolvers().count as usize;
    if count > spec::MAX_INDIRECT_RESOLVER_ACCOUNTS {
        return None;
    }
    let start = condition.resolvers().offset as usize;
    let end = start.checked_add(count.checked_mul(spec::ACCOUNT_REF_LEN)?)?;
    let region = block_account_data.get(start..end)?;
    // Align-1 pod, packed: the region already *is* a `[AccountRefV0]`.
    Some(spec::bytemuck::cast_slice::<u8, spec::AccountRefV0>(region).to_vec())
}

/// Stable metric label for a skip reason. Spelled out rather than derived
/// so a rename in the enum cannot silently rename a dashboard series.
fn skip_label(reason: &SkipReason) -> &'static str {
    match reason {
        SkipReason::NotDue => "not_due",
        SkipReason::Backoff => "backoff",
        SkipReason::Inactive => "inactive",
        SkipReason::BelowMinPayment => "below_min_payment",
        SkipReason::ParseFailed => "parse_failed",
        SkipReason::Unprofitable => "unprofitable",
        SkipReason::ContentionDelay => "contention_delay",
        SkipReason::NoSafePayout => "no_safe_payout",
        SkipReason::ExecutorNamedSigner => "executor_named_signer",
    }
}

/// Stable metric label for a wake kind, for the load breakdown.
fn wake_label(condition: &spec::ConditionV0) -> &'static str {
    match condition.wake() {
        Ok(spec::WakeView::AtTimestamp { .. }) => "at_timestamp",
        Ok(spec::WakeView::AtSlot { .. }) => "at_slot",
        Ok(spec::WakeView::EverySlots { .. }) => "every_slots",
        Ok(spec::WakeView::OnAccountChange { .. }) => "on_account_change",
        Ok(spec::WakeView::OnValueCross { .. }) => "on_value_cross",
        Err(_) => "unknown",
    }
}

fn stage_label(stage: &Stage) -> &'static str {
    match stage {
        Stage::ResolveSim => "resolve_sim",
        Stage::ExecuteSim => "execute_sim",
        Stage::Send => "send",
    }
}

fn account_ref_meta(a: &spec::AccountRefV0) -> AccountMeta {
    AccountMeta {
        pubkey: Pubkey::from(a.address),
        is_signer: false,
        is_writable: a.is_writable(),
    }
}

/// Clamped slice, so a hostile/garbled offset can't panic the turner.
fn slice_or_empty(data: &[u8], offset: u32, len: u32) -> Vec<u8> {
    let start = (offset as usize).min(data.len());
    let end = (offset as usize)
        .saturating_add(len as usize)
        .min(data.len());
    data[start..end].to_vec()
}

/// Read a resolver's staged payload out of post-simulation account state.
fn read_staged(
    accounts: &[Option<Account>],
    pointer: &spec::ResponsePointerV0,
) -> Result<spec::ResolvedCrankV0> {
    let account = accounts
        .get(pointer.account_index as usize)
        .and_then(|maybe| maybe.as_ref())
        .with_context(|| {
            format!(
                "simulation returned no account at index {} (staging account must be in the \
                 resolver's account list)",
                pointer.account_index
            )
        })?;
    let start = pointer.offset() as usize;
    let end = start
        .checked_add(pointer.len() as usize)
        .context("staging range overflows")?;
    if end > account.data.len() {
        anyhow::bail!(
            "staging range {start}..{end} outside account data ({} bytes)",
            account.data.len()
        );
    }
    spec::ResolvedCrankV0::read(&account.data[start..end])
        .map_err(|e| anyhow::anyhow!("unparseable staged payload: {e:?}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn prepared(program: Pubkey, index: u8) -> Prepared {
        Prepared {
            key: (Pubkey::new_unique(), 0, index),
            program,
            min_payment: 1,
            units: 1,
            ixs: Vec::new(),
        }
    }

    /// A transaction has one outcome and the submitter books it against one
    /// program, so cranks from two programs must never share one — however
    /// they interleave coming out of the concurrent phase.
    #[test]
    fn cranks_are_grouped_by_program_before_packing() {
        let (a, b) = (Pubkey::new_unique(), Pubkey::new_unique());
        let interleaved = vec![
            prepared(a, 0),
            prepared(b, 1),
            prepared(a, 2),
            prepared(b, 3),
            prepared(a, 4),
        ];
        let groups = group_by_program(interleaved);
        assert_eq!(groups.len(), 2);
        assert!(groups
            .iter()
            .all(|group| group.iter().all(|p| p.program == group[0].program)));
        // Order within a program is the order they finished in.
        assert_eq!(
            groups[0].iter().map(|p| p.key.2).collect::<Vec<_>>(),
            vec![0, 2, 4]
        );
        assert_eq!(
            groups[1].iter().map(|p| p.key.2).collect::<Vec<_>>(),
            vec![1, 3]
        );
    }

    /// The guard exemption in `signer_leak` is keyed on the instruction, not
    /// the program: a resolver may name relay as an executor like anything
    /// else, and `begin_guard_v0` spends the fee payer's lamports.
    #[test]
    fn only_the_guard_instructions_are_exempt() {
        let relay = Pubkey::new_unique();
        let guard = |data: Vec<u8>| Instruction {
            program_id: relay,
            accounts: Vec::new(),
            data,
        };
        assert!(is_own_guard(
            &guard(spec::encode_begin_guard_v0_data(3).to_vec()),
            &relay
        ));
        assert!(is_own_guard(
            &guard(spec::encode_assert_paid_v0_data(5, 3).to_vec()),
            &relay
        ));
        assert!(!is_own_guard(&guard(vec![9; 8]), &relay));
        assert!(!is_own_guard(&guard(Vec::new()), &relay));
        assert!(!is_own_guard(
            &guard(spec::encode_begin_guard_v0_data(3).to_vec()),
            &Pubkey::new_unique()
        ));
    }
}
