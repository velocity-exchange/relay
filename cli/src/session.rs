//! A turner configured exactly as the daemon would be, with its registry
//! loaded. Everything else in this crate is presentation.

use anyhow::{anyhow, Context, Result};
use clap::Args as ClapArgs;
use solana_sdk::pubkey::Pubkey;
use solana_sdk::signature::Keypair;
use solana_sdk::signer::EncodableKey;

use relay_chain_source::{ChainSource, LocalSimConfig, LocalSimSource, RpcSource};
use relay_crank_turner::{RefreshSummary, Turner, TurnerConfig, WatchFilter};

#[derive(ClapArgs, Clone)]
pub struct Common {
    #[arg(long, env = "RELAY_RPC_URL", global = true)]
    pub rpc_url: Option<String>,
    /// relay program id.
    #[arg(long, env = "RELAY_PROGRAM_ID", global = true)]
    pub program_id: Option<Pubkey>,
    /// Keeper keypair. Only needed to send; read-only commands use an
    /// ephemeral key, which is fine because nothing is signed.
    #[arg(long, env = "RELAY_KEEPER_KEYPAIR", global = true)]
    pub keypair: Option<String>,
    /// Emit JSON instead of tables.
    #[arg(long, global = true)]
    pub json: bool,

    // --- the daemon's own config, so verdicts match production ---
    /// Mirror the daemon's `--min-crank-payment`.
    #[arg(long, env = "RELAY_MIN_CRANK_PAYMENT", global = true)]
    pub min_crank_payment: Option<u64>,
    /// Mirror the daemon's `--target-program`.
    #[arg(
        long,
        env = "RELAY_TARGET_PROGRAMS",
        value_delimiter = ',',
        global = true
    )]
    pub target_program: Vec<Pubkey>,
    /// Mirror the daemon's `--blocked-program`.
    #[arg(
        long,
        env = "RELAY_BLOCKED_PROGRAMS",
        value_delimiter = ',',
        global = true
    )]
    pub blocked_program: Vec<Pubkey>,
    /// Mirror the daemon's `--allowed-creator`.
    #[arg(
        long,
        env = "RELAY_ALLOWED_CREATORS",
        value_delimiter = ',',
        global = true
    )]
    pub allowed_creator: Vec<Pubkey>,
    /// Mirror the daemon's `--trusted-program`.
    #[arg(
        long,
        env = "RELAY_TRUSTED_PROGRAMS",
        value_delimiter = ',',
        global = true
    )]
    pub trusted_program: Vec<Pubkey>,
    /// Mirror the daemon's `--payout-address`.
    #[arg(long, env = "RELAY_PAYOUT_ADDRESS", global = true)]
    pub payout_address: Option<Pubkey>,
    /// Mirror the daemon's `--no-guard`.
    #[arg(long, global = true)]
    pub no_guard: bool,
    /// A running daemon's metrics endpoint, for the state a fresh process
    /// cannot see: backoff, contention delay, profitability.
    #[arg(long, env = "RELAY_METRICS_URL", global = true)]
    pub metrics_url: Option<String>,
}

/// A turner configured exactly as the daemon would be, with its registry
/// loaded. Everything else in this file is presentation.
pub type Loaded = (
    Turner<std::sync::Arc<dyn ChainSource>>,
    TurnerConfig,
    RefreshSummary,
);

pub async fn turner(common: &Common) -> Result<Loaded> {
    let rpc_url = common
        .rpc_url
        .clone()
        .ok_or_else(|| anyhow!("--rpc-url is required (or set RELAY_RPC_URL)"))?;
    let keeper = match &common.keypair {
        Some(path) => {
            Keypair::read_from_file(path).map_err(|err| anyhow!("read keypair {path}: {err}"))?
        }
        // Read-only commands never sign, so an ephemeral key is honest:
        // it makes "I could not have sent this" structural.
        None => Keypair::new(),
    };
    let defaults = TurnerConfig::default();
    let config = TurnerConfig {
        relay_program: common.program_id.unwrap_or(defaults.relay_program),
        min_crank_payment: common.min_crank_payment.unwrap_or(0),
        payout: common.payout_address,
        trusted_programs: common.trusted_program.iter().copied().collect(),
        guard_payments: !common.no_guard,
        filter: WatchFilter {
            allowed_target_programs: common.target_program.iter().copied().collect(),
            blocked_target_programs: common.blocked_program.iter().copied().collect(),
            allowed_creators: common.allowed_creator.iter().copied().collect(),
            ..WatchFilter::default()
        },
        ..TurnerConfig::default()
    };
    // Local simulation, same as the daemon: a resolver that behaves
    // differently under provider simulation is a difference worth not
    // introducing while debugging.
    let source: std::sync::Arc<dyn ChainSource> = std::sync::Arc::new(LocalSimSource::new(
        RpcSource::new(rpc_url),
        LocalSimConfig {
            pool_size: 2,
            // Without a keypair the fee payer is ephemeral and has no
            // account, which litesvm rejects before any instruction runs.
            // Crediting it inside the simulation is what lets a read-only
            // run reach the resolver at all; with a real keypair the
            // account exists and this never applies.
            synthetic_fee_payer_lamports: common.keypair.is_none().then_some(1_000_000_000),
        },
    ));
    let mut turner = Turner::new(source, keeper, config.clone());
    let summary = turner
        .refresh_watches()
        .await
        .context("load the watch registry")?;
    Ok((turner, config, summary))
}

/// Watches are keyed by (target, offset); with one block per target the
/// offset is discoverable, so it should not have to be typed.
pub fn resolve_offset(
    turner: &Turner<std::sync::Arc<dyn ChainSource>>,
    target: Pubkey,
    offset: Option<u32>,
) -> Result<u32> {
    if let Some(offset) = offset {
        return Ok(offset);
    }
    let mut offsets = turner
        .watches()
        .iter()
        .filter(|w| w.target == target)
        .map(|w| w.offset);
    let first = offsets
        .next()
        .ok_or_else(|| anyhow!("no watch for {target}: nothing is registered against it"))?;
    match offsets.next() {
        None => Ok(first),
        Some(_) => Err(anyhow!(
            "{target} carries more than one condition block; pass --offset"
        )),
    }
}
