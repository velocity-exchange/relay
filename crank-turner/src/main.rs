use std::time::Duration;

use anyhow::{Context, Result};
use clap::{Parser, ValueEnum};
use relay_crank_turner::{
    derive_ws_url, feed_channel, metrics, spawn_grpc_feed, spawn_submitter, spawn_ws_feed,
    watch_subscription, CachedSource, CachedSourceConfig, ChainSource, GrpcFeedConfig,
    LocalSimConfig, LocalSimSource, Outcome, ProgramSubscription, RpcSource, SubmitterConfig,
    Turner, TurnerConfig, WatchFilter,
};
use solana_sdk::pubkey::Pubkey;
use solana_sdk::signature::Keypair;
use solana_sdk::signer::EncodableKey;
use tracing::{debug, info, warn};

fn submitter_config(args: &Args) -> SubmitterConfig {
    SubmitterConfig {
        contention_step_slots: args.contention_step_slots,
        max_contention_slots: args.max_contention_slots,
        ..SubmitterConfig::default()
    }
}

/// How the turner learns about account state. Simulation and submission
/// always go over RPC regardless.
#[derive(Copy, Clone, Debug, PartialEq, Eq, ValueEnum)]
enum Transport {
    /// Poll RPC for every read (no extra infra).
    Rpc,
    /// `programSubscribe`/`accountSubscribe` over websocket.
    Ws,
    /// Yellowstone/geyser gRPC.
    Grpc,
}

