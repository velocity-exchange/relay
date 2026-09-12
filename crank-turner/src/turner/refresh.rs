//! Re-scanning the watch registry and re-applying the operator's filter.
//!
//! The gates run cheapest-first. The provider is asked only for allowed
//! programs (server-side memcmp), the registry-only rules are decided from
//! the watch account alone, and only the survivors have their target fetched
//! for the size, owner, and fee checks. A watch dropped here stops being
//! fetched and subscribed entirely until the next refresh.

use std::collections::HashSet;
use std::time::Instant;

use anyhow::Result;
use solana_sdk::account::Account;
use solana_sdk::pubkey::Pubkey;

use relay_chain_source::ChainSource;
use relay_spec as spec;

use crate::filter::{reject_label, RefreshSummary, RejectReason, WatchFilter};

use super::{metrics, Turner, Watch};

impl<S: ChainSource> Turner<S> {
    /// Re-scan the registry and re-apply the watch filter. Returns what was
    /// admitted and why anything else was dropped.
    pub async fn refresh_watches(&mut self) -> Result<RefreshSummary> {
        let started = Instant::now();
        let filter = self.config.filter.clone();
        let mut summary = RefreshSummary::default();

        let candidates = self.registered_watches(&filter).await?;
        let survivors = registry_gate(candidates, &filter, &mut summary);
        let admitted = self.target_gate(survivors, &filter, &mut summary).await?;
        let kept = self.take_to_capacity(admitted, &mut summary);

        summary.admitted = kept;
        record(&summary, kept, started);
        Ok(summary)
    }

    /// Every watch the registry holds for the programs this turner accepts,
    /// deduplicated and in a stable order.
    async fn registered_watches(&self, filter: &WatchFilter) -> Result<Vec<Watch>> {
        let accounts = self
            .source()
            .get_program_accounts(
                &self.config.relay_program,
                &crate::watches::watch_filter_sets(&filter.server_side_programs()),
            )
            .await?;
        let mut candidates: Vec<Watch> = accounts
            .iter()
            .filter_map(|(_, account)| spec::WatchV0::read_from_account(&account.data).ok())
            .map(|w| Watch {
                target_program: Pubkey::from(w.target_program),
                target: Pubkey::from(w.target),
                creator: Pubkey::from(w.creator),
                offset: w.offset,
            })
            .collect();
        candidates.sort_by_key(|w| (w.target.to_bytes(), w.offset));
        candidates.dedup();
        Ok(candidates)
    }

    /// The gates that need the target account. One fetch of the survivors
    /// covers the size, owner, and fee checks.
    async fn target_gate(
        &self,
        candidates: Vec<Watch>,
        filter: &WatchFilter,
        summary: &mut RefreshSummary,
    ) -> Result<Vec<Watch>> {
        let targets: Vec<Pubkey> = candidates.iter().map(|w| w.target).collect();
        let fetched = self.load_map(&targets).await?;
        Ok(candidates
            .into_iter()
            .filter(|w| {
                let account = fetched.get(&w.target);
                match filter
                    .check_target(w, account)
                    .and_then(|()| self.check_conditions(w, account))
                {
                    Ok(()) => true,
                    Err(reason) => {
                        summary.rejected.push((*w, reason));
                        false
                    }
                }
            })
            .collect())
    }

    /// Adopt as many admitted watches as the configured ceiling allows, and
    /// forget the state of everything no longer tracked.
    ///
    /// Dropping the state matters: a re-admitted watch starts clean rather
    /// than inheriting a stale backoff from the last time it was tracked.
    fn take_to_capacity(&mut self, admitted: Vec<Watch>, summary: &mut RefreshSummary) -> usize {
        let (kept, over_capacity) = match self.config.filter.max_watches {
            Some(max) if admitted.len() > max => admitted.split_at(max),
            _ => (admitted.as_slice(), &[][..]),
        };
        summary.rejected.extend(
            over_capacity
                .iter()
                .map(|w| (*w, RejectReason::OverCapacity)),
        );
        self.watches = kept.to_vec();
        let tracked: HashSet<(Pubkey, u32)> =
            self.watches.iter().map(|w| (w.target, w.offset)).collect();
        self.state
            .retain(|(target, offset, _), _| tracked.contains(&(*target, *offset)));
        self.watches.len()
    }

    /// Fee gate: a watch earns its place only if some active condition pays
    /// at least `min_crank_payment`.
    fn check_conditions(
        &self,
        watch: &Watch,
        account: Option<&Account>,
    ) -> Result<(), RejectReason> {
        let account = account.ok_or(RejectReason::TargetMissing)?;
        let (_, conditions) = spec::read_block(&account.data, watch.offset as usize)
            .map_err(|_| RejectReason::Unparseable)?;
        conditions
            .iter()
            .any(|c| c.is_active() && c.min_payment() >= self.config.min_crank_payment)
            .then_some(())
            .ok_or(RejectReason::PaysTooLittle)
    }
}

/// The gates that need nothing but the watch account itself.
fn registry_gate(
    candidates: Vec<Watch>,
    filter: &WatchFilter,
    summary: &mut RefreshSummary,
) -> Vec<Watch> {
    candidates
        .into_iter()
        .filter(|w| match filter.check_registry(w) {
            Ok(()) => true,
            Err(reason) => {
                summary.rejected.push((*w, reason));
                false
            }
        })
        .collect()
}

/// Publish what the refresh found.
///
/// Counters as well as the gauges: a watch that flaps in and out of
/// admission is invisible in a gauge sampled every 15 seconds.
fn record(summary: &RefreshSummary, kept: usize, started: Instant) {
    metrics::WATCHES
        .with_label_values(&["admitted"])
        .set(kept as i64);
    metrics::WATCHES
        .with_label_values(&["rejected"])
        .set(summary.rejected.len() as i64);
    summary.rejected.iter().for_each(|(_, reason)| {
        metrics::REGISTRY_REJECTED
            .with_label_values(&[reject_label(reason)])
            .inc();
    });
    metrics::REFRESH_SECONDS
        .with_label_values(&["total"])
        .observe(started.elapsed().as_secs_f64());
}
