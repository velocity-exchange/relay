//! Transaction submission as its own subsystem, fed by a channel.
//!
//! Keeping this off the decision loop is the point: send, confirm, and
//! resend all involve RPC round trips, and a turner that awaited them
//! inline would stop evaluating conditions while a transaction settles.
//! The turner signs (so it knows the signature immediately), hands the
//! transaction over, and moves on.
//!
//! Lifted from tuktuk's sender, which learned these the hard way:
//!
//! - **One shared blockhash**, refreshed on a timer and published over a
//!   watch channel, instead of a `getLatestBlockhash` per transaction.
//! - **Track unconfirmed signatures** and poll `getSignatureStatuses` in
//!   batches rather than awaiting each send.
//! - **Resend while the blockhash is still valid**, then give up with a
//!   distinct outcome so the caller retries immediately rather than
//!   counting it as a real failure. Re-signing is deliberately not done
//!   here: the keypair lives with the turner, and an `Expired` outcome puts
//!   the condition straight back in front of it.
//! - **Classify failures.** "The blockhash expired" and "the executor
//!   underpaid" deserve different reactions.

use std::collections::HashMap;
use std::time::Duration;

use solana_sdk::pubkey::Pubkey;
use solana_sdk::signature::Signature;
use solana_sdk::transaction::Transaction;
use tokio::sync::{mpsc, watch};
use tracing::{debug, info, warn};

use crate::metrics;
use relay_chain_source::{BlockhashInfo, ChainSource, SignatureOutcome};

/// How a submitted transaction ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TxResult {
    /// Landed and succeeded.
    Landed,
    /// Landed and the runtime rejected it (guard tripped, executor failed,
    /// state moved). The turner should back this condition off.
    Failed(String),
    /// Never landed before its blockhash expired, after exhausting
    /// re-signs. Not the condition's fault — retry promptly.
    Expired,
}

/// A signed transaction plus what the submitter needs to account for it.
#[derive(Debug, Clone)]
pub struct PendingTx {
    pub transaction: Transaction,
    pub signature: Signature,
    /// Target program, for per-program metrics and profitability.
    pub program: Pubkey,
    /// What the crank is expected to earn, for profitability accounting.
    pub expected_payment: u64,
    pub last_valid_block_height: u64,
}

#[derive(Debug, Clone)]
pub struct SubmitterConfig {
    /// How often to refresh the shared blockhash.
    pub blockhash_refresh: Duration,
    /// How often to poll for confirmations.
    pub confirm_interval: Duration,
    /// How many times to rebroadcast a transaction that is unconfirmed but
    /// whose blockhash can still land. Expiry is decided by block height,
    /// not by this — a transaction that runs out of rebroadcasts is still
    /// tracked until it confirms or its blockhash dies.
    pub max_resends: u32,
    /// Rolling window length for per-program profitability.
    pub profit_window: usize,
    /// How often to sample recent prioritization fees.
    pub fee_refresh: Duration,
    /// Slots added to a program's contention delay per reverted
    /// transaction. Zero disables adaptive delay entirely.
    pub contention_step_slots: u64,
    /// Ceiling on the contention delay, in slots.
    pub max_contention_slots: u64,
}

impl Default for SubmitterConfig {
    fn default() -> Self {
        Self {
            blockhash_refresh: Duration::from_secs(2),
            confirm_interval: Duration::from_secs(2),
            max_resends: 3,
            profit_window: 20,
            fee_refresh: Duration::from_secs(5),
            // ~1.6s per revert, up to ~5s. Enough to let a competitor's
            // transaction land and be visible to the next simulation,
            // while staying well inside the slack on the wakes this is
            // meant for (expiries, evictions) rather than the ones where
            // being first is the whole point.
            contention_step_slots: 4,
            max_contention_slots: 12,
        }
    }
}

/// Rolling net lamports per target program, published for the turner to
/// gate on. A program whose cranks keep costing more than they pay gets
/// deprioritized instead of retried forever — the tuktuk profitability
/// lesson, which keeps a fleet of independent turners from all grinding on
/// the same loss-making work.
pub type ProfitSnapshot = HashMap<Pubkey, i64>;

