//! One pass over every known condition.
//!
//! Three phases, and the split is what makes the middle one lock-free.
//! Deciding is cheap, sequential, and does no I/O. Cranking the due
//! conditions runs concurrently and borrows nothing mutable, so one slow
//! simulation cannot stall every other condition. Only then does the
//! bookkeeping each attempt produced get folded back in.

use std::collections::HashMap;
use std::time::Instant;

use anyhow::Result;
use futures_util::stream::{FuturesUnordered, StreamExt};
use solana_sdk::account::Account;
use solana_sdk::pubkey::Pubkey;

use relay_chain_source::{ChainSource, ClockSnapshot};
use relay_spec as spec;

use super::crank::{CrankResult, Prepared};
use super::decide::{materialize_resolver_accounts, slice_or_empty, watched_accounts, Due};
use super::state::StateUpdate;
use super::{metrics, outcome_key, skip_label, wake_label, CondKey, Outcome, SkipReason, Turner};

/// Everything one tick reads, and everything it concludes, while the three
/// phases hand off to each other.
///
/// The accounts are borrowed for the whole pass: every wire type is
/// alignment 1, so a condition block is a cast out of the fetched bytes
/// rather than a copy per condition — which is what makes a registry of
/// thousands affordable to read every tick.
struct Pass<'a> {
    clock: &'a ClockSnapshot,
    /// The watched targets, plus the extra accounts change-wakes point at.
    ///
    /// Borrowed rather than owned so a condition block read out of it
    /// outlives any `&mut` borrow of the pass itself — the conditions are
    /// cast in place out of these bytes, and copying them per condition is
    /// the cost this avoids.
    accounts: &'a HashMap<Pubkey, Account>,
    outcomes: Vec<Outcome>,
    updates: Vec<(CondKey, StateUpdate)>,
}

impl<'a> Pass<'a> {
    fn account(&self, pubkey: &Pubkey) -> Option<&'a Account> {
        self.accounts.get(pubkey)
    }

    /// The bytes a change- or value-cross wake compares against, as they
    /// read now. `None` for wakes that watch no account.
    fn watched_now(&self, condition: &spec::ConditionV0) -> Option<Vec<u8>> {
        match condition.wake() {
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
                self.account(&Pubkey::from(address))
                    .map(|acc| slice_or_empty(&acc.data, offset, len))
                    .unwrap_or_default(),
            ),
            _ => None,
        }
    }

    fn skip(&mut self, key: CondKey, reason: SkipReason, program: &str) {
        metrics::SKIPS
            .with_label_values(&[skip_label(&reason), program])
            .inc();
        self.outcomes.push(Outcome::Skipped(key, reason));
    }
}

impl<S: ChainSource> Turner<S> {
    /// One pass over every known condition. See the module docs for the
    /// phase split.
    pub async fn tick(&mut self) -> Result<Vec<Outcome>> {
        let started = Instant::now();
        let clock = self.source.clock().await?;
        let accounts = self.load_tick_accounts().await?;
        let mut pass = Pass {
            clock: &clock,
            accounts: &accounts,
            outcomes: Vec::new(),
            updates: Vec::new(),
        };

        let due = self.decide_all(&mut pass);
        metrics::TICK_SECONDS
            .with_label_values(&["decide"])
            .observe(started.elapsed().as_secs_f64());

        let executing = Instant::now();
        let prepared = self.crank_all(due, &mut pass).await;
        self.submit_packs(prepared, &mut pass.outcomes, &mut pass.updates)
            .await;
        metrics::IN_FLIGHT.with_label_values(&["cranks"]).set(0);
        metrics::TICK_SECONDS
            .with_label_values(&["execute"])
            .observe(executing.elapsed().as_secs_f64());

        let Pass {
            outcomes, updates, ..
        } = pass;
        updates
            .into_iter()
            .for_each(|(key, update)| self.apply(key, update, &clock));
        metrics::TICK_SECONDS
            .with_label_values(&["total"])
            .observe(started.elapsed().as_secs_f64());
        Ok(outcomes)
    }

    /// Every account this tick evaluates against: the watched targets, plus
    /// the accounts change-wakes point at that are not targets themselves.
    async fn load_tick_accounts(&self) -> Result<HashMap<Pubkey, Account>> {
        let targets: Vec<Pubkey> = self.watches.iter().map(|w| w.target).collect();
        let mut accounts = self.load_map(&targets).await?;
        let extras = watched_accounts(
            self.watches
                .iter()
                .filter_map(|w| self.conditions_of(&accounts, w))
                .flat_map(|conditions| conditions.iter()),
            &accounts,
        );
        accounts.extend(self.load_map(&extras).await?);
        Ok(accounts)
    }

