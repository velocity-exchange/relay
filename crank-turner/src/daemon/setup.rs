//! Turning the command line into the configuration the turner runs on.
//!
//! The validation here is deliberately loud. A payout that signs, or an
//! empty program allowlist, are both configurations the turner would
//! otherwise carry quietly — one skip at a time, or one other protocol's
//! watches at a time — and neither reads as a misconfiguration from the
//! outside.

use anyhow::Result;
use solana_sdk::pubkey::Pubkey;
use tracing::warn;

use relay_crank_turner::{
    watch_subscription, CachedSourceConfig, ChainSource, LocalSimConfig, LocalSimSource,
    ProgramSubscription, SubmitterConfig, TurnerConfig, WatchFilter,
};

use super::args::Args;

/// Which watches this turner is willing to track at all.
pub fn watch_filter(args: &Args) -> WatchFilter {
    WatchFilter {
        allowed_target_programs: args.target_program.iter().copied().collect(),
        blocked_target_programs: args.blocked_program.iter().copied().collect(),
        allowed_creators: args.allowed_creator.iter().copied().collect(),
        allowed_targets: Default::default(),
        max_target_bytes: args.max_target_bytes,
        max_watches: args.max_watches,
    }
}

/// The turner's own configuration, once the arguments have been checked
/// against the keeper they will run as.
pub fn turner_config(args: &Args, filter: WatchFilter, keeper: Pubkey) -> Result<TurnerConfig> {
    check_payout(args, keeper)?;
    if filter.allowed_target_programs.is_empty() {
        warn!(
            "no --target-program allowlist: this turner will track every watch in the registry, \
             including other protocols'"
        );
    }
    Ok(TurnerConfig {
        relay_program: args.program_id,
        min_crank_payment: args.min_crank_payment,
        crank_margin_lamports: args.crank_margin_lamports,
        payout: args.payout_address,
        sync_native_payout: args.sync_native_payout,
        trusted_programs: args.trusted_program.iter().copied().collect(),
        guard_payments: !args.no_guard,
        guard_nonce: args.guard_nonce,
        loaded_accounts_data_size: args.loaded_accounts_data_size,
        concurrency: args.concurrency,
        min_program_profit: args.min_program_profit.unwrap_or(i64::MIN),
        max_cranks_per_tx: args.max_cranks_per_tx.max(1),
        max_priority_fee: args.max_priority_fee,
        filter,
        ..TurnerConfig::default()
    })
}

/// The payout must not be the fee payer.
///
/// Signer status is transaction-global, so a payout that signs is a payout
/// an untrusted executor holds a signature for. The turner would refuse
/// every untrusted crank at signing time anyway, silently, one skip at a
/// time. Failing here instead reads as the misconfiguration it is.
fn check_payout(args: &Args, keeper: Pubkey) -> Result<()> {
    if args.payout_address == Some(keeper) {
        anyhow::bail!(
            "--payout-address is the keeper's own key ({keeper}), which is this turner's fee \
             payer. Signer status is transaction-global, so an untrusted executor handed that \
             account could drain it; use a separate account that never signs."
        );
    }
    if args.payout_address.is_none() && !args.trusted_program.is_empty() {
        warn!(
            "no --payout-address: trusted programs will be paid to the fee payer, and untrusted \
             ones will be skipped"
        );
    }
    Ok(())
}

pub fn submitter_config(args: &Args) -> SubmitterConfig {
    SubmitterConfig {
        contention_step_slots: args.contention_step_slots,
        max_contention_slots: args.max_contention_slots,
        ..SubmitterConfig::default()
    }
}

pub fn cached_config(args: &Args) -> CachedSourceConfig {
    CachedSourceConfig {
        // The registry is the query the turner answers from cache.
        indexed_programs: vec![watch_subscription(args.program_id, &[])],
        max_age_uncovered: std::time::Duration::from_millis(args.max_age_uncovered_ms),
        max_age_covered: std::time::Duration::from_secs(args.max_age_covered_s),
        feed_silence_timeout: std::time::Duration::from_secs(args.feed_silence_s),
        covered_programs: args.watch_program.clone(),
    }
}

/// What the feed subscribes to: the watch registry (scoped to the
/// operator's allowlist, so other protocols' watches never cross the wire)
/// plus every account owned by each `--watch-program`, which is what keeps
/// local simulation off the network.
pub fn subscriptions(args: &Args, filter: &WatchFilter) -> Vec<ProgramSubscription> {
    std::iter::once(watch_subscription(
        args.program_id,
        &filter.server_side_programs(),
    ))
    .chain(
        args.watch_program
            .iter()
            .copied()
            .map(ProgramSubscription::all),
    )
    .collect()
}

/// Wrap a source in the local simulator unless the operator opted out.
pub fn with_local_sim<S: ChainSource + 'static>(
    source: S,
    args: &Args,
) -> std::sync::Arc<dyn ChainSource> {
    if args.remote_sim {
        std::sync::Arc::new(source)
    } else {
        std::sync::Arc::new(LocalSimSource::new(
            source,
            LocalSimConfig {
                pool_size: args.concurrency.max(1),
                ..LocalSimConfig::default()
            },
        ))
    }
}
