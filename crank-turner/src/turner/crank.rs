//! One due condition, from the resolver simulation through to a transaction
//! the packing phase can take.
//!
//! Three stages, in order: simulate the resolver and read back what it
//! staged; build the executor the payload names; then measure that executor
//! and hold it to what landing it actually costs. Each stage can end the
//! attempt, which is why they hand back a [`CrankResult`] rather than
//! unwinding.
//!
//! Everything here takes `&self`. What an attempt learns comes back as a
//! [`StateUpdate`] for the caller to apply, which is what lets a whole
//! tick's worth of cranks run concurrently with no lock between them.

use std::time::Instant;

use anyhow::{Context, Result};
use solana_sdk::account::Account;
use solana_sdk::instruction::{AccountMeta, Instruction};
use solana_sdk::pubkey::Pubkey;
use tracing::warn;

use relay_chain_source::{ChainSource, SimOutcome};
use relay_spec as spec;

use super::config::MAX_COMPUTE_UNITS;
use super::decide::Due;
use super::state::StateUpdate;
use super::{metrics, skip_label, stage_label, CondKey, Outcome, SkipReason, Stage, Turner};

/// A crank that resolved to real work and passed its own simulation,
/// waiting to be packed into a transaction.
pub(super) struct Prepared {
    pub(super) key: CondKey,
    pub(super) program: Pubkey,
    pub(super) min_payment: u64,
    /// Compute units this crank alone consumed in simulation.
    pub(super) units: u64,
    /// `[begin_guard, executor, assert_paid]`, without compute budget.
    pub(super) ixs: Vec<Instruction>,
}

/// What the concurrent phase produced for one condition.
pub(super) enum CrankResult {
    /// Verified and ready to submit.
    Ready(Box<Prepared>),
    /// Finished without submitting (no work, or a failure).
    Done(Outcome, StateUpdate),
}

/// What the resolver simulation produced.
enum Resolved {
    /// The resolver staged a payload naming the work to do.
    Work(spec::ResolvedCrankV0),
    /// The attempt ends here: the resolver reported nothing to do, or its
    /// simulation failed.
    Settled(CrankResult),
}

