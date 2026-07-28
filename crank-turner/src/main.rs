use std::time::Duration;

use anyhow::{Context, Result};
use clap::Parser;
use relay_crank_turner::{Outcome, RpcSource, Turner, TurnerConfig};
use solana_sdk::pubkey::Pubkey;
use solana_sdk::signature::Keypair;
use solana_sdk::signer::EncodableKey;
use tracing::{info, warn};

#[derive(Parser)]
#[command(about = "relay crank turner: watch conditions, discover work via simulation, crank")]
struct Args {
    #[arg(long, env = "RELAY_RPC_URL")]
    rpc_url: String,
    /// Path to the keeper keypair (fee payer + payment recipient).
    #[arg(long, env = "RELAY_KEEPER_KEYPAIR")]
    keypair: String,
    /// relay program id.
    #[arg(long, env = "RELAY_PROGRAM_ID", default_value_t = TurnerConfig::default().relay_program)]
    program_id: Pubkey,
    /// Skip conditions advertising less than this many lamports.
    #[arg(long, env = "RELAY_MIN_CRANK_PAYMENT", default_value_t = 0)]
    min_crank_payment: u64,
    /// Milliseconds between ticks.
    #[arg(long, env = "RELAY_TICK_MS", default_value_t = 1000)]
    tick_ms: u64,
    /// Re-scan the watch registry every N ticks.
    #[arg(long, env = "RELAY_REFRESH_TICKS", default_value_t = 30)]
    refresh_ticks: u64,
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
    let config = TurnerConfig {
        relay_program: args.program_id,
        min_crank_payment: args.min_crank_payment,
        ..TurnerConfig::default()
    };
    let mut turner = Turner::new(RpcSource::new(args.rpc_url), keeper, config);
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
                Ok(n) => info!(watches = n, "registry refreshed"),
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