/// How many slots to hold back before cranking each program, published for
/// the turner to apply.
///
/// This is the adaptive half of the profitability lesson, and the more
/// useful one in a competitive fleet. A turner that is simply slower than a
/// rival loses every race, and each loss costs a transaction fee for a
/// reverted crank — it does the work, arrives second, and pays for the
/// privilege. Delaying deliberately converts that into a free no-op: by the
/// time the turner resolves and simulates, the rival's crank has landed, the
/// resolver reports nothing to do, and no transaction is ever sent.
///
/// It is self-healing in the direction that matters. If the rival stops —
/// crashes, is turned off, runs out of funds — the delayed turner's
/// simulation starts finding real work again, its cranks land, and the delay
/// decays back toward zero. The cost of a competitor disappearing is that
/// cranks run a few seconds late until it does.
pub type LagSnapshot = HashMap<Pubkey, u64>;

/// Handle the turner holds: an outbox plus two read-only views.
#[derive(Clone)]
pub struct SubmitterHandle {
    pub outbox: mpsc::UnboundedSender<PendingTx>,
    pub blockhash: watch::Receiver<Option<BlockhashInfo>>,
    pub profit: watch::Receiver<ProfitSnapshot>,
    /// Per-program contention delay, in slots.
    pub lag: watch::Receiver<LagSnapshot>,
    /// Recently observed prioritization fee, micro-lamports per CU.
    pub priority_fee: watch::Receiver<u64>,
}

impl SubmitterHandle {
    /// Latest cached blockhash, if the refresher has produced one.
    pub fn cached_blockhash(&self) -> Option<BlockhashInfo> {
        self.blockhash.borrow().clone()
    }

    /// Rolling net lamports for a program (negative = losing money).
    pub fn profit_for(&self, program: &Pubkey) -> i64 {
        self.profit.borrow().get(program).copied().unwrap_or(0)
    }

    /// Slots to hold a program's cranks back by, to avoid paying for
    /// races it keeps losing. Zero when it is winning or uncontested.
    pub fn lag_for(&self, program: &Pubkey) -> u64 {
        self.lag.borrow().get(program).copied().unwrap_or(0)
    }

    /// Latest observed priority fee, micro-lamports per compute unit.
    pub fn priority_fee(&self) -> u64 {
        *self.priority_fee.borrow()
    }
}

struct Tracked {
    pending: PendingTx,
    resends: u32,
    /// When it was handed to the cluster, for confirmation latency.
    submitted: std::time::Instant,
}

/// Spawn the submitter: a blockhash refresher, and a send/confirm loop.
pub fn spawn<S: ChainSource + ?Sized + 'static>(
    source: std::sync::Arc<S>,
    config: SubmitterConfig,
) -> SubmitterHandle {
    let (outbox, inbox) = mpsc::unbounded_channel();
    let (blockhash_tx, blockhash_rx) = watch::channel(None);
    let (profit_tx, profit_rx) = watch::channel(ProfitSnapshot::new());
    let (lag_tx, lag_rx) = watch::channel(LagSnapshot::new());
    let (fee_tx, fee_rx) = watch::channel(0);

    tokio::spawn(refresh_blockhash(
        std::sync::Arc::clone(&source),
        config.blockhash_refresh,
        blockhash_tx,
    ));
    tokio::spawn(refresh_priority_fee(
        std::sync::Arc::clone(&source),
        config.fee_refresh,
        fee_tx,
    ));
    tokio::spawn(run(source, config, inbox, profit_tx, lag_tx));

    SubmitterHandle {
        outbox,
        blockhash: blockhash_rx,
        profit: profit_rx,
        lag: lag_rx,
        priority_fee: fee_rx,
    }
}

