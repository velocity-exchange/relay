//! Grouping verified cranks into transactions, and handing those to the
//! submitter.

use anyhow::Result;
use solana_message::v1;
use solana_sdk::instruction::Instruction;
use solana_sdk::message::VersionedMessage;
use solana_sdk::pubkey::Pubkey;
use solana_sdk::signature::Signature;
use solana_sdk::transaction::VersionedTransaction;

use relay_chain_source::ChainSource;

use crate::submit::PendingTx;

use super::config::MAX_COMPUTE_UNITS;
use super::crank::Prepared;
use super::payment::priority_fee_lamports;
use super::state::StateUpdate;
use super::{metrics, CondKey, Outcome, Stage, Turner};

impl<S: ChainSource> Turner<S> {
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
    pub(super) async fn submit_packs(
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
            let config = self.tx_config(units, self.priority_fee());
            match self.sign_for_submission(&body, config).await {
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
    pub(super) async fn measure(&self, body: &[Instruction]) -> Option<u32> {
        let (tx, _) = self
            .signed_tx(body, self.tx_config(MAX_COMPUTE_UNITS, 0))
            .await
            .ok()?;
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
        let config = self.tx_config(units, self.priority_fee());
        match self.sign_for_submission(&body, config).await {
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
        tx: VersionedTransaction,
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

    /// Exact size check: build the transaction and measure it, no
    /// estimating. Signed with the worst-case budget so the measurement
    /// bounds the real transaction.
    ///
    /// v1 caps four things, not just bytes. A pack that fits in 4096 bytes
    /// can still carry too many addresses or instructions, and each of
    /// those is a sanitization failure: the transaction never executes, so
    /// it reports no error to act on.
    async fn fits(&self, pack: &[Prepared]) -> bool {
        let ixs: Vec<Instruction> = pack.iter().flat_map(|p| p.ixs.iter().cloned()).collect();
        let config = self.tx_config(MAX_COMPUTE_UNITS, u64::MAX);
        let Ok((tx, _)) = self.signed_tx(&ixs, config).await else {
            return false;
        };
        let VersionedMessage::V1(message) = &tx.message else {
            return false;
        };
        v1_transaction_size(message, tx.signatures.len()) <= v1::MAX_TRANSACTION_SIZE
            && message.account_keys.len() <= usize::from(v1::MAX_ADDRESSES)
            && message.instructions.len() <= usize::from(v1::MAX_INSTRUCTIONS)
            && message.header.num_required_signatures <= v1::MAX_SIGNATURES
    }

    /// Hand a signed transaction to the submitter, or send it inline when
    /// none is attached. Either way the signature is already known — the
    /// turner signed it — so nothing here waits on the cluster.
    pub(super) async fn submit(
        &self,
        tx: VersionedTransaction,
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

    /// The compute budget for one transaction.
    ///
    /// A v1 message carries the budget in its own header, so the budget is
    /// not an instruction. Nothing is prepended to the instruction list, so
    /// a guard triple keeps the position it was packed at.
    ///
    /// Every field is set on purpose. A v1 field left out is zero rather
    /// than a default, and a transaction that requests zero compute units
    /// or zero loaded-accounts data cannot land.
    ///
    /// `price_per_unit` is micro-lamports per compute unit, which is what
    /// the RPC reports and what the turner bids in. v1 states the priority
    /// fee as a total in lamports, so it is converted here with the same
    /// arithmetic that prices the crank in `required_payment`.
    pub(super) fn tx_config(&self, units: u32, price_per_unit: u64) -> v1::TransactionConfig {
        v1::TransactionConfig::empty()
            .with_compute_unit_limit(units)
            .with_loaded_accounts_data_size_limit(self.config.loaded_accounts_data_size)
            .with_priority_fee(priority_fee_lamports(price_per_unit, units))
    }
}

/// Serialized size of a signed v1 transaction, in bytes.
///
/// The wire form is the version prefix byte, then the message, then the
/// signatures as a fixed-length array with no length prefix.
fn v1_transaction_size(message: &v1::Message, signatures: usize) -> usize {
    const VERSION_PREFIX_BYTES: usize = 1;
    VERSION_PREFIX_BYTES + message.size() + signatures * v1::SIGNATURE_SIZE
}

/// Split cranks into one group per target program, keeping the order they
/// arrived in within each group. See [`Turner::pack`] for why a transaction
/// must not mix two.
pub(super) fn group_by_program(prepared: Vec<Prepared>) -> Vec<Vec<Prepared>> {
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

/// Headroom over the simulated cost, for state that moved since.
pub(super) fn compute_limit(units_consumed: u64) -> u32 {
    ((units_consumed as f64 * 1.2) as u64).clamp(1_000, MAX_COMPUTE_UNITS as u64) as u32
}
