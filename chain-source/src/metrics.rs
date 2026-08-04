//! Metrics for the chain-access layer, in their own registry so a consumer
//! can gather them alongside its own without this crate owning the endpoint.
//!
//! Two exist for operational failures that are otherwise invisible:
//! `chain_update_source` distinguishes an account arriving over the
//! subscription from one arriving via repoll — a silently dead stream shows
//! up as the poll counter climbing while the stream counter flatlines, long
//! before anything else looks wrong. `chain_cache_reads` shows how often the
//! cache actually spares a fetch, which is the whole reason it exists.

use std::sync::LazyLock;

use prometheus::{
    register_histogram_vec_with_registry, register_int_counter_vec_with_registry,
    register_int_gauge_vec_with_registry, HistogramVec, IntCounterVec, IntGaugeVec, Registry,
};

/// This crate's registry. Consumers gather it next to their own.
pub static REGISTRY: LazyLock<Registry> = LazyLock::new(Registry::new);

/// Wall time of each outbound RPC call, by method.
///
/// The other half of a bottleneck question: a tick that takes 800ms has
/// either done a lot of work or spent the time waiting on a provider, and
/// `relay_stage_seconds` alone cannot tell those apart. Watch this next to
/// `chain_cache_reads_total{outcome="uncovered"}` — reads climbing together
/// with latency is the case for another `--watch-program`.
pub static RPC_SECONDS: LazyLock<HistogramVec> = LazyLock::new(|| {
    register_histogram_vec_with_registry!(
        "chain_rpc_seconds",
        "Wall time of outbound RPC calls, by method",
        &["method"],
        vec![0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0],
        REGISTRY
    )
    .expect("metric registers")
});

/// Accounts asked for over RPC. Divided by the call count it gives the
/// average batch size, which is how you see chunking working (or a caller
/// fetching one account at a time).
pub static RPC_ACCOUNTS: LazyLock<IntCounterVec> = LazyLock::new(|| {
    register_int_counter_vec_with_registry!(
        "chain_rpc_accounts_total",
        "Accounts requested over RPC, by method",
        &["method"],
        REGISTRY
    )
    .expect("metric registers")
});

/// RPC calls that returned an error, by method. Distinct from a call that
/// succeeded slowly, and the two want different alerts.
pub static RPC_ERRORS: LazyLock<IntCounterVec> = LazyLock::new(|| {
    register_int_counter_vec_with_registry!(
        "chain_rpc_errors_total",
        "Failed RPC calls, by method",
        &["method"],
        REGISTRY
    )
    .expect("metric registers")
});

/// Accounts held in the cache. Bounded by what the watched programs own, so
/// a step change means a program started emitting accounts — or that a
/// `--watch-program` was pointed somewhere much larger than intended.
pub static CACHED_ACCOUNTS: LazyLock<IntGaugeVec> = LazyLock::new(|| {
    register_int_gauge_vec_with_registry!(
        "chain_cached_accounts",
        "Accounts currently held in the cache",
        &["kind"],
        REGISTRY
    )
    .expect("metric registers")
});

/// Fork switches observed, and the provisional cached accounts they
/// invalidated. Reads run at `processed`, so a write can be taken back; this
/// is how often that actually happens.
pub static REORGS: LazyLock<IntCounterVec> = LazyLock::new(|| {
    register_int_counter_vec_with_registry!(
        "chain_reorgs_total",
        "Fork switches detected, and accounts dropped as a result",
        &["kind"],
        REGISTRY
    )
    .expect("metric registers")
});

/// Where a watched account update came from: `stream` or `poll`.
pub static UPDATE_SOURCE: LazyLock<IntCounterVec> = LazyLock::new(|| {
    register_int_counter_vec_with_registry!(
        "chain_update_source_total",
        "Account updates by arrival path",
        &["source"],
        REGISTRY
    )
    .expect("metric registers")
});

/// Simulations by where they ran: `local` or `error`.
pub static SIMULATIONS: LazyLock<IntCounterVec> = LazyLock::new(|| {
    register_int_counter_vec_with_registry!(
        "chain_simulations_total",
        "Simulations by execution path",
        &["path"],
        REGISTRY
    )
    .expect("metric registers")
});

/// Account reads by how they were served: `hit`, `stale`, or `miss`.
pub static CACHE_READS: LazyLock<IntCounterVec> = LazyLock::new(|| {
    register_int_counter_vec_with_registry!(
        "chain_cache_reads_total",
        "Account reads by cache outcome",
        &["outcome"],
        REGISTRY
    )
    .expect("metric registers")
});

/// Whether the subscription feed is considered alive (1) or dead (0).
pub static FEED_HEALTHY: LazyLock<IntGaugeVec> = LazyLock::new(|| {
    register_int_gauge_vec_with_registry!(
        "chain_feed_healthy",
        "Subscription feed liveness",
        &["feed"],
        REGISTRY
    )
    .expect("metric registers")
});

/// Bounded label for a program: the same 8-character prefix the turner's
/// metrics use, so the two registries agree on how a program is named.
/// Never label by a full pubkey — a large registry would blow up
/// cardinality, and per-account drilldown belongs in the CLI.
pub fn program_label(program: &solana_sdk::pubkey::Pubkey) -> String {
    program.to_string()[..8].to_string()
}

/// Program ELFs re-loaded after detecting an on-chain upgrade, by program.
///
/// A step per redeploy is the expected shape. Anything sustained — and in
/// particular anything tracking `chain_simulations_total` — means the slot
/// comparison never matches and every simulation is re-verifying the ELF,
/// which silently defeats the instance pool. Labelled so that a program
/// thrashing this way can be named.
pub static PROGRAM_RELOADS: LazyLock<IntCounterVec> = LazyLock::new(|| {
    register_int_counter_vec_with_registry!(
        "chain_program_reloads_total",
        "Program ELF re-loads triggered by on-chain upgrade detection",
        &["program"],
        REGISTRY
    )
    .expect("metric registers")
});