#[derive(Parser)]
#[command(about = "relay crank turner: watch conditions, discover work via simulation, crank")]
struct Args {
    #[arg(long, env = "RELAY_RPC_URL")]
    rpc_url: String,
    /// Path to the keeper keypair (fee payer + payment recipient).
    #[arg(long, env = "RELAY_KEEPER_KEYPAIR")]
    keypair: String,
    #[arg(long, env = "RELAY_TRANSPORT", value_enum, default_value_t = Transport::Rpc)]
    transport: Transport,
    /// Websocket endpoint (ws transport). Defaults to `rpc_url` with the
    /// scheme/port swapped, like the Solana CLI.
    #[arg(long, env = "RELAY_WS_URL")]
    ws_url: Option<String>,
    /// Yellowstone gRPC endpoint (grpc transport).
    #[arg(long, env = "RELAY_GRPC_ENDPOINT")]
    grpc_endpoint: Option<String>,
    /// Yellowstone auth token, if the provider requires one.
    #[arg(long, env = "RELAY_GRPC_X_TOKEN")]
    grpc_x_token: Option<String>,
    /// relay program id.
    #[arg(long, env = "RELAY_PROGRAM_ID", default_value_t = TurnerConfig::default().relay_program)]
    program_id: Pubkey,
    /// Skip conditions advertising less than this many lamports. A watch
    /// with no condition clearing the bar is dropped from the working set
    /// entirely, so its target stops being fetched and subscribed.
    #[arg(long, env = "RELAY_MIN_CRANK_PAYMENT", default_value_t = 0)]
    min_crank_payment: u64,
    /// Only crank watches whose target is owned by one of these programs.
    /// Pushed down to the RPC/geyser provider, so other protocols' watches
    /// are never transmitted. Repeatable / comma-separated. Default: all.
    #[arg(long, env = "RELAY_TARGET_PROGRAMS", value_delimiter = ',')]
    target_program: Vec<Pubkey>,
    /// Never crank these programs. Repeatable / comma-separated.
    #[arg(long, env = "RELAY_BLOCKED_PROGRAMS", value_delimiter = ',')]
    blocked_program: Vec<Pubkey>,
    /// Only crank watches registered by these keys. Repeatable.
    #[arg(long, env = "RELAY_ALLOWED_REGISTRARS", value_delimiter = ',')]
    allowed_registrar: Vec<Pubkey>,
    /// Drop watches whose target account exceeds this many bytes.
    #[arg(long, env = "RELAY_MAX_TARGET_BYTES")]
    max_target_bytes: Option<usize>,
    /// Hard ceiling on how many watches to track at once.
    #[arg(long, env = "RELAY_MAX_WATCHES")]
    max_watches: Option<usize>,
    /// Milliseconds between ticks.
    #[arg(long, env = "RELAY_TICK_MS", default_value_t = 1000)]
    tick_ms: u64,
    /// Re-scan the watch registry every N ticks.
    #[arg(long, env = "RELAY_REFRESH_TICKS", default_value_t = 30)]
    refresh_ticks: u64,
    /// Milliseconds an account with no live subscription may be served
    /// from cache before it is refetched. These are the accounts a
    /// simulation could be wrong about, so keep it short.
    #[arg(long, env = "RELAY_MAX_AGE_UNCOVERED_MS", default_value_t = 400)]
    max_age_uncovered_ms: u64,
    /// Seconds before even a subscription-covered account is revalidated.
    #[arg(long, env = "RELAY_MAX_AGE_COVERED_S", default_value_t = 30)]
    max_age_covered_s: u64,
    /// Seconds of total feed silence after which subscriptions are
    /// disbelieved and everything falls back to refetching.
    #[arg(long, env = "RELAY_FEED_SILENCE_S", default_value_t = 10)]
    feed_silence_s: u64,
    /// Submit executors bare instead of bracketing them with relay's
    /// payment guards. Cheaper, but nothing catches a payment that shrinks
    /// between simulating and landing.
    #[arg(long, env = "RELAY_NO_GUARD", default_value_t = false)]
    no_guard: bool,
    /// Which of the keeper's guard accounts to use. Vary it across
    /// concurrent turners sharing a keeper so they don't serialize on one
    /// write lock.
    #[arg(long, env = "RELAY_GUARD_NONCE", default_value_t = 0)]
    guard_nonce: u8,
    /// How many conditions to resolve and submit at once.
    #[arg(long, env = "RELAY_CONCURRENCY", default_value_t = 8)]
    concurrency: usize,
    /// Skip target programs whose recent cranks netted less than this many
    /// lamports. Default: never skip.
    #[arg(long, env = "RELAY_MIN_PROGRAM_PROFIT", allow_negative_numbers = true)]
    min_program_profit: Option<i64>,
    /// Slots to add to a program's crank delay each time one of its
    /// transactions reverts. A reverted crank means a fee was burned for
    /// nothing — usually a rival turner landed the same work first — and
    /// arriving deliberately later turns the next loss into a free no-op,
    /// because simulation catches it before a transaction is built. Decays
    /// back toward zero as cranks start landing again, so a rival going
    /// away costs only a few seconds of lateness. Zero disables.
    #[arg(long, env = "RELAY_CONTENTION_STEP_SLOTS", default_value_t = 4)]
    contention_step_slots: u64,
    /// Ceiling on that delay, in slots.
    #[arg(long, env = "RELAY_MAX_CONTENTION_SLOTS", default_value_t = 12)]
    max_contention_slots: u64,
    /// Port for /metrics and /health.
    #[arg(long, env = "RELAY_METRICS_PORT", default_value_t = 9899)]
    metrics_port: u16,
    /// Cache every account owned by these programs. Point this at the
    /// protocol you crank so local simulation almost never has to fetch.
    /// Repeatable / comma-separated.
    #[arg(long, env = "RELAY_WATCH_PROGRAMS", value_delimiter = ',')]
    watch_program: Vec<Pubkey>,
    /// Send simulations to the RPC provider instead of running them in an
    /// in-process SVM. Slower and metered; useful to cross-check.
    #[arg(long, env = "RELAY_REMOTE_SIM", default_value_t = false)]
    remote_sim: bool,
    /// Pack up to this many cranks into one transaction (1 = never pack).
    #[arg(long, env = "RELAY_MAX_CRANKS_PER_TX", default_value_t = 3)]
    max_cranks_per_tx: usize,
    /// Where executors pay. MUST NOT be the fee payer: signer status is
    /// transaction-global, so an untrusted executor handed the fee payer
    /// could sign a transfer draining it. Untrusted programs are skipped
    /// entirely without this.
    #[arg(long, env = "RELAY_PAYOUT_ADDRESS")]
    payout_address: Option<Pubkey>,
    /// Periodically roll a wrapped-SOL payout's accumulated lamports into
    /// its token balance. Payment lands as lamports either way; this only
    /// decides when it becomes spendable as wSOL.
    #[arg(long, env = "RELAY_SYNC_NATIVE_PAYOUT", default_value_t = false)]
    sync_native_payout: bool,
    /// How often, in ticks, to run that sync.
    #[arg(long, env = "RELAY_SYNC_EVERY_TICKS", default_value_t = 300)]
    sync_every_ticks: u64,
    /// Programs you wrote and trust: their executors run without payment
    /// guards (two fewer instructions, less compute, smaller
    /// transactions) and may be paid straight to the fee payer.
    #[arg(long, env = "RELAY_TRUSTED_PROGRAMS", value_delimiter = ',')]
    trusted_program: Vec<Pubkey>,
    /// Ceiling on the priority fee, micro-lamports per compute unit.
    #[arg(long, env = "RELAY_MAX_PRIORITY_FEE", default_value_t = 1_000_000)]
    max_priority_fee: u64,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();
    let args = Args::parse();

