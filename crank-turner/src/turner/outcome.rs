//! What one attempt on a condition concludes, and the metric labels that
//! name those conclusions.
//!
//! The label strings are spelled out rather than derived from the variant
//! names. `grafana/relay-dashboard.json` and the alerts beside it select on
//! these exact strings, so a rename in an enum must not silently empty a
//! panel.

use solana_sdk::signature::Signature;

use relay_spec as spec;

use super::CondKey;

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
    /// This crank's payment does not cover the fee this turner would pay to
    /// land it. Distinct from `Unprofitable`, which is a judgement about a
    /// program's history rather than about the crank in hand.
    PaymentBelowCost,
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

/// The condition an outcome belongs to.
pub(super) fn outcome_key(outcome: &Outcome) -> CondKey {
    match outcome {
        Outcome::Skipped(key, _) | Outcome::NoWork(key) => *key,
        Outcome::Sent { condition, .. } | Outcome::Failed { condition, .. } => *condition,
    }
}

pub(super) fn skip_label(reason: &SkipReason) -> &'static str {
    match reason {
        SkipReason::NotDue => "not_due",
        SkipReason::Backoff => "backoff",
        SkipReason::Inactive => "inactive",
        SkipReason::BelowMinPayment => "below_min_payment",
        SkipReason::ParseFailed => "parse_failed",
        SkipReason::Unprofitable => "unprofitable",
        SkipReason::PaymentBelowCost => "payment_below_cost",
        SkipReason::ContentionDelay => "contention_delay",
        SkipReason::NoSafePayout => "no_safe_payout",
        SkipReason::ExecutorNamedSigner => "executor_named_signer",
    }
}

/// Stable metric label for a wake kind, for the load breakdown.
pub(super) fn wake_label(condition: &spec::ConditionV0) -> &'static str {
    match condition.wake() {
        Ok(spec::WakeView::AtTimestamp { .. }) => "at_timestamp",
        Ok(spec::WakeView::AtSlot { .. }) => "at_slot",
        Ok(spec::WakeView::EverySlots { .. }) => "every_slots",
        Ok(spec::WakeView::OnAccountChange { .. }) => "on_account_change",
        Ok(spec::WakeView::OnValueCross { .. }) => "on_value_cross",
        Err(_) => "unknown",
    }
}

pub(super) fn stage_label(stage: &Stage) -> &'static str {
    match stage {
        Stage::ResolveSim => "resolve_sim",
        Stage::ExecuteSim => "execute_sim",
        Stage::Send => "send",
    }
}