impl<S: ChainSource> Turner<S> {
    /// Resolve → build → simulate → submit, for one due condition.
    ///
    /// Wraps [`Self::try_crank`] in the metric bookkeeping, and turns an
    /// error into a failed outcome so one bad condition never ends a tick.
    pub(super) async fn crank(&self, due: Due) -> CrankResult {
        let key = due.key;
        let program = metrics::program_label(&due.program);
        match self.try_crank(&due).await {
            Ok(CrankResult::Ready(prepared)) => CrankResult::Ready(prepared),
            Ok(CrankResult::Done(outcome, update)) => {
                metrics::CRANKS
                    .with_label_values(&[count_label(&outcome, &program), &program])
                    .inc();
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

    pub(super) async fn try_crank(&self, due: &Due) -> Result<CrankResult> {
        let attempt = Attempt { turner: self, due };
        let resolved = match attempt.resolve().await? {
            Resolved::Work(resolved) => resolved,
            Resolved::Settled(result) => return Ok(result),
        };

        // Which instruction to run comes from the resolver, not from the
        // condition: if the turner is willing to run the accounts and args a
        // resolver chose, it is willing to run the instruction it chose, and
        // the guards bound the damage either way.
        let program = Pubkey::from(resolved.executor_program);
        let Some(payout) = self.payout_for(&program) else {
            return Ok(CrankResult::Done(
                Outcome::Skipped(due.key, SkipReason::NoSafePayout),
                StateUpdate::Failed,
            ));
        };
        require_keeper_placeholder(&resolved)?;
        let executor = attempt.build_executor(&resolved, payout);

        // An untrusted executor must not name any account that signs this
        // transaction, because signer status is transaction-global — see
        // `names_transaction_signer`.
        if !self.trusts(&program)
            && super::names_transaction_signer(&executor, &[self.keeper_pubkey()])
        {
            warn!(
                %program,
                "untrusted executor named the fee payer; refusing to submit"
            );
            return Ok(CrankResult::Done(
                Outcome::Skipped(due.key, SkipReason::ExecutorNamedSigner),
                StateUpdate::Failed,
            ));
        }
        attempt.price(executor, payout, program).await
    }
}

/// The three stages of one attempt, and the state they share.
///
/// Each stage needs the condition, its coordinates, and the turner itself,
/// so they are methods on one context rather than four functions passing the
/// same arguments along.
struct Attempt<'a, S: ChainSource> {
    turner: &'a Turner<S>,
    due: &'a Due,
}

impl<S: ChainSource> Attempt<'_, S> {
    fn key(&self) -> CondKey {
        self.due.key
    }

    fn condition(&self) -> &spec::ConditionV0 {
        &self.due.condition
    }

    /// Stage one. Simulate the resolver, asking for its accounts back so the
    /// staged payload can be read out of post-execution state.
    ///
    /// The resolver is told which condition it is answering for: its
    /// discriminator followed by the fired condition's coordinates. A
    /// resolver serving several conditions cannot work without this, and it
    /// costs 37 transaction bytes and no accounts.
    async fn resolve(&self) -> Result<Resolved> {
        let key = self.key();
        let spec = self.condition().crank_spec();
        let resolver_accounts: Vec<Pubkey> = self
            .due
            .resolver_accounts
            .iter()
            .map(|a| Pubkey::from(a.address))
            .collect();
        let resolver_ix = Instruction {
            program_id: Pubkey::from(spec.resolver_program),
            accounts: self
                .due
                .resolver_accounts
                .iter()
                .map(account_ref_meta)
                .collect(),
            data: spec::encode_resolver_data(
                spec.resolver_disc,
                spec::FiredConditionV0::new(key.0.to_bytes(), key.1, key.2),
            )
            .to_vec(),
        };
        let label = metrics::program_label(&Pubkey::from(spec.resolver_program));

        let started = Instant::now();
        let (tx, _) = self.turner.signed_tx(&[resolver_ix]).await?;
        let sim = self
            .turner
            .source()
            .simulate_transaction(&tx, &resolver_accounts)
            .await?;
        metrics::STAGE_SECONDS
            .with_label_values(&["resolve_sim", &label])
            .observe(started.elapsed().as_secs_f64());
        metrics::COMPUTE_UNITS
            .with_label_values(&["resolve", &label])
            .observe(sim.units_consumed as f64);

        if let Some(err) = sim.err {
            return Ok(Resolved::Settled(CrankResult::Done(
                Outcome::Failed {
                    condition: key,
                    stage: Stage::ResolveSim,
                    error: err,
                },
                StateUpdate::Failed,
            )));
        }
        let pointer = spec::ResponsePointerV0::read(sim.return_data.as_deref().unwrap_or_default())
            .map_err(|e| anyhow::anyhow!("resolver return data: {e:?}"))?;
        if !pointer.has_work() {
            return Ok(Resolved::Settled(CrankResult::Done(
                Outcome::NoWork(key),
                StateUpdate::NoWork {
                    last_seen: self.due.watched_now.clone(),
                },
            )));
        }
        Ok(Resolved::Work(
            read_staged(&sim.accounts, &pointer).context("staged resolver payload")?,
        ))
    }

    /// Stage two. Build the executor itself — no CPI wrapper, so all four
    /// CPI levels stay available to the executor's own call stack.
    fn build_executor(&self, resolved: &spec::ResolvedCrankV0, payout: Pubkey) -> Instruction {
        Instruction {
            program_id: Pubkey::from(resolved.executor_program),
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
                    // sufficient on its own — see `names_transaction_signer`.
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
        }
    }

    /// Stage three. Measure the executor, then hold it to what landing it
    /// costs this turner rather than to what the program advertised.
    ///
    /// Both simulations run with a generous budget to learn the real cost;
    /// the packing phase re-signs with a tight limit, because the fee is
    /// charged on the limit the transaction requests rather than on the
    /// units it burns.
    async fn price(
        &self,
        executor: Instruction,
        payout: Pubkey,
        program: Pubkey,
    ) -> Result<CrankResult> {
        let key = self.key();
        let advertised = self.condition().min_payment();
        let label = metrics::program_label(&program);
        let ixs = self
            .turner
            .guarded(executor.clone(), payout, &program, advertised);

        let started = Instant::now();
        let sim = self.simulate(&ixs).await?;
        metrics::STAGE_SECONDS
            .with_label_values(&["execute_sim", &label])
            .observe(started.elapsed().as_secs_f64());
        metrics::COMPUTE_UNITS
            .with_label_values(&["execute", &label])
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

        // The first simulation priced the guard at what the program
        // advertised, which is what it promises rather than what landing it
        // costs us. Now that the units are measured, re-guard at our own cost
        // and simulate once more: a crank that cannot cover the fee we are
        // about to pay fails here, for the price of a local simulation,
        // instead of on chain for the price of the fee itself.
        //
        // Floored at the advertised figure, so a program is still held to its
        // own word even when that word is worth more than our cost.
        let required = advertised.max(self.turner.required_payment(sim.units_consumed as u32));
        let ixs = if required > advertised {
            let repriced = self.turner.guarded(executor, payout, &program, required);
            if self.simulate(&repriced).await?.err.is_some() {
                metrics::SKIPS
                    .with_label_values(&[skip_label(&SkipReason::PaymentBelowCost), &label])
                    .inc();
                return Ok(CrankResult::Done(
                    Outcome::Skipped(key, SkipReason::PaymentBelowCost),
                    StateUpdate::Failed,
                ));
            }
            repriced
        } else {
            ixs
        };

        Ok(CrankResult::Ready(Box::new(Prepared {
            key,
            program: self.due.program,
            min_payment: required,
            units: sim.units_consumed,
            ixs,
        })))
    }

    /// Simulate a guarded executor with the per-transaction compute ceiling,
    /// so the budget is never the thing that fails.
    async fn simulate(&self, ixs: &[Instruction]) -> Result<SimOutcome> {
        let probe = self
            .turner
            .with_compute_budget(ixs.to_vec(), MAX_COMPUTE_UNITS, 0);
        let (tx, _) = self.turner.signed_tx(&probe).await?;
        self.turner.source().simulate_transaction(&tx, &[]).await
    }
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

fn account_ref_meta(a: &spec::AccountRefV0) -> AccountMeta {
    AccountMeta {
        pubkey: Pubkey::from(a.address),
        is_signer: false,
        is_writable: a.is_writable(),
    }
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

/// `relay_cranks_total`'s outcome label for a finished attempt, counting the
/// per-stage failure or skip on the way past.
fn count_label(outcome: &Outcome, program: &str) -> &'static str {
    match outcome {
        Outcome::NoWork(_) => "no_work",
        Outcome::Failed { stage, .. } => {
            metrics::FAILURES
                .with_label_values(&[stage_label(stage), program])
                .inc();
            "failed"
        }
        Outcome::Skipped(_, reason) => {
            metrics::SKIPS
                .with_label_values(&[skip_label(reason), program])
                .inc();
            "skipped"
        }
        Outcome::Sent { .. } => "skipped",
    }
}