    let keeper = Keypair::read_from_file(shellexpand(&args.keypair))
        .map_err(|e| anyhow::anyhow!("read keypair {}: {e}", args.keypair))?;
    let filter = WatchFilter {
        allowed_target_programs: args.target_program.iter().copied().collect(),
        blocked_target_programs: args.blocked_program.iter().copied().collect(),
        allowed_registrars: args.allowed_registrar.iter().copied().collect(),
        allowed_targets: Default::default(),
        max_target_bytes: args.max_target_bytes,
        max_watches: args.max_watches,
    };
    if args.payout_address.is_none() && !args.trusted_program.is_empty() {
        warn!(
            "no --payout-address: trusted programs will be paid to the fee payer, and untrusted \
             ones will be skipped"
        );
    }
    if filter.allowed_target_programs.is_empty() {
        warn!(
            "no --target-program allowlist: this turner will track every watch in the registry, \
             including other protocols'"
        );
    }
    let config = TurnerConfig {
        relay_program: args.program_id,
        min_crank_payment: args.min_crank_payment,
        payout: args.payout_address,
        sync_native_payout: args.sync_native_payout,
        trusted_programs: args.trusted_program.iter().copied().collect(),
        guard_payments: !args.no_guard,
        guard_nonce: args.guard_nonce,
        concurrency: args.concurrency,
        min_program_profit: args.min_program_profit.unwrap_or(i64::MIN),
        max_cranks_per_tx: args.max_cranks_per_tx.max(1),
        max_priority_fee: args.max_priority_fee,
        filter: filter.clone(),
        ..TurnerConfig::default()
    };

    let metrics_port = args.metrics_port;
    tokio::spawn(async move {
        if let Err(err) = metrics::serve(metrics_port).await {
            warn!(error = %format!("{err:#}"), "metrics server stopped");
        }
    });
    let rpc = RpcSource::new(args.rpc_url.clone());

