//! The condition loop: wake evaluation → resolver simulation → executor
//! build → simulation → send. Deliberately a pull-style `tick()` state
//! machine so tests (and alternative runtimes) drive it deterministically;
//! `main.rs` wraps it in a timer.

use std::collections::HashMap;

use anyhow::{Context, Result};
use relay_spec as spec;
use solana_sdk::account::Account;
use solana_sdk::instruction::{AccountMeta, Instruction};
use solana_sdk::pubkey::Pubkey;
use solana_sdk::signature::{Keypair, Signature};
use solana_sdk::signer::Signer;
use solana_sdk::transaction::Transaction;

use crate::source::{ChainSource, ClockSnapshot};

/// One registered watch, parsed from a `WatchV0` account.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Watch {
    pub target: Pubkey,
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
}

impl Default for TurnerConfig {
    fn default() -> Self {
        Self {
            relay_program: "4D5tPhw9sqkdkR5CpmP427TH6y9p9AMuKUukUEHn3Mpu"
                .parse()
                .unwrap(),
            min_crank_payment: 0,
            no_work_backoff_slots: 8,
            failure_backoff_slots: 16,
        }
    }
}

#[derive(Debug, Default)]
struct CondState {
    /// Last-seen bytes of a dirty-wake's watched range. `None` = never
    /// evaluated (counts as dirty).
    last_dirty: Option<Vec<u8>>,
    /// Slot before which this condition is not re-evaluated.
    suppress_until: u64,
    /// Consecutive failures (drives exponential backoff).
    failures: u32,
    /// Last slot an `EverySlots` wake fired.
    last_fired: Option<u64>,
}

pub struct Turner<S: ChainSource> {
    source: S,
    keeper: Keypair,
    config: TurnerConfig,
    watches: Vec<Watch>,
    state: HashMap<CondKey, CondState>,
}

impl<S: ChainSource> Turner<S> {
    pub fn new(source: S, keeper: Keypair, config: TurnerConfig) -> Self {
        Self {
            source,
            keeper,
            config,
            watches: Vec::new(),
            state: HashMap::new(),
        }
    }

    pub fn keeper_pubkey(&self) -> Pubkey {
        self.keeper.pubkey()
    }

    pub fn source(&self) -> &S {
        &self.source
    }

    /// Re-scan the registry. Unparseable accounts are ignored; duplicate
    /// `(target, offset)` registrations collapse to one.
    pub async fn refresh_watches(&mut self) -> Result<usize> {
        let accounts = self
            .source
            .get_watch_accounts(&self.config.relay_program)
            .await?;
        let mut watches: Vec<Watch> = accounts
            .iter()
            .filter_map(|(_, account)| spec::WatchV0::read_from_account(&account.data).ok())
            .map(|w| Watch {
                target: Pubkey::from(w.target),
                offset: w.offset,
            })
            .collect();
        watches.sort_by_key(|w| (w.target.to_bytes(), w.offset));
        watches.dedup();
        self.watches = watches;
        Ok(self.watches.len())
    }

