//! Per-condition bookkeeping between ticks, and how an attempt folds back
//! into it.

use relay_chain_source::ClockSnapshot;

use super::{CondKey, Turner};
use relay_chain_source::ChainSource;

#[derive(Debug, Default)]
pub(super) struct CondState {
    /// Last-seen bytes of a change-wake's watched range. `None` = never
    /// evaluated (counts as changed).
    pub(super) last_seen: Option<Vec<u8>>,
    /// Slot before which this condition is not re-evaluated.
    pub(super) suppress_until: u64,
    /// Consecutive failures (drives exponential backoff).
    pub(super) failures: u32,
    /// Last slot an `EverySlots` wake fired.
    pub(super) last_fired: Option<u64>,
    /// Slot at which this condition was first seen due, when a contention
    /// delay is holding it back. Cleared once it is acted on, so each new
    /// piece of work is delayed afresh rather than once per condition.
    pub(super) deferred_since: Option<u64>,
}

/// How a condition's bookkeeping should change after an attempt. Collected
/// during the concurrent phase and applied afterwards, so the execution
/// path needs no shared mutable state at all — no locks, no channels.
#[derive(Debug)]
pub(super) enum StateUpdate {
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
    /// Fold one attempt's bookkeeping back into the condition state.
    pub(super) fn apply(&mut self, key: CondKey, update: StateUpdate, clock: &ClockSnapshot) {
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
}