async fn refresh_priority_fee<S: ChainSource + ?Sized>(
    source: std::sync::Arc<S>,
    every: Duration,
    tx: watch::Sender<u64>,
) {
    let mut interval = tokio::time::interval(every);
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        interval.tick().await;
        match source.recent_priority_fee(&[]).await {
            Ok(fee) => {
                let _ = tx.send(fee);
            }
            Err(err) => debug!(error = %format!("{err:#}"), "priority fee sample failed"),
        }
    }
}

async fn refresh_blockhash<S: ChainSource + ?Sized>(
    source: std::sync::Arc<S>,
    every: Duration,
    tx: watch::Sender<Option<BlockhashInfo>>,
) {
    let mut interval = tokio::time::interval(every);
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        interval.tick().await;
        match source.latest_blockhash().await {
            Ok(info) => {
                let _ = tx.send(Some(info));
            }
            Err(err) => warn!(error = %format!("{err:#}"), "blockhash refresh failed"),
        }
    }
}

async fn run<S: ChainSource + ?Sized>(
    source: std::sync::Arc<S>,
    config: SubmitterConfig,
    mut inbox: mpsc::UnboundedReceiver<PendingTx>,
    profit: watch::Sender<ProfitSnapshot>,
    lag: watch::Sender<LagSnapshot>,
) {
    let mut tracked: HashMap<Signature, Tracked> = HashMap::new();
    let mut history: HashMap<Pubkey, Vec<i64>> = HashMap::new();
    let mut confirm = tokio::time::interval(config.confirm_interval);
    confirm.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

    loop {
        tokio::select! {
            incoming = inbox.recv() => match incoming {
                Some(pending) => {
                    if let Err(err) = source.send_transaction(&pending.transaction).await {
                        // A failed send is not a lost transaction: it may
                        // still have reached the cluster, so keep tracking.
                        warn!(signature = %pending.signature, error = %format!("{err:#}"), "send failed");
                    }
                    metrics::IN_FLIGHT.with_label_values(&["transactions"]).inc();
                    tracked.insert(
                        pending.signature,
                        Tracked {
                            pending,
                            resends: 0,
                            submitted: std::time::Instant::now(),
                        },
                    );
                }
                None => {
                    debug!("submitter outbox closed");
                    return;
                }
            },
            _ = confirm.tick() => {
                sweep(&source, &config, &mut tracked, &mut history, &profit, &lag).await;
            }
        }
    }
}

/// One confirmation pass: settle what landed, resend what is still valid,
/// expire what is not.
async fn sweep<S: ChainSource + ?Sized>(
    source: &S,
    config: &SubmitterConfig,
    tracked: &mut HashMap<Signature, Tracked>,
    history: &mut HashMap<Pubkey, Vec<i64>>,
    profit: &watch::Sender<ProfitSnapshot>,
    lag: &watch::Sender<LagSnapshot>,
) {
    if tracked.is_empty() {
        return;
    }
    let signatures: Vec<Signature> = tracked.keys().copied().collect();
    let statuses = match source.signature_statuses(&signatures).await {
        Ok(statuses) => statuses,
        Err(err) => {
            warn!(error = %format!("{err:#}"), "signature status poll failed");
            return;
        }
    };
    let block_height = match source.block_height().await {
        Ok(height) => Some(height),
        Err(err) => {
            // Without a height nothing can be declared expired; rebroadcast
            // anyway, which is the harmless half.
            warn!(error = %format!("{err:#}"), "block height poll failed");
            None
        }
    };

    let settled: Vec<(Signature, TxResult)> = signatures
        .iter()
        .zip(statuses)
        .filter_map(|(signature, status)| match status {
            Some(SignatureOutcome::Landed) => Some((*signature, TxResult::Landed)),
            Some(SignatureOutcome::Failed(err)) => Some((*signature, TxResult::Failed(err))),
            None => None,
        })
        .collect();

    settled.iter().for_each(|(signature, result)| {
        if let Some(entry) = tracked.remove(signature) {
            metrics::CONFIRM_SECONDS
                .with_label_values(&[result_label(result)])
                .observe(entry.submitted.elapsed().as_secs_f64());
            record(&entry.pending, result, history, profit, lag, config);
        }
    });

    // Past its last valid block height a transaction can never land, and no
    // amount of resending changes that. Retire it under a distinct outcome
    // so the caller retries promptly rather than treating it as the
    // condition's fault.
    let dead: Vec<Signature> = block_height
        .map(|height| {
            tracked
                .iter()
                .filter(|(_, entry)| entry.pending.last_valid_block_height < height)
                .map(|(signature, _)| *signature)
                .collect()
        })
        .unwrap_or_default();
    for signature in dead {
        let Some(entry) = tracked.remove(&signature) else {
            continue;
        };
        metrics::CONFIRM_SECONDS
            .with_label_values(&["expired"])
            .observe(entry.submitted.elapsed().as_secs_f64());
        record(
            &entry.pending,
            &TxResult::Expired,
            history,
            profit,
            lag,
            config,
        );
    }

    // What is left is unconfirmed but still landable — the only state a
    // resend can help. Rebroadcasting the same signed bytes is idempotent
    // (identical signature, deduped by the cluster); the bound is there so
    // a transaction nobody will ever include is not shouted about for the
    // whole life of its blockhash.
    for (signature, entry) in tracked
        .iter_mut()
        .filter(|(_, entry)| entry.resends < config.max_resends)
    {
        entry.resends += 1;
        if let Err(err) = source.send_transaction(&entry.pending.transaction).await {
            warn!(%signature, error = %format!("{err:#}"), "resend failed");
        }
    }
}