    /// One pass over every known condition. Returns an outcome per
    /// condition so callers (and tests) see exactly what happened.
    pub async fn tick(&mut self) -> Result<Vec<Outcome>> {
        let clock = self.source.clock().await?;

        // Load all targets, parse their condition blocks.
        let targets: Vec<Pubkey> = self.watches.iter().map(|w| w.target).collect();
        let target_accounts = self.load_map(&targets).await?;
        let parsed: Vec<(Watch, Option<Vec<spec::ConditionV0>>)> = self
            .watches
            .iter()
            .map(|w| {
                let conditions = target_accounts.get(&w.target).and_then(|acc| {
                    spec::read_conditions_unaligned(&acc.data, w.offset as usize).ok()
                });
                (*w, conditions)
            })
            .collect();

        // Dirty wakes may watch accounts other than the target; fetch those
        // too (deduped, skipping ones already loaded).
        let dirty_extras: Vec<Pubkey> = parsed
            .iter()
            .filter_map(|(_, conditions)| conditions.as_ref())
            .flat_map(|conditions| conditions.iter())
            .filter_map(|c| match c.wake() {
                Ok(spec::WakeView::OnAccountDirty { address, .. }) => {
                    let pk = Pubkey::from(address);
                    (!target_accounts.contains_key(&pk)).then_some(pk)
                }
                _ => None,
            })
            .collect();
        let extra_accounts = self.load_map(&dirty_extras).await?;
        let account_of = |pk: &Pubkey| -> Option<&Account> {
            target_accounts.get(pk).or_else(|| extra_accounts.get(pk))
        };

        let mut outcomes = Vec::new();
        for (watch, conditions) in &parsed {
            let Some(conditions) = conditions else {
                outcomes.push(Outcome::Skipped(
                    (watch.target, watch.offset, 0),
                    SkipReason::ParseFailed,
                ));
                continue;
            };
            for (index, condition) in conditions.iter().enumerate() {
                let key: CondKey = (watch.target, watch.offset, index as u8);
                // Dirty-wake current bytes, read before the mutable state
                // borrow inside evaluate_condition.
                let dirty_now: Option<Vec<u8>> = match condition.wake() {
                    Ok(spec::WakeView::OnAccountDirty {
                        address,
                        offset,
                        len,
                    }) => Some(
                        account_of(&Pubkey::from(address))
                            .map(|acc| {
                                let start = (offset as usize).min(acc.data.len());
                                let end = (offset as usize + len as usize).min(acc.data.len());
                                acc.data[start..end].to_vec()
                            })
                            .unwrap_or_default(),
                    ),
                    _ => None,
                };
                let outcome = self
                    .evaluate_condition(key, condition, &clock, dirty_now)
                    .await;
                outcomes.push(outcome);
            }
        }
        Ok(outcomes)
    }

    async fn evaluate_condition(
        &mut self,
        key: CondKey,
        condition: &spec::ConditionV0,
        clock: &ClockSnapshot,
        dirty_now: Option<Vec<u8>>,
    ) -> Outcome {
        if !condition.is_active() {
            return Outcome::Skipped(key, SkipReason::Inactive);
        }
        if condition.min_payment < self.config.min_crank_payment {
            return Outcome::Skipped(key, SkipReason::BelowMinPayment);
        }
        let Ok(wake) = condition.wake() else {
            return Outcome::Skipped(key, SkipReason::ParseFailed);
        };
        let state = self.state.entry(key).or_default();
        if clock.slot < state.suppress_until {
            return Outcome::Skipped(key, SkipReason::Backoff);
        }
        let due = match wake {
            spec::WakeView::AtTimestamp { unix_ts } => unix_ts <= clock.unix_timestamp,
            spec::WakeView::EverySlots { slots } => state
                .last_fired
                .is_none_or(|last| clock.slot >= last.saturating_add(slots)),
            spec::WakeView::OnAccountDirty { .. } => {
                state.last_dirty.as_deref() != dirty_now.as_deref()
            }
        };
        if !due {
            return Outcome::Skipped(key, SkipReason::NotDue);
        }

        match self.crank(key, condition, clock, dirty_now).await {
            Ok(outcome) => outcome,
            Err(err) => {
                self.record_failure(key, clock);
                Outcome::Failed {
                    condition: key,
                    stage: Stage::Send,
                    error: format!("{err:#}"),
                }
            }
        }
    }

