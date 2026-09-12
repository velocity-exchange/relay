//! `relay` — the debugging CLI for a relay deployment.
//!
//! The premise: when a condition is not being cranked and you think it
//! should be, the answer is one of about a dozen gates, and guessing which
//! is miserable. So every verdict this tool prints comes from
//! `Turner::explain`, which runs the daemon's own `decide` and crank path
//! rather than a reimplementation. The CLI cannot disagree with production
//! about whether a condition is due — if it says READY and the daemon is not
//! cranking, the difference is configuration or per-process state, and
//! `relay condition explain` says which.
//!
//! Two classes of reason are worth keeping straight, because they need
//! different tools:
//!
//! - **On chain** — inactive, below min payment, wake not due, filtered out,
//!   resolver reports no work, simulation fails, executor asks for a
//!   signature. All visible here.
//! - **Per-process** — failure backoff, post-send suppression, adaptive
//!   contention delay, and the rolling profitability window. These live in
//!   the running daemon and a fresh process cannot see them, so `explain`
//!   scrapes the daemon's metrics endpoint (`--metrics-url`) to report them
//!   instead of pretending they do not exist.

mod commands;
mod daemon;
mod rejects;
mod render;
mod session;

use anyhow::Result;
use clap::{Parser, Subcommand};
use solana_sdk::pubkey::Pubkey;

use session::Common;

#[derive(Parser)]
#[command(
    name = "relay",
    about = "Inspect and debug a relay deployment: what is registered, what is due, and why something is not cranking"
)]
struct Cli {
    #[command(flatten)]
    common: Common,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// The watch registry: what is registered to be cranked.
    #[command(subcommand)]
    Watch(WatchCommand),
    /// Individual conditions, across every watch.
    #[command(subcommand)]
    Condition(ConditionCommand),
    /// A payment guard's state.
    Guard {
        /// The payout account the guard is seeded by.
        payout: Pubkey,
        #[arg(long, default_value_t = 0)]
        nonce: u8,
    },
    /// Chain clock and slot — the values timestamp and slot wakes compare
    /// against.
    Clock,
    /// One-shot health sweep: the registry, every condition's status, and
    /// anything that looks wrong.
    Doctor,
}

#[derive(Subcommand)]
enum WatchCommand {
    /// Every watch this configuration would track.
    List {
        /// Show watches this configuration filters out, and why.
        #[arg(long)]
        rejected: bool,
    },
    /// One watch, with its conditions decoded.
    Get { target: Pubkey },
}

#[derive(Subcommand)]
enum ConditionCommand {
    /// Every condition, with what the turner would do about it.
    List {
        /// Only conditions that would be cranked right now.
        #[arg(long)]
        due: bool,
    },
    /// Why is this condition cranking, or not? Walks every gate.
    Explain {
        target: Pubkey,
        #[arg(long, default_value_t = 0)]
        index: u8,
        /// Watch offset, if the target carries more than one block.
        #[arg(long)]
        offset: Option<u32>,
    },
    /// Resolve, build, and simulate one condition. Prints the transaction
    /// it would send; `--send` actually sends it.
    Run {
        target: Pubkey,
        #[arg(long, default_value_t = 0)]
        index: u8,
        #[arg(long)]
        offset: Option<u32>,
        /// Submit for real. Costs a transaction fee and does the work.
        #[arg(long)]
        send: bool,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    let common = &cli.common;
    match &cli.command {
        Command::Watch(WatchCommand::List { rejected }) => {
            commands::watch::watch_list(common, *rejected).await
        }
        Command::Watch(WatchCommand::Get { target }) => {
            commands::watch::watch_get(common, *target).await
        }
        Command::Condition(ConditionCommand::List { due }) => {
            commands::condition::condition_list(common, *due).await
        }
        Command::Condition(ConditionCommand::Explain {
            target,
            index,
            offset,
        }) => commands::explain::explain(common, *target, *index, *offset).await,
        Command::Condition(ConditionCommand::Run {
            target,
            index,
            offset,
            send,
        }) => commands::run::run_condition(common, *target, *index, *offset, *send).await,
        Command::Guard { payout, nonce } => commands::chain::guard(common, *payout, *nonce).await,
        Command::Clock => commands::chain::clock(common).await,
        Command::Doctor => commands::doctor::doctor(common).await,
    }
}
