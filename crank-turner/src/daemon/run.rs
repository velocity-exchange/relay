//! The daemon's timer loop around `Turner::tick`.

use std::time::Duration;

use anyhow::{Context, Result};
use tracing::{debug, info, trace, warn};

use relay_crank_turner::{ChainSource, Outcome, SkipReason, Turner};

use super::args::Args;

/// Tick until interrupted, refreshing the registry and syncing the payout on
/// their own schedules.
pub async fn run<S: ChainSource>(mut turner: Turner<S>, args: &Args) -> Result<()> {
    info!(keeper = %turner.keeper_pubkey(), program = %args.program_id, "starting");
    let mut interval = tokio::time::interval(Duration::from_millis(args.tick_ms));
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut ticks: u64 = 0;
    loop {
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {
                info!("shutting down");
                return Ok(());
            }
            _ = interval.tick() => {}
        }
        if ticks.is_multiple_of(args.refresh_ticks) {
            refresh(&mut turner).await;
        }
        if args.sync_native_payout && ticks > 0 && ticks.is_multiple_of(args.sync_every_ticks) {
            sync_payout(&turner).await;
        }
        ticks += 1;
        match turner.tick().await.context("tick") {
            Ok(outcomes) => outcomes.iter().for_each(log_outcome),
            Err(e) => warn!(error = %format!("{e:#}"), "tick failed"),
        }
    }
}

/// Re-scan the registry, and say what was dropped.
///
/// A dropped watch is invisible work: the target looks registered on chain
/// and simply never cranks. Naming which one and why is the difference
/// between operating this and guessing at it.
async fn refresh<S: ChainSource>(turner: &mut Turner<S>) {
    match turner.refresh_watches().await {
        Ok(summary) => {
            info!(
                watches = summary.admitted,
                filtered_out = summary.rejected.len(),
                "registry refreshed"
            );
            for (watch, reason) in &summary.rejected {
                debug!(
                    target = %watch.target,
                    offset = watch.offset,
                    ?reason,
                    "watch filtered out"
                );
            }
        }
        Err(e) => warn!(error = %format!("{e:#}"), "registry refresh failed"),
    }
}

async fn sync_payout<S: ChainSource>(turner: &Turner<S>) {
    match turner.sync_payout().await {
        Ok(Some(signature)) => info!(%signature, "synced wrapped-SOL payout"),
        Ok(None) => {}
        Err(e) => warn!(error = %format!("{e:#}"), "payout sync failed"),
    }
}

/// Loud enough to triage from, quiet enough to leave on.
///
/// A wake that fired but resolved to nothing is the signal debugging always
/// wants, so it sits at debug; the every-tick not-due and backoff churn
/// stays behind trace.
fn log_outcome(outcome: &Outcome) {
    match outcome {
        Outcome::Sent {
            condition,
            signature,
            min_payment,
        } => info!(?condition, %signature, min_payment, "cranked"),
        Outcome::Failed {
            condition,
            stage,
            error,
        } => warn!(?condition, ?stage, error, "crank failed"),
        Outcome::NoWork(condition) => debug!(?condition, "resolved: no work"),
        Outcome::Skipped(condition, reason) => match reason {
            SkipReason::NotDue | SkipReason::Backoff => trace!(?condition, ?reason, "skipped"),
            _ => debug!(?condition, ?reason, "skipped"),
        },
    }
}