    /// Resolve → build → simulate → send, for a condition whose wake is due.
    async fn crank(
        &mut self,
        key: CondKey,
        condition: &spec::ConditionV0,
        clock: &ClockSnapshot,
        dirty_now: Option<Vec<u8>>,
    ) -> Result<Outcome> {
        // Simulate the resolver.
        let resolver_ix = Instruction {
            program_id: Pubkey::from(condition.resolver_program),
            accounts: condition
                .resolver_accounts()
                .iter()
                .map(account_ref_meta)
                .collect(),
            data: condition.resolver_disc.to_vec(),
        };
        let sim = {
            let tx = self.signed_tx(&[resolver_ix]).await?;
            self.source.simulate_transaction(&tx).await?
        };
        if let Some(err) = sim.err {
            self.record_failure(key, clock);
            return Ok(Outcome::Failed {
                condition: key,
                stage: Stage::ResolveSim,
                error: err,
            });
        }
        let resolved = parse_resolved(sim.return_data.as_deref().unwrap_or_default())
            .context("resolver return data")?;

        if !resolved.work {
            // The wake was evaluated: settle its state so a stale-early hint
            // doesn't re-simulate every tick.
            let state = self.state.entry(key).or_default();
            state.last_dirty = dirty_now;
            state.last_fired = Some(clock.slot);
            state.suppress_until = clock.slot + self.config.no_work_backoff_slots;
            state.failures = 0;
            return Ok(Outcome::NoWork(key));
        }

        // Build the crank_v0-wrapped executor.
        let keeper = self.keeper.pubkey();
        let keeper_index = resolved
            .accounts
            .iter()
            .position(|a| a.address == spec::KEEPER_PLACEHOLDER)
            .context("resolver output names no keeper placeholder")?;
        let executor_metas = resolved.accounts.iter().map(|a| AccountMeta {
            pubkey: if a.address == spec::KEEPER_PLACEHOLDER {
                keeper
            } else {
                Pubkey::from(a.address)
            },
            is_signer: false,
            is_writable: a.is_writable(),
        });
        let crank_ix = Instruction {
            program_id: self.config.relay_program,
            accounts: [
                AccountMeta::new_readonly(key.0, false),
                AccountMeta::new_readonly(Pubkey::from(condition.executor_program), false),
            ]
            .into_iter()
            .chain(executor_metas)
            .collect(),
            data: spec::encode_crank_v0_data(key.1, key.2, keeper_index as u8, &resolved.data),
        };

        // Simulate, then fire. `crank_v0` asserts the keeper payment, so a
        // successful simulation is also the payment check.
        let tx = self.signed_tx(&[crank_ix]).await?;
        let sim = self.source.simulate_transaction(&tx).await?;
        if let Some(err) = sim.err {
            self.record_failure(key, clock);
            return Ok(Outcome::Failed {
                condition: key,
                stage: Stage::ExecuteSim,
                error: err,
            });
        }
        let signature = self.source.send_transaction(&tx).await?;

        let state = self.state.entry(key).or_default();
        // The crank mutates watched state; force a fresh dirty evaluation
        // next tick rather than diffing against a pre-crank snapshot.
        state.last_dirty = None;
        state.last_fired = Some(clock.slot);
        state.suppress_until = clock.slot + 1;
        state.failures = 0;
        Ok(Outcome::Sent {
            condition: key,
            signature,
            min_payment: condition.min_payment,
        })
    }

    fn record_failure(&mut self, key: CondKey, clock: &ClockSnapshot) {
        let base = self.config.failure_backoff_slots;
        let state = self.state.entry(key).or_default();
        state.failures += 1;
        state.suppress_until = clock.slot + (base << state.failures.min(6));
    }

    async fn signed_tx(&self, ixs: &[Instruction]) -> Result<Transaction> {
        let blockhash = self.source.latest_blockhash().await?;
        Ok(Transaction::new_signed_with_payer(
            ixs,
            Some(&self.keeper.pubkey()),
            &[&self.keeper],
            blockhash,
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

fn account_ref_meta(a: &spec::AccountRefV0) -> AccountMeta {
    AccountMeta {
        pubkey: Pubkey::from(a.address),
        is_signer: false,
        is_writable: a.is_writable(),
    }
}

/// Parse a resolver's return data. RPC transports strip trailing zero bytes
/// from return data, so on truncation retry with a zero-extended buffer —
/// safe because only zeros can have been stripped.
fn parse_resolved(data: &[u8]) -> Result<spec::ResolvedCrankV0> {
    spec::ResolvedCrankV0::read(data).or_else(|first_err| {
        let padded: Vec<u8> = data
            .iter()
            .copied()
            .chain(std::iter::repeat_n(0u8, 1024 + 16))
            .collect();
        spec::ResolvedCrankV0::read(&padded)
            .map_err(|_| anyhow::anyhow!("unparseable resolver return data: {first_err:?}"))
    })
}
