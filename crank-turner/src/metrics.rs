//! Prometheus metrics and a `/metrics` + `/health` endpoint.
//!
//! The label set follows the one tuktuk's crank turner found useful in
//! production, adapted to conditions instead of tasks. `relay_wake_lag_seconds`
//! is there for an operational failure that is otherwise invisible: how late a
//! condition was cranked relative to when its wake became due. Rising lag means
//! the turner is oversubscribed, not that the chain is slow.
//!
//! Chain-access metrics (`chain_*`: cache outcomes, feed liveness, update
//! path, simulation path) live in `relay-chain-source`'s own registry;
//! [`encode`] gathers both so `/metrics` exports them together.

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

/// Skips broken out by reason. `relay_cranks_total{outcome="skipped"}`
/// counts them all together, which is nearly useless on a dashboard: a
/// turner skipping everything because nothing is due looks identical to one
/// skipping because a target program is trying to steal its signature. This
/// is the series to graph, and the reasons that matter are not the common
/// ones — `not_due` and `backoff` are the healthy baseline, while
/// `executor_named_signer`, `parse_failed`, and `no_safe_payout` should be
/// flat zero and mean something is wrong when they are not.
pub static SKIPS: LazyLock<IntCounterVec> = LazyLock::new(|| {
    register_int_counter_vec_with_registry!(
        "relay_skips_total",
        "Conditions skipped, by reason",
        &["reason", "program"],
        REGISTRY
    )
    .expect("metric registers")
});

/// Conditions evaluated, by the kind of wake that put them up for
/// evaluation. This is the load metric: it counts every condition looked at
/// on every tick, whether or not anything came of it, so it answers "what is
/// generating all this work" — and the wake kind is usually the answer,
/// since a tight `EverySlots` or a change-wake on a hot account costs a
/// resolve simulation every time it fires.
pub static EVALUATIONS: LazyLock<IntCounterVec> = LazyLock::new(|| {
    register_int_counter_vec_with_registry!(
        "relay_evaluations_total",
        "Conditions evaluated, by wake kind",
        &["wake", "program"],
        REGISTRY
    )
    .expect("metric registers")
});

/// Wall time of one condition's resolve and execute simulations, and of its
/// submission. Per-condition, not per-tick, so a single expensive resolver
/// is visible rather than averaged into the tick.
pub static STAGE_SECONDS: LazyLock<HistogramVec> = LazyLock::new(|| {
    register_histogram_vec_with_registry!(
        "relay_stage_seconds",
        "Wall time of one crank's stages",
        &["stage", "program"],
        // Local simulation should be sub-millisecond; anything past ~50ms
        // means it is reaching the network, which is the bug this catches.
        vec![0.0005, 0.001, 0.005, 0.01, 0.05, 0.1, 0.5, 1.0, 5.0],
        REGISTRY
    )
    .expect("metric registers")
});

/// Compute units a crank's simulations consumed. The cost side of the same
/// question `relay_lamports_total` answers on the revenue side: a resolver
/// whose CU keeps climbing will eventually stop fitting in a packed
/// transaction, and this is the early warning.
pub static COMPUTE_UNITS: LazyLock<HistogramVec> = LazyLock::new(|| {
    register_histogram_vec_with_registry!(
        "relay_compute_units",
        "Compute units consumed in simulation, by stage",
        &["stage", "program"],
        vec![
            1_000.0,
            5_000.0,
            20_000.0,
            50_000.0,
            100_000.0,
            200_000.0,
            400_000.0,
            800_000.0,
            1_400_000.0
        ],
        REGISTRY
    )
    .expect("metric registers")
});

/// Conditions found due in one tick. Compare against `--concurrency`: a
/// distribution pressed against the cap means the turner is the bottleneck
/// and work is waiting on a slot, not on the chain.
pub static DUE_PER_TICK: LazyLock<HistogramVec> = LazyLock::new(|| {
    register_histogram_vec_with_registry!(
        "relay_due_per_tick",
        "Conditions due in one tick",
        &["program"],
        vec![1.0, 2.0, 4.0, 8.0, 16.0, 32.0, 64.0, 128.0, 512.0],
        REGISTRY
    )
    .expect("metric registers")
});

/// Ticks in which more conditions were due than could run at once. The
/// single clearest saturation signal: while this climbs, cranks are late for
/// no reason but the turner's own limits.
pub static SATURATED_TICKS: LazyLock<IntCounterVec> = LazyLock::new(|| {
    register_int_counter_vec_with_registry!(
        "relay_saturated_ticks_total",
        "Ticks where due conditions exceeded the concurrency limit",
        &["kind"],
        REGISTRY
    )
    .expect("metric registers")
});

/// Watches dropped at registry refresh, by reason — the same reasons
/// `relay watch list --rejected` prints. A rejected watch is invisible
/// everywhere else, so a step change here (a target program upgrading its
/// layout, say, and turning every block unparseable) is otherwise silent.
pub static REGISTRY_REJECTED: LazyLock<IntCounterVec> = LazyLock::new(|| {
    register_int_counter_vec_with_registry!(
        "relay_registry_rejected_total",
        "Watches rejected at refresh, by reason",
        &["reason"],
        REGISTRY
    )
    .expect("metric registers")
});

/// How long a registry refresh takes. It scans every watch the provider
/// will return, so it grows with the whole registry rather than with this
/// turner's share of it.
pub static REFRESH_SECONDS: LazyLock<HistogramVec> = LazyLock::new(|| {
    register_histogram_vec_with_registry!(
        "relay_refresh_seconds",
        "Wall time of a registry refresh",
        &["phase"],
        vec![0.01, 0.05, 0.1, 0.5, 1.0, 2.5, 5.0, 10.0, 30.0],
        REGISTRY
    )
    .expect("metric registers")
});

/// Submission to settlement, by what it settled as. Separating this from
/// `relay_wake_lag_seconds` splits "we were slow to decide" from "the
/// cluster was slow to confirm", which are different problems with
/// different fixes.
pub static CONFIRM_SECONDS: LazyLock<HistogramVec> = LazyLock::new(|| {
    register_histogram_vec_with_registry!(
        "relay_confirm_seconds",
        "Time from submission to settlement",
        &["result"],
        vec![0.5, 1.0, 2.0, 4.0, 8.0, 15.0, 30.0, 60.0],
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

/// Both registries: the turner's own metrics plus the chain-access layer's
/// (`relay-chain-source` owns those so it needn't know about this endpoint).
pub fn encode() -> String {
    let mut families = REGISTRY.gather();
    families.extend(relay_chain_source::metrics::REGISTRY.gather());
    TextEncoder::new()
        .encode_to_string(&families)
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
