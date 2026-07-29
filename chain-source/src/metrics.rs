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
    register_int_counter_vec_with_registry, register_int_gauge_vec_with_registry, IntCounterVec,
    IntGaugeVec, Registry,
};

/// This crate's registry. Consumers gather it next to their own.
pub static REGISTRY: LazyLock<Registry> = LazyLock::new(Registry::new);

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