    match args.transport {
        Transport::Rpc => {
            let source = with_local_sim(rpc, &args);
            let submitter =
                spawn_submitter(std::sync::Arc::clone(&source), submitter_config(&args));
            run(
                Turner::new(source, keeper, config).with_submitter(submitter),
                &args,
            )
            .await
        }
        Transport::Ws => {
            let ws_url = args
                .ws_url
                .clone()
                .unwrap_or_else(|| derive_ws_url(&args.rpc_url));
            let (sender, receiver) = feed_channel();
            spawn_ws_feed(ws_url.clone(), subscriptions(&args, &filter), sender);
            info!(%ws_url, watched = args.watch_program.len(), "websocket subscriptions enabled");
            let source = with_local_sim(
                CachedSource::new(rpc, receiver, cached_config(&args)),
                &args,
            );
            let submitter =
                spawn_submitter(std::sync::Arc::clone(&source), submitter_config(&args));
            run(
                Turner::new(source, keeper, config).with_submitter(submitter),
                &args,
            )
            .await
        }
        Transport::Grpc => {
            let endpoint = args
                .grpc_endpoint
                .clone()
                .context("--grpc-endpoint is required for the grpc transport")?;
            let (sender, receiver) = feed_channel();
            spawn_grpc_feed(
                GrpcFeedConfig {
                    endpoint: endpoint.clone(),
                    x_token: args.grpc_x_token.clone(),
                },
                subscriptions(&args, &filter),
                sender,
            );
            info!(%endpoint, watched = args.watch_program.len(), "yellowstone gRPC subscriptions enabled");
            let source = with_local_sim(
                CachedSource::new(rpc, receiver, cached_config(&args)),
                &args,
            );
            let submitter =
                spawn_submitter(std::sync::Arc::clone(&source), submitter_config(&args));
            run(
                Turner::new(source, keeper, config).with_submitter(submitter),
                &args,
            )
            .await
        }
    }
}

/// What the feed subscribes to: the watch registry (scoped to the
/// operator's allowlist, so other protocols' watches never cross the wire)
/// plus every account owned by each `--watch-program`, which is what keeps
/// local simulation off the network.
fn subscriptions(args: &Args, filter: &WatchFilter) -> Vec<ProgramSubscription> {
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

fn cached_config(args: &Args) -> CachedSourceConfig {
    CachedSourceConfig {
        // The registry is the query the turner answers from cache.
        indexed_programs: vec![watch_subscription(args.program_id, &[])],
        max_age_uncovered: std::time::Duration::from_millis(args.max_age_uncovered_ms),
        max_age_covered: std::time::Duration::from_secs(args.max_age_covered_s),
        feed_silence_timeout: std::time::Duration::from_secs(args.feed_silence_s),
        covered_programs: args.watch_program.clone(),
    }
}

/// Wrap a source in the local simulator unless the operator opted out.
fn with_local_sim<S: ChainSource + 'static>(
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

async fn run<S: ChainSource>(mut turner: Turner<S>, args: &Args) -> Result<()> {
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
            match turner.refresh_watches().await {
                Ok(summary) => {
                    info!(
                        watches = summary.admitted,
                        filtered_out = summary.rejected.len(),
                        "registry refreshed"
                    );
                    // A dropped watch is invisible work: the target looks
                    // registered on chain and simply never cranks. Say which
                    // one and why, or operating this is guesswork.
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
        if args.sync_native_payout && ticks > 0 && ticks.is_multiple_of(args.sync_every_ticks) {
            match turner.sync_payout().await {
                Ok(Some(signature)) => info!(%signature, "synced wrapped-SOL payout"),
                Ok(None) => {}
                Err(e) => warn!(error = %format!("{e:#}"), "payout sync failed"),
            }
        }
        ticks += 1;
        match turner.tick().await.context("tick") {
            Ok(outcomes) => outcomes.iter().for_each(|o| match o {
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
                Outcome::NoWork(_) | Outcome::Skipped(..) => {}
            }),
            Err(e) => warn!(error = %format!("{e:#}"), "tick failed"),
        }
    }
}

fn shellexpand(path: &str) -> String {
    path.strip_prefix("~/")
        .and_then(|rest| {
            std::env::var("HOME")
                .ok()
                .map(|home| format!("{home}/{rest}"))
        })
        .unwrap_or_else(|| path.to_string())
}
