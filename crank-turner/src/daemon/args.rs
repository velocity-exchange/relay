//! The daemon's command line.

use clap::{Parser, ValueEnum};
use solana_sdk::pubkey::Pubkey;

use relay_crank_turner::TurnerConfig;

/// How the turner learns about account state. Simulation and submission
/// always go over RPC regardless.
#[derive(Copy, Clone, Debug, PartialEq, Eq, ValueEnum)]
pub enum Transport {
    /// Poll RPC for every read (no extra infra).
    Rpc,
    /// `programSubscribe`/`accountSubscribe` over websocket.
    Ws,
    /// Yellowstone/geyser gRPC.
    Grpc,
}

#[derive(Parser)]
#[command(about = "relay crank turner: watch conditions, discover work via simulation, crank")]
pub struct Args {
    #[arg(long, env = "RELAY_RPC_URL")]
    pub rpc_url: String,
    /// Path to the keeper keypair (fee payer + payment recipient).
    #[arg(long, env = "RELAY_KEEPER_KEYPAIR")]
    pub keypair: String,
    #[arg(long, env = "RELAY_TRANSPORT", value_enum, default_value_t = Transport::Rpc)]
    pub transport: Transport,
    /// Websocket endpoint (ws transport). Defaults to `rpc_url` with the
    /// scheme/port swapped, like the Solana CLI.
    #[arg(long, env = "RELAY_WS_URL")]
    pub ws_url: Option<String>,
    /// Yellowstone gRPC endpoint (grpc transport).
    #[arg(long, env = "RELAY_GRPC_ENDPOINT")]
    pub grpc_endpoint: Option<String>,
    /// Yellowstone auth token, if the provider requires one.
    #[arg(long, env = "RELAY_GRPC_X_TOKEN")]
    pub grpc_x_token: Option<String>,
    /// relay program id.
    #[arg(long, env = "RELAY_PROGRAM_ID", default_value_t = TurnerConfig::default().relay_program)]
    pub program_id: Pubkey,
    /// Skip conditions advertising less than this many lamports. A watch
    /// with no condition clearing the bar is dropped from the working set
    /// entirely, so its target stops being fetched and subscribed.
    #[arg(long, env = "RELAY_MIN_CRANK_PAYMENT", default_value_t = 0)]
    pub min_crank_payment: u64,
    /// Lamports a crank must clear *above* the fee this turner pays to land
    /// it. The payment guard is set to fee + this, so a crank that cannot
    /// cover it fails in simulation and is skipped rather than landing at a
    /// loss. Zero accepts break-even work.
    #[arg(long, env = "RELAY_CRANK_MARGIN", default_value_t = 0)]
    pub crank_margin_lamports: u64,
    /// Only crank watches whose target is owned by one of these programs.
    /// Pushed down to the RPC/geyser provider, so other protocols' watches
    /// are never transmitted. Repeatable / comma-separated. Default: all.
    #[arg(long, env = "RELAY_TARGET_PROGRAMS", value_delimiter = ',')]
    pub target_program: Vec<Pubkey>,
    /// Never crank these programs. Repeatable / comma-separated.
    #[arg(long, env = "RELAY_BLOCKED_PROGRAMS", value_delimiter = ',')]
    pub blocked_program: Vec<Pubkey>,
    /// Only crank watches registered by these keys. Repeatable.
    #[arg(long, env = "RELAY_ALLOWED_CREATORS", value_delimiter = ',')]
    pub allowed_creator: Vec<Pubkey>,
    /// Drop watches whose target account exceeds this many bytes.
    #[arg(long, env = "RELAY_MAX_TARGET_BYTES")]
    pub max_target_bytes: Option<usize>,
    /// Hard ceiling on how many watches to track at once.
    #[arg(long, env = "RELAY_MAX_WATCHES")]
    pub max_watches: Option<usize>,
    /// Milliseconds between ticks.
    #[arg(long, env = "RELAY_TICK_MS", default_value_t = 1000)]
    pub tick_ms: u64,
    /// Re-scan the watch registry every N ticks.
    #[arg(long, env = "RELAY_REFRESH_TICKS", default_value_t = 30)]
    pub refresh_ticks: u64,
    /// Milliseconds an account with no live subscription may be served
    /// from cache before it is refetched. These are the accounts a
    /// simulation could be wrong about, so keep it short.
    #[arg(long, env = "RELAY_MAX_AGE_UNCOVERED_MS", default_value_t = 400)]
    pub max_age_uncovered_ms: u64,
    /// Seconds before even a subscription-covered account is revalidated.
    #[arg(long, env = "RELAY_MAX_AGE_COVERED_S", default_value_t = 30)]
    pub max_age_covered_s: u64,
    /// Seconds of total feed silence after which subscriptions are
    /// disbelieved and everything falls back to refetching.
    #[arg(long, env = "RELAY_FEED_SILENCE_S", default_value_t = 10)]
    pub feed_silence_s: u64,
    /// Submit executors bare instead of bracketing them with relay's
    /// payment guards. Cheaper, but nothing catches a payment that shrinks
    /// between simulating and landing.
    #[arg(long, env = "RELAY_NO_GUARD", default_value_t = false)]
    pub no_guard: bool,
    /// Which of the keeper's guard accounts to use. Vary it across
    /// concurrent turners sharing a keeper so they don't serialize on one
    /// write lock.
    #[arg(long, env = "RELAY_GUARD_NONCE", default_value_t = 0)]
    pub guard_nonce: u8,
    /// Ceiling on the account data a crank transaction may load, in bytes.
    /// Billed on the figure asked for, so asking for nothing takes the 64 MiB
    /// default and pays for all of it. Must clear the largest program this
    /// turner cranks plus its program data, with room to grow.
    #[arg(
        long,
        env = "RELAY_LOADED_ACCOUNTS_DATA_SIZE",
        default_value_t = 12 * 1024 * 1024
    )]
    pub loaded_accounts_data_size: u32,
    /// How many conditions to resolve and submit at once.
    #[arg(long, env = "RELAY_CONCURRENCY", default_value_t = 8)]
    pub concurrency: usize,
    /// Skip target programs whose recent cranks netted less than this many
    /// lamports. Default: never skip.
    #[arg(long, env = "RELAY_MIN_PROGRAM_PROFIT", allow_negative_numbers = true)]
    pub min_program_profit: Option<i64>,
    /// Slots to add to a program's crank delay each time one of its
    /// transactions reverts. A reverted crank means a fee was burned for
    /// nothing — usually a rival turner landed the same work first — and
    /// arriving deliberately later turns the next loss into a free no-op,
    /// because simulation catches it before a transaction is built. Decays
    /// back toward zero as cranks start landing again, so a rival going
    /// away costs only a few seconds of lateness. Zero disables.
    #[arg(long, env = "RELAY_CONTENTION_STEP_SLOTS", default_value_t = 4)]
    pub contention_step_slots: u64,
    /// Ceiling on that delay, in slots.
    #[arg(long, env = "RELAY_MAX_CONTENTION_SLOTS", default_value_t = 12)]
    pub max_contention_slots: u64,
    /// Port for /metrics and /health.
    #[arg(long, env = "RELAY_METRICS_PORT", default_value_t = 9899)]
    pub metrics_port: u16,
    /// Cache every account owned by these programs. Point this at the
    /// protocol you crank so local simulation almost never has to fetch.
    /// Repeatable / comma-separated.
    #[arg(long, env = "RELAY_WATCH_PROGRAMS", value_delimiter = ',')]
    pub watch_program: Vec<Pubkey>,
    /// Send simulations to the RPC provider instead of running them in an
    /// in-process SVM. Slower and metered; useful to cross-check.
    #[arg(long, env = "RELAY_REMOTE_SIM", default_value_t = false)]
    pub remote_sim: bool,
    /// Pack up to this many cranks into one transaction (1 = never pack).
    #[arg(long, env = "RELAY_MAX_CRANKS_PER_TX", default_value_t = 3)]
    pub max_cranks_per_tx: usize,
    /// Where executors pay. MUST NOT be the fee payer: signer status is
    /// transaction-global, so an untrusted executor handed the fee payer
    /// could sign a transfer draining it. Untrusted programs are skipped
    /// entirely without this.
    #[arg(long, env = "RELAY_PAYOUT_ADDRESS")]
    pub payout_address: Option<Pubkey>,
    /// Periodically roll a wrapped-SOL payout's accumulated lamports into
    /// its token balance. Payment lands as lamports either way; this only
    /// decides when it becomes spendable as wSOL.
    #[arg(long, env = "RELAY_SYNC_NATIVE_PAYOUT", default_value_t = false)]
    pub sync_native_payout: bool,
    /// How often, in ticks, to run that sync.
    #[arg(long, env = "RELAY_SYNC_EVERY_TICKS", default_value_t = 300)]
    pub sync_every_ticks: u64,
    /// Programs you wrote and trust: their executors run without payment
    /// guards (two fewer instructions, less compute, smaller
    /// transactions) and may be paid straight to the fee payer.
    #[arg(long, env = "RELAY_TRUSTED_PROGRAMS", value_delimiter = ',')]
    pub trusted_program: Vec<Pubkey>,
    /// Ceiling on the priority fee, micro-lamports per compute unit.
    #[arg(long, env = "RELAY_MAX_PRIORITY_FEE", default_value_t = 1_000_000)]
    pub max_priority_fee: u64,
}

pub fn shellexpand(path: &str) -> String {
    path.strip_prefix("~/")
        .and_then(|rest| {
            std::env::var("HOME")
                .ok()
                .map(|home| format!("{home}/{rest}"))
        })
        .unwrap_or_else(|| path.to_string())
}