    /// The condition block a watch points at, borrowed in place out of the
    /// fetched account bytes.
    fn conditions_of<'a>(
        &self,
        accounts: &'a HashMap<Pubkey, Account>,
        watch: &super::Watch,
    ) -> Option<&'a [spec::ConditionV0]> {
        accounts
            .get(&watch.target)
            .and_then(|acc| spec::read_block(&acc.data, watch.offset as usize).ok())
            .map(|(_, conditions)| conditions)
    }

    /// Phase one. A pure function of the cached state and the accounts
    /// already fetched, so nothing here waits on the cluster.
    fn decide_all(&self, pass: &mut Pass) -> Vec<Due> {
        // Held apart from `pass` so a borrow of the account bytes survives
        // the `&mut pass` each skip needs.
        let accounts = pass.accounts;
        self.watches
            .iter()
            .flat_map(|watch| {
                let program = metrics::program_label(&watch.target_program);
                let Some(conditions) = self.conditions_of(accounts, watch) else {
                    pass.skip(
                        (watch.target, watch.offset, 0),
                        SkipReason::ParseFailed,
                        &program,
                    );
                    return Vec::new();
                };
                conditions
                    .iter()
                    .enumerate()
                    .filter_map(|(index, condition)| {
                        self.decide_one(pass, watch, condition, index as u8, &program)
                    })
                    .collect()
            })
            .collect()
    }

    /// One condition's verdict, with the skip bookkeeping that goes with it.
    fn decide_one(
        &self,
        pass: &mut Pass,
        watch: &super::Watch,
        condition: &spec::ConditionV0,
        index: u8,
        program: &str,
    ) -> Option<Due> {
        let key: CondKey = (watch.target, watch.offset, index);
        let watched_now = pass.watched_now(condition);
        metrics::EVALUATIONS
            .with_label_values(&[wake_label(condition), program])
            .inc();
        let Some(resolver_accounts) = pass
            .account(&watch.target)
            .and_then(|acc| materialize_resolver_accounts(condition, &acc.data))
        else {
            pass.skip(key, SkipReason::ParseFailed, program);
            return None;
        };
        match self.decide(
            key,
            watch,
            condition,
            pass.clock,
            watched_now,
            resolver_accounts,
        ) {
            Ok(ready) => Some(ready),
            Err(outcome) => {
                if let Outcome::Skipped(_, reason) = &outcome {
                    metrics::SKIPS
                        .with_label_values(&[skip_label(reason), program])
                        .inc();
                }
                // The one skip with bookkeeping: record when the wait
                // started, so the delay is measured from first sight rather
                // than from the latest tick.
                if matches!(outcome, Outcome::Skipped(_, SkipReason::ContentionDelay)) {
                    pass.updates.push((key, StateUpdate::Deferred));
                }
                pass.outcomes.push(outcome);
                None
            }
        }
    }

    /// Phase two. Crank the due conditions concurrently, bounded so a large
    /// registry cannot open an unbounded number of RPC calls.
    async fn crank_all(&self, due: Vec<Due>, pass: &mut Pass<'_>) -> Vec<Prepared> {
        record_load(&due, self.config.concurrency);
        let mut prepared: Vec<Prepared> = Vec::new();
        // Explicit loops: the closure forms would need `&mut running`
        // alongside the `&self` the in-flight futures hold.
        let mut running = FuturesUnordered::new();
        let mut queued = due.into_iter();
        for _ in 0..self.config.concurrency {
            match queued.next() {
                Some(next) => running.push(self.crank(next)),
                None => break,
            }
        }
        // Refill as each finishes, so the pipeline stays full rather than
        // draining in lockstep batches.
        while let Some(result) = running.next().await {
            if let Some(next) = queued.next() {
                running.push(self.crank(next));
            }
            match result {
                CrankResult::Ready(ready) => prepared.push(*ready),
                CrankResult::Done(outcome, update) => {
                    pass.updates.push((outcome_key(&outcome), update));
                    pass.outcomes.push(outcome);
                }
            }
        }
        prepared
    }
}

/// Publish how much work this tick found, bucketed per program so one busy
/// protocol is distinguishable from a registry-wide surge.
fn record_load(due: &[Due], concurrency: usize) {
    metrics::IN_FLIGHT
        .with_label_values(&["cranks"])
        .set(due.len() as i64);
    if due.is_empty() {
        return;
    }
    let per_program = due.iter().fold(HashMap::new(), |mut counts, d| {
        *counts.entry(d.program).or_insert(0u64) += 1;
        counts
    });
    per_program.iter().for_each(|(program, count)| {
        metrics::DUE_PER_TICK
            .with_label_values(&[&metrics::program_label(program)])
            .observe(*count as f64);
    });
    if due.len() > concurrency {
        metrics::SATURATED_TICKS
            .with_label_values(&["concurrency"])
            .inc();
    }
}