fn result_label(result: &TxResult) -> &'static str {
    match result {
        TxResult::Landed => "landed",
        TxResult::Failed(_) => "failed",
        TxResult::Expired => "expired",
    }
}

/// The contention delay a program should carry after one transaction
/// outcome: additive increase, multiplicative decay.
///
/// A reverted transaction is the signal, not overall profit. It is the one
/// outcome that means a fee was burned for nothing, and the two things that
/// cause it — a rival landed the same crank first, or the target's state
/// moved out from under a simulation that had already passed — have the
/// same fix: arrive later, and let simulation catch it for free next time.
///
/// Increase is additive so the delay converges on the smallest one that
/// actually clears the rival, rather than overshooting to the ceiling on
/// the first loss. Decay is multiplicative so recovery takes a handful of
/// wins instead of a single one — snapping straight back to zero after one
/// landed crank would just pay for another revert on the next contested
/// one, and oscillate.
fn next_delay(current: u64, result: &TxResult, step: u64, max: u64) -> u64 {
    match result {
        TxResult::Failed(_) => current.saturating_add(step).min(max),
        TxResult::Landed => current / 2,
        // Never landed, so no fee was burned and no rival took the work.
        // That is congestion, and waiting longer does not help it.
        TxResult::Expired => current,
    }
}

