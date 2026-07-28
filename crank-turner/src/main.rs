use std::time::Duration;

use anyhow::{Context, Result};
use clap::{Parser, ValueEnum};
use relay_crank_turner::{
    derive_ws_url, feed_channel, grpc::spawn_grpc_feed_with_programs, metrics, spawn_submitter,
    ws::spawn_ws_feed_with_programs, CachedSource, CachedSourceConfig, ChainSource, GrpcFeedConfig,
    LocalSimConfig, LocalSimSource, Outcome, RpcSource, SubmitterConfig, Turner, TurnerConfig,
    WatchFilter,
};
use solana_sdk::pubkey::Pubkey;
use solana_sdk::signature::Keypair;
use solana_sdk::signer::EncodableKey;
use tracing::{info, warn};

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
    /// Refetch a subscription-cached account through RPC every N reads
    /// (0 = never) — insurance against a silently dead subscription.
    #[arg(long, env = "RELAY_REPOLL_EVERY", default_value_t = 32)]
    repoll_every: u64,
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
    if filter.allowed_target_programs.is_empty() {
        warn!(
            "no --target-program allowlist: this turner will track every watch in the registry, \
             including other protocols'"
        );
    }
    let config = TurnerConfig {
        relay_program: args.program_id,
        min_crank_payment: args.min_crank_payment,
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
                spawn_submitter(std::sync::Arc::clone(&source), SubmitterConfig::default());
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
            spawn_ws_feed_with_programs(
                ws_url.clone(),
                args.program_id,
                filter.server_side_programs(),
                args.watch_program.clone(),
                sender,
            );
            info!(%ws_url, watched = args.watch_program.len(), "websocket subscriptions enabled");
            let source = with_local_sim(
                CachedSource::new(rpc, receiver, cached_config(&args)),
                &args,
            );
            let submitter =
                spawn_submitter(std::sync::Arc::clone(&source), SubmitterConfig::default());
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
            spawn_grpc_feed_with_programs(
                GrpcFeedConfig {
                    endpoint: endpoint.clone(),
                    x_token: args.grpc_x_token.clone(),
                },
                args.program_id,
                filter.server_side_programs(),
                args.watch_program.clone(),
                sender,
            );
            info!(%endpoint, watched = args.watch_program.len(), "yellowstone gRPC subscriptions enabled");
            let source = with_local_sim(
                CachedSource::new(rpc, receiver, cached_config(&args)),
                &args,
            );
            let submitter =
                spawn_submitter(std::sync::Arc::clone(&source), SubmitterConfig::default());
            run(
                Turner::new(source, keeper, config).with_submitter(submitter),
                &args,
            )
            .await
        }
    }
}

fn cached_config(args: &Args) -> CachedSourceConfig {
    CachedSourceConfig {
        relay_program: args.program_id,
        repoll_every: args.repoll_every,
        watch_programs: args.watch_program.clone(),
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
                Ok(summary) => info!(
                    watches = summary.admitted,
                    filtered_out = summary.rejected.len(),
                    "registry refreshed"
                ),
                Err(e) => warn!(error = %format!("{e:#}"), "registry refresh failed"),
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
