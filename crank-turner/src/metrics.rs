//! Prometheus metrics and a `/metrics` + `/health` endpoint.
//!
//! The label set follows the one tuktuk's crank turner found useful in
//! production, adapted to conditions instead of tasks. Two of them are
//! there for specific operational failures that are otherwise invisible:
//!
//! - `relay_update_source` — whether a watched account arrived over the
//!   subscription or the periodic repoll. A subscription that has silently
//!   died shows up as the poll counter climbing while the stream counter
//!   flatlines, long before anything else looks wrong.
//! - `relay_wake_lag_seconds` — how late a condition was cranked relative
//!   to when its wake became due. Rising lag means the turner is
//!   oversubscribed, not that the chain is slow.

use std::sync::LazyLock;

use anyhow::{Context, Result};
use prometheus::{
    register_histogram_vec_with_registry, register_int_counter_vec_with_registry,
    register_int_gauge_vec_with_registry, HistogramVec, IntCounterVec, IntGaugeVec, Registry,
    TextEncoder,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tracing::{info, warn};

pub static REGISTRY: LazyLock<Registry> = LazyLock::new(Registry::new);

/// Cranks by terminal outcome: `sent`, `no_work`, `skipped`, `failed`.
pub static CRANKS: LazyLock<IntCounterVec> = LazyLock::new(|| {
    register_int_counter_vec_with_registry!(
        "relay_cranks_total",
        "Crank attempts by outcome",
        &["outcome", "program"],
        REGISTRY
    )
    .expect("metric registers")
});

/// Failures by the stage that produced them, so a target program that
/// resolves badly is distinguishable from one that pays badly.
pub static FAILURES: LazyLock<IntCounterVec> = LazyLock::new(|| {
    register_int_counter_vec_with_registry!(
        "relay_crank_failures_total",
        "Crank failures by stage",
        &["stage", "program"],
        REGISTRY
    )
    .expect("metric registers")
});

/// Transactions by what became of them after submission.
pub static TRANSACTIONS: LazyLock<IntCounterVec> = LazyLock::new(|| {
    register_int_counter_vec_with_registry!(
        "relay_transactions_total",
        "Submitted transactions by result",
        &["result"],
        REGISTRY
    )
    .expect("metric registers")
});

/// Lamports in and out, per target program — the input to profitability.
pub static LAMPORTS: LazyLock<IntCounterVec> = LazyLock::new(|| {
    register_int_counter_vec_with_registry!(
        "relay_lamports_total",
        "Lamports earned and spent, by direction",
        &["direction", "program"],
        REGISTRY
    )
    .expect("metric registers")
});

/// Where a watched account update came from: `subscription` or `repoll`.
pub static UPDATE_SOURCE: LazyLock<IntCounterVec> = LazyLock::new(|| {
    register_int_counter_vec_with_registry!(
        "relay_update_source_total",
        "Account reads served by source",
        &["source"],
        REGISTRY
    )
    .expect("metric registers")
});

/// Simulations by where they ran: `local` (in-process SVM) or `error`.
pub static SIMULATIONS: LazyLock<IntCounterVec> = LazyLock::new(|| {
    register_int_counter_vec_with_registry!(
        "relay_simulations_total",
        "Simulations by execution site",
        &["site"],
        REGISTRY
    )
    .expect("metric registers")
});

/// Transactions by how they were assembled: `packed`, `single`, or
/// `split` (a pack that failed to simulate and was sent one by one).
pub static PACKS: LazyLock<IntCounterVec> = LazyLock::new(|| {
    register_int_counter_vec_with_registry!(
        "relay_packs_total",
        "Crank transactions by packing outcome",
        &["kind"],
        REGISTRY
    )
    .expect("metric registers")
});

/// Cache reads by whether a live subscription covers the account. A
/// climbing `uncovered` count is the signal to add a `--watch-program`:
/// those reads are the ones paying an RPC round trip to stay safe.
pub static CACHE_READS: LazyLock<IntCounterVec> = LazyLock::new(|| {
    register_int_counter_vec_with_registry!(
        "relay_cache_reads_total",
        "Cache reads by subscription coverage",
        &["coverage"],
        REGISTRY
    )
    .expect("metric registers")
});

/// 1 while the feed is delivering, 0 once it has gone silent past the
/// timeout. The clock sysvar updates every slot, so this is a true
/// liveness signal rather than a measure of chain activity.
pub static FEED_HEALTHY: LazyLock<IntGaugeVec> = LazyLock::new(|| {
    register_int_gauge_vec_with_registry!(
        "relay_feed_healthy",
        "Whether the account subscription feed is delivering",
        &["feed"],
        REGISTRY
    )
    .expect("metric registers")
});

pub static WATCHES: LazyLock<IntGaugeVec> = LazyLock::new(|| {
    register_int_gauge_vec_with_registry!(
        "relay_watches",
        "Watches by admission state",
        &["state"],
        REGISTRY
    )
    .expect("metric registers")
});

pub static IN_FLIGHT: LazyLock<IntGaugeVec> = LazyLock::new(|| {
    register_int_gauge_vec_with_registry!(
        "relay_in_flight",
        "Work in progress",
        &["kind"],
        REGISTRY
    )
    .expect("metric registers")
});

/// Slots each program's cranks are currently held back by, to avoid paying
/// for races it keeps losing. Nonzero means a rival turner is winning.
pub static CONTENTION_DELAY: LazyLock<IntGaugeVec> = LazyLock::new(|| {
    register_int_gauge_vec_with_registry!(
        "relay_contention_delay_slots",
        "Slots cranks are deliberately delayed by, per program",
        &["program"],
        REGISTRY
    )
    .expect("metric registers")
});

/// Seconds between a wake becoming due and the crank being submitted.
pub static WAKE_LAG: LazyLock<HistogramVec> = LazyLock::new(|| {
    register_histogram_vec_with_registry!(
        "relay_wake_lag_seconds",
        "Delay between a wake coming due and its crank being submitted",
        &["program"],
        vec![0.25, 0.5, 1.0, 2.0, 5.0, 10.0, 30.0, 60.0, 300.0],
        REGISTRY
    )
    .expect("metric registers")
});

/// Duration of a full tick, the signal for whether the turner keeps up.
pub static TICK_SECONDS: LazyLock<HistogramVec> = LazyLock::new(|| {
    register_histogram_vec_with_registry!(
        "relay_tick_seconds",
        "Wall time of one turner tick",
        &["phase"],
        vec![0.01, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0],
        REGISTRY
    )
    .expect("metric registers")
});

pub fn encode() -> String {
    TextEncoder::new()
        .encode_to_string(&REGISTRY.gather())
        .unwrap_or_else(|err| format!("# encoding failed: {err}\n"))
}

/// Serve `/metrics` and `/health` until the process exits.
///
/// Hand-rolled rather than pulling in a web framework: two routes, no
/// dynamic paths, no bodies to parse.
pub async fn serve(port: u16) -> Result<()> {
    let listener = TcpListener::bind(("0.0.0.0", port))
        .await
        .with_context(|| format!("bind metrics port {port}"))?;
    info!(port, "metrics listening");
    loop {
        let (mut socket, _) = match listener.accept().await {
            Ok(pair) => pair,
            Err(err) => {
                warn!(error = %err, "metrics accept failed");
                continue;
            }
        };
        tokio::spawn(async move {
            let mut buf = [0u8; 1024];
            let read = socket.read(&mut buf).await.unwrap_or(0);
            let request = String::from_utf8_lossy(&buf[..read]);
            let body = if request.starts_with("GET /health") {
                "ok".to_string()
            } else if request.starts_with("GET /metrics") {
                encode()
            } else {
                let _ = socket
                    .write_all(b"HTTP/1.1 404 Not Found\r\ncontent-length: 0\r\n\r\n")
                    .await;
                return;
            };
            let response = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: text/plain; version=0.0.4\r\ncontent-length: \
                 {}\r\n\r\n{body}",
                body.len()
            );
            let _ = socket.write_all(response.as_bytes()).await;
        });
    }
}

/// Programs are labels, so keep the cardinality bounded and readable.
pub fn program_label(program: &solana_sdk::pubkey::Pubkey) -> String {
    program.to_string()[..8].to_string()
}
