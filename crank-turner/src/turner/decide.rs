//! Whether one condition is due, and the account bytes that answer says so
//! from.
//!
//! A pure function of the cached state and the accounts already fetched: it
//! performs no I/O, which is what lets a whole tick decide sequentially and
//! then crank concurrently.

use std::collections::HashMap;

use solana_sdk::account::Account;
use solana_sdk::pubkey::Pubkey;

use relay_chain_source::{ChainSource, ClockSnapshot};
use relay_spec as spec;

use super::state::CondState;
use super::{metrics, CondKey, Outcome, SkipReason, Turner, Watch};

/// A condition whose wake came due, carried into the concurrent phase.
#[derive(Debug)]
pub(super) struct Due {
    pub(super) key: CondKey,
    pub(super) program: Pubkey,
    pub(super) condition: spec::ConditionV0,
    pub(super) watched_now: Option<Vec<u8>>,
    /// The resolver's account list, materialized at decide time — inline
    /// refs copied out, or an indirect list read from the block's own
    /// account (`resolver_list_offset`), so downstream stages never need
    /// the account bytes again.
    pub(super) resolver_accounts: Vec<spec::AccountRefV0>,
}

impl<S: ChainSource> Turner<S> {
    /// Is this condition due? `Err` carries the skip outcome.
    ///
    /// Six arguments because the gates need them all and none is derivable
    /// from the rest: the watched bytes and the resolver list were read from
    /// account data the caller already holds, and re-reading them here would
    /// undo the point of the phase split.
    pub(super) fn decide(
        &self,
        key: CondKey,
        watch: &Watch,
        condition: &spec::ConditionV0,
        clock: &ClockSnapshot,
        watched_now: Option<Vec<u8>>,
        resolver_accounts: Vec<spec::AccountRefV0>,
    ) -> Result<Due, Outcome> {
        let skipped = |reason| Err(Outcome::Skipped(key, reason));
        if !condition.is_active() {
            return skipped(SkipReason::Inactive);
        }
        if condition.min_payment() < self.config.min_crank_payment {
            return skipped(SkipReason::BelowMinPayment);
        }
        let Ok(wake) = condition.wake() else {
            return skipped(SkipReason::ParseFailed);
        };
        // A program that keeps losing us money is deprioritized rather
        // than retried forever.
        if self.program_profit(&watch.target_program) < self.config.min_program_profit {
            return skipped(SkipReason::Unprofitable);
        }
        let default = CondState::default();
        let state = self.state.get(&key).unwrap_or(&default);
        if clock.slot < state.suppress_until {
            return skipped(SkipReason::Backoff);
        }
        if !wake_is_due(&wake, clock, state, watched_now.as_deref()) {
            return skipped(SkipReason::NotDue);
        }
        if self.held_for_contention(watch, clock, state) {
            return skipped(SkipReason::ContentionDelay);
        }
        metrics::WAKE_LAG
            .with_label_values(&[&metrics::program_label(&watch.target_program)])
            .observe(wake_lag(&wake, clock));
        Ok(Due {
            key,
            program: watch.target_program,
            condition: *condition,
            watched_now,
            resolver_accounts,
        })
    }

    /// The rolling net lamports this program has earned the turner. Without
    /// a submitter nothing observes outcomes, so no program is unprofitable.
    fn program_profit(&self, program: &Pubkey) -> i64 {
        self.submitter
            .as_ref()
            .map_or(i64::MAX, |submitter| submitter.profit_for(program))
    }

    /// Is this program's work being held back to avoid paying for a race it
    /// keeps losing?
    ///
    /// The wait belongs here, ahead of resolve and simulate, and that
    /// placement is the whole mechanism. Arriving late means the resolver
    /// reports nothing to do and no transaction is ever built, so a lost race
    /// costs nothing. Sleeping later — after simulation, before submission —
    /// would be worse than not waiting at all: it would submit a transaction
    /// built on state already known to be stale.
    fn held_for_contention(&self, watch: &Watch, clock: &ClockSnapshot, state: &CondState) -> bool {
        let delay = self.contention_delay(&watch.target_program);
        if delay == 0 {
            return false;
        }
        // Measured against the delay as it stands now, not as it stood when
        // the wait began. That is what makes recovery prompt: when the rival
        // stops and the published delay decays, work already being held is
        // released on the next tick instead of serving out a sentence handed
        // down under the old conditions.
        let waiting_since = state.deferred_since.unwrap_or(clock.slot);
        clock.slot < waiting_since.saturating_add(delay)
    }
}

/// Has this wake come due?
fn wake_is_due(
    wake: &spec::WakeView,
    clock: &ClockSnapshot,
    state: &CondState,
    watched_now: Option<&[u8]>,
) -> bool {
    match *wake {
        spec::WakeView::AtTimestamp { unix_ts } => unix_ts <= clock.unix_timestamp,
        spec::WakeView::AtSlot { slot } => slot <= clock.slot,
        spec::WakeView::EverySlots { slots } => state
            .last_fired
            .is_none_or(|last| clock.slot >= last.saturating_add(slots)),
        spec::WakeView::OnAccountChange { .. } => state.last_seen.as_deref() != watched_now,
        spec::WakeView::OnValueCross { threshold, cmp, .. } => watched_now
            .and_then(|bytes| spec::read_watched_value(bytes, threshold.is_unsigned()))
            .is_some_and(|value| spec::value_crossed(value, threshold, cmp)),
    }
}

/// How late the turner is, relative to when the wake actually came due.
/// Zero for wakes with no due moment to be late against.
fn wake_lag(wake: &spec::WakeView, clock: &ClockSnapshot) -> f64 {
    match *wake {
        spec::WakeView::AtTimestamp { unix_ts } => (clock.unix_timestamp - unix_ts).max(0) as f64,
        _ => 0.0,
    }
}

/// The distinct accounts these conditions watch, minus the ones already
/// loaded.
///
/// Deduped, and that is the point rather than a nicety: two conditions
/// watching one account is the ordinary case — a book watched for its order
/// counts and again for its best prices — and a watched account is
/// whole-account sized, so a duplicate is a second copy of the whole book
/// every tick.
///
/// Ordered, so the account list a tick asks for is the same list twice over
/// and a log or a trace reads the same way each time.
pub(super) fn watched_accounts<'a>(
    conditions: impl Iterator<Item = &'a spec::ConditionV0>,
    loaded: &HashMap<Pubkey, Account>,
) -> Vec<Pubkey> {
    conditions
        .filter_map(|c| match c.wake() {
            Ok(spec::WakeView::OnAccountChange { address, .. })
            | Ok(spec::WakeView::OnValueCross { address, .. }) => {
                let pk = Pubkey::from(address);
                (!loaded.contains_key(&pk)).then_some(pk)
            }
            _ => None,
        })
        .collect::<std::collections::BTreeSet<_>>()
        .into_iter()
        .collect()
}

/// Materialize a condition's resolver account list: `num_resolver_accounts`
/// refs read from the block account's data at `resolver_list_offset`.
/// `None` when that region is unreadable (out of bounds / over the cap).
pub(super) fn materialize_resolver_accounts(
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

/// Clamped slice, so a hostile/garbled offset can't panic the turner.
pub(super) fn slice_or_empty(data: &[u8], offset: u32, len: u32) -> Vec<u8> {
    let start = (offset as usize).min(data.len());
    let end = (offset as usize)
        .saturating_add(len as usize)
        .min(data.len());
    data[start..end].to_vec()
}
