//! What an operator configures, and the limits the runtime fixes.

use std::collections::HashSet;

use solana_sdk::pubkey::Pubkey;

use crate::filter::WatchFilter;

#[derive(Debug, Clone)]
pub struct TurnerConfig {
    /// The relay program (watch registry + `crank_v0`).
    pub relay_program: Pubkey,
    /// Skip conditions advertising less than this (the original tuktuk
    /// `min_crank_fee` pattern — a fleet of turners self-selects by fee).
    pub min_crank_payment: u64,
    /// On top of the fee, what a crank must clear for this turner to land it.
    /// Zero accepts break-even work.
    pub crank_margin_lamports: u64,
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
    /// balance via [`super::Turner::sync_payout`].
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

impl Default for TurnerConfig {
    fn default() -> Self {
        Self {
            relay_program: RELAY_PROGRAM_ID.parse().unwrap(),
            min_crank_payment: 0,
            crank_margin_lamports: 0,
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

/// Simulation probe budget: the per-transaction ceiling, so the probe is
/// never the thing that fails.
pub(super) const MAX_COMPUTE_UNITS: u32 = 1_400_000;

/// Solana's packet limit.
pub(super) const MAX_TRANSACTION_BYTES: usize = 1232;

/// SPL Token, and its `SyncNative` instruction tag. Hand-rolled rather
/// than pulling in the token crate for one 1-byte instruction.
pub(super) static TOKEN_PROGRAM_ID: std::sync::LazyLock<Pubkey> = std::sync::LazyLock::new(|| {
    "TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA"
        .parse()
        .expect("valid program id")
});

pub(super) const SYNC_NATIVE_TAG: u8 = 17;

/// `ComputeBudget111111111111111111111111111111`.
pub(super) static COMPUTE_BUDGET_PROGRAM_ID: std::sync::LazyLock<Pubkey> =
    std::sync::LazyLock::new(|| {
        "ComputeBudget111111111111111111111111111111"
            .parse()
            .expect("valid program id")
    });

/// System program id (the all-zero address), required by
/// `begin_guard_v0`'s lazy guard-account creation.
pub(super) const SYSTEM_PROGRAM_ID: Pubkey = Pubkey::new_from_array([0u8; 32]);

/// Default relay program id (`declare_id!` in `programs/relay`).
pub const RELAY_PROGRAM_ID: &str = "4D5tPhw9sqkdkR5CpmP427TH6y9p9AMuKUukUEHn3Mpu";