fn record(
    pending: &PendingTx,
    result: &TxResult,
    history: &mut HashMap<Pubkey, Vec<i64>>,
    profit: &watch::Sender<ProfitSnapshot>,
    lag: &watch::Sender<LagSnapshot>,
    config: &SubmitterConfig,
) {
    metrics::IN_FLIGHT
        .with_label_values(&["transactions"])
        .dec();
    let program = metrics::program_label(&pending.program);
    metrics::TRANSACTIONS
        .with_label_values(&[result_label(result)])
        .inc();
    if let TxResult::Failed(err) = result {
        info!(signature = %pending.signature, error = %err, "transaction failed on chain");
    }

    // Profit accounting: a landed crank earns its payment, anything else
    // just burned a fee. Both are approximations good enough to steer with.
    let delta = match result {
        TxResult::Landed => {
            metrics::LAMPORTS
                .with_label_values(&["earned", &program])
                .inc_by(pending.expected_payment);
            pending.expected_payment as i64
        }
        _ => {
            metrics::LAMPORTS
                .with_label_values(&["spent", &program])
                .inc_by(5000);
            -5000
        }
    };
    // Adaptive contention delay, from the same outcome.
    if config.contention_step_slots > 0 {
        lag.send_modify(|snapshot| {
            let slots = snapshot.entry(pending.program).or_insert(0);
            let next = next_delay(
                *slots,
                result,
                config.contention_step_slots,
                config.max_contention_slots,
            );
            if next != *slots {
                debug!(
                    program = %pending.program,
                    from = *slots,
                    to = next,
                    "contention delay adjusted"
                );
            }
            *slots = next;
            metrics::CONTENTION_DELAY
                .with_label_values(&[&program])
                .set(next as i64);
        });
    }

    let entries = history.entry(pending.program).or_default();
    entries.push(delta);
    if entries.len() > config.profit_window {
        entries.remove(0);
    }
    let net: i64 = entries.iter().sum();
    profit.send_modify(|snapshot| {
        snapshot.insert(pending.program, net);
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn failed() -> TxResult {
        TxResult::Failed("custom program error: 0x1771".into())
    }

    /// A source that confirms nothing, counts rebroadcasts, and reports
    /// whatever block height the test sets.
    struct Recorder {
        height: std::sync::Mutex<u64>,
        sends: std::sync::Mutex<Vec<Signature>>,
    }

    impl Recorder {
        fn new(height: u64) -> Self {
            Self {
                height: std::sync::Mutex::new(height),
                sends: std::sync::Mutex::new(Vec::new()),
            }
        }

        fn sends(&self) -> usize {
            self.sends.lock().unwrap().len()
        }
    }

    #[async_trait::async_trait]
    impl ChainSource for Recorder {
        async fn get_multiple_accounts(
            &self,
            _pubkeys: &[Pubkey],
        ) -> anyhow::Result<Vec<Option<solana_sdk::account::Account>>> {
            unreachable!()
        }
        async fn get_program_accounts(
            &self,
            _program: &Pubkey,
            _filter_sets: &[Vec<relay_chain_source::AccountFilter>],
        ) -> anyhow::Result<Vec<(Pubkey, solana_sdk::account::Account)>> {
            unreachable!()
        }
        async fn clock(&self) -> anyhow::Result<relay_chain_source::ClockSnapshot> {
            unreachable!()
        }
        async fn latest_blockhash(&self) -> anyhow::Result<BlockhashInfo> {
            unreachable!()
        }
        async fn block_height(&self) -> anyhow::Result<u64> {
            Ok(*self.height.lock().unwrap())
        }
        async fn signature_statuses(
            &self,
            signatures: &[Signature],
        ) -> anyhow::Result<Vec<Option<SignatureOutcome>>> {
            Ok(signatures.iter().map(|_| None).collect())
        }
        async fn recent_priority_fee(&self, _accounts: &[Pubkey]) -> anyhow::Result<u64> {
            unreachable!()
        }
        async fn simulate_transaction(
            &self,
            _tx: &Transaction,
            _return_accounts: &[Pubkey],
        ) -> anyhow::Result<relay_chain_source::SimOutcome> {
            unreachable!()
        }
        async fn send_transaction(&self, tx: &Transaction) -> anyhow::Result<Signature> {
            let signature = tx.signatures[0];
            self.sends.lock().unwrap().push(signature);
            Ok(signature)
        }
    }

    fn tracked_one(last_valid_block_height: u64) -> HashMap<Signature, Tracked> {
        let signature = Signature::new_unique();
        let pending = PendingTx {
            transaction: Transaction {
                signatures: vec![signature],
                ..Default::default()
            },
            signature,
            program: Pubkey::new_unique(),
            expected_payment: 5_000,
            last_valid_block_height,
        };
        [(
            signature,
            Tracked {
                pending,
                resends: 0,
                submitted: std::time::Instant::now(),
            },
        )]
        .into_iter()
        .collect()
    }

    /// The state a resend is actually for: unconfirmed, blockhash still
    /// good. Rebroadcast — bounded by `max_resends` — and keep tracking,
    /// because it can still land.
    #[tokio::test]
    async fn a_live_transaction_is_rebroadcast_and_kept() {
        let source = Recorder::new(100);
        let config = SubmitterConfig {
            max_resends: 2,
            ..SubmitterConfig::default()
        };
        let mut tracked = tracked_one(200);
        let mut history = HashMap::new();
        let (profit, _p) = watch::channel(ProfitSnapshot::new());
        let (lag, _l) = watch::channel(LagSnapshot::new());

        for _ in 0..4 {
            sweep(&source, &config, &mut tracked, &mut history, &profit, &lag).await;
        }
        assert_eq!(source.sends(), 2, "rebroadcasts are bounded by max_resends");
        assert_eq!(tracked.len(), 1, "a landable transaction stays tracked");
    }

    /// Past its last valid block height there is nothing left to try:
    /// retire it as `Expired` on the first pass rather than rebroadcasting
    /// a transaction the cluster can no longer accept.
    #[tokio::test]
    async fn a_dead_blockhash_expires_immediately_without_resending() {
        let source = Recorder::new(300);
        let config = SubmitterConfig::default();
        let mut tracked = tracked_one(200);
        let program = tracked.values().next().unwrap().pending.program;
        let mut history = HashMap::new();
        let (profit, _p) = watch::channel(ProfitSnapshot::new());
        let (lag, mut lag_rx) = watch::channel(LagSnapshot::new());

        sweep(&source, &config, &mut tracked, &mut history, &profit, &lag).await;
        assert_eq!(source.sends(), 0, "an expired blockhash cannot land");
        assert!(tracked.is_empty(), "expired transactions are retired");
        // Expiry is congestion, not a lost race: it must not ramp the delay.
        assert_eq!(lag_rx.borrow_and_update().get(&program).copied(), Some(0));
    }

    /// Each revert pushes the delay out by one step, up to the ceiling —
    /// additive, so it settles on the smallest delay that clears the rival
    /// rather than jumping to the ceiling on the first loss.
    #[test]
    fn reverts_ramp_the_delay_additively_up_to_the_ceiling() {
        let ramp: Vec<u64> = (0..6)
            .scan(0, |delay, _| {
                *delay = next_delay(*delay, &failed(), 4, 12);
                Some(*delay)
            })
            .collect();
        assert_eq!(ramp, vec![4, 8, 12, 12, 12, 12]);
    }

    /// Landed cranks decay it, and it reaches zero — a turner whose rival
    /// disappears has to end up back at real time, not stuck a few slots
    /// late forever.
    #[test]
    fn landed_cranks_decay_the_delay_to_zero() {
        let decay: Vec<u64> = (0..5)
            .scan(12, |delay, _| {
                *delay = next_delay(*delay, &TxResult::Landed, 4, 12);
                Some(*delay)
            })
            .collect();
        assert_eq!(decay, vec![6, 3, 1, 0, 0]);
    }

    /// Recovery is deliberately slower than escalation. One landed crank
    /// must not snap the delay back to zero, or the next contested
    /// condition immediately pays for another revert and the delay
    /// oscillates instead of converging.
    #[test]
    fn recovery_is_slower_than_escalation() {
        let after_one_loss = next_delay(0, &failed(), 4, 12);
        let recovered = next_delay(after_one_loss, &TxResult::Landed, 4, 12);
        assert!(
            recovered > 0,
            "a single win wiped the delay: {after_one_loss} -> {recovered}"
        );
    }

    /// An expired transaction never landed, so no fee was burned and no
    /// rival took the work. That is congestion, and waiting longer is not
    /// the fix.
    #[test]
    fn expiry_leaves_the_delay_alone() {
        assert_eq!(next_delay(8, &TxResult::Expired, 4, 12), 8);
        assert_eq!(next_delay(0, &TxResult::Expired, 4, 12), 0);
    }

    /// Zero step is the off switch.
    #[test]
    fn a_zero_step_never_delays() {
        assert_eq!(next_delay(0, &failed(), 0, 12), 0);
    }
}
