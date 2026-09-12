//! A read-only account of everything the turner concludes about one
//! condition.
//!
//! The verdict comes from the same `decide` and crank path the daemon runs,
//! so a debugging tool built on this cannot disagree with production about
//! whether a condition is due.

use anyhow::Result;
use solana_sdk::instruction::Instruction;
use solana_sdk::pubkey::Pubkey;
use solana_sdk::signature::Signature;

use relay_chain_source::{ChainSource, ClockSnapshot};
use relay_spec as spec;

use super::crank::CrankResult;
use super::decide::{materialize_resolver_accounts, slice_or_empty, Due};
use super::pack::compute_limit;
use super::{CondKey, Outcome, SkipReason, Stage, Turner};

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

impl<S: ChainSource> Turner<S> {
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
        let clock = self.source().clock().await?;
        let account = self
            .load_map(&[target])
            .await?
            .remove(&target)
            .ok_or_else(|| anyhow::anyhow!("target account {target} does not exist"))?;
        let condition = read_condition(&account.data, offset, index)?;
        let watched_now = self
            .watched_bytes(&condition, target, &account.data)
            .await?;

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
            Ok(due) => self.would_send(due).await,
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

    /// Take a due condition as far as the crank path goes without
    /// submitting: a prepared transaction, or why there is not one.
    async fn would_send(&self, due: Due) -> Verdict {
        match self.try_crank(&due).await {
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
        }
    }

    /// The bytes a change- or value-cross wake compares against, fetching
    /// the watched account when it is not the target itself.
    ///
    /// Sliced where the bytes already are. A watched account is
    /// whole-account sized — a book's is hundreds of kilobytes — so copying
    /// one to read a few bytes out of it is the difference between a watch
    /// costing its region and costing its target.
    async fn watched_bytes(
        &self,
        condition: &spec::ConditionV0,
        target: Pubkey,
        target_data: &[u8],
    ) -> Result<Option<Vec<u8>>> {
        let (address, offset, len) = match condition.wake() {
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
            }) => (Pubkey::from(address), offset, len),
            _ => return Ok(None),
        };
        if address == target {
            return Ok(Some(slice_or_empty(target_data, offset, len)));
        }
        Ok(Some(
            self.load_map(&[address])
                .await?
                .get(&address)
                .map_or_else(Vec::new, |acc| slice_or_empty(&acc.data, offset, len)),
        ))
    }

    /// Submit a prepared crank. Split out from [`Self::explain`] so a
    /// debugging tool can show what would be sent and then send exactly
    /// that, rather than re-deriving it.
    pub async fn send_explained(&self, explanation: &Explanation) -> Result<Signature> {
        let Verdict::WouldSend {
            instructions,
            units,
            ..
        } = &explanation.verdict
        else {
            anyhow::bail!("nothing to send: {:?}", explanation.verdict);
        };
        // The explained instructions are the body alone. A v1 message that
        // names no budget requests zero compute units, so the budget the
        // daemon would have applied is rebuilt from the units the
        // explanation already measured.
        let config = self.tx_config(compute_limit(*units), self.priority_fee());
        let (tx, _) = self.sign_for_submission(instructions, config).await?;
        self.source().send_transaction(&tx).await
    }
}

/// One condition out of a target account's block, with an error that says
/// which step of the lookup failed rather than that something did.
fn read_condition(data: &[u8], offset: u32, index: u8) -> Result<spec::ConditionV0> {
    let (_, conditions) = spec::read_block(data, offset as usize).map_err(|err| {
        anyhow::anyhow!("condition block at offset {offset} is unreadable: {err:?}")
    })?;
    conditions.get(index as usize).copied().ok_or_else(|| {
        anyhow::anyhow!(
            "condition {index} is past the end of the block ({} present)",
            conditions.len()
        )
    })
}
