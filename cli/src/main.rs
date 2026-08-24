//! `relay` — the debugging CLI for a relay deployment.
//!
//! The premise: when a condition is not being cranked and you think it
//! should be, the answer is one of about a dozen gates, and guessing which
//! is miserable. So every verdict this tool prints comes from
//! [`Turner::explain`], which runs the daemon's own `decide` and crank path
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

mod render;

use anyhow::{anyhow, Context, Result};
use clap::{Args as ClapArgs, Parser, Subcommand};
use relay_chain_source::{ChainSource, LocalSimConfig, LocalSimSource, RpcSource};
use relay_crank_turner::{
    Explanation, RefreshSummary, RejectReason, Turner, TurnerConfig, Verdict, Watch, WatchFilter,
};
use serde_json::json;
use solana_sdk::pubkey::Pubkey;
use solana_sdk::signature::Keypair;
use solana_sdk::signer::EncodableKey;

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

#[derive(ClapArgs, Clone)]
struct Common {
    #[arg(long, env = "RELAY_RPC_URL", global = true)]
    rpc_url: Option<String>,
    /// relay program id.
    #[arg(long, env = "RELAY_PROGRAM_ID", global = true)]
    program_id: Option<Pubkey>,
    /// Keeper keypair. Only needed to send; read-only commands use an
    /// ephemeral key, which is fine because nothing is signed.
    #[arg(long, env = "RELAY_KEEPER_KEYPAIR", global = true)]
    keypair: Option<String>,
    /// Emit JSON instead of tables.
    #[arg(long, global = true)]
    json: bool,

    // --- the daemon's own config, so verdicts match production ---
    /// Mirror the daemon's `--min-crank-payment`.
    #[arg(long, env = "RELAY_MIN_CRANK_PAYMENT", global = true)]
    min_crank_payment: Option<u64>,
    /// Mirror the daemon's `--target-program`.
    #[arg(
        long,
        env = "RELAY_TARGET_PROGRAMS",
        value_delimiter = ',',
        global = true
    )]
    target_program: Vec<Pubkey>,
    /// Mirror the daemon's `--blocked-program`.
    #[arg(
        long,
        env = "RELAY_BLOCKED_PROGRAMS",
        value_delimiter = ',',
        global = true
    )]
    blocked_program: Vec<Pubkey>,
    /// Mirror the daemon's `--allowed-creator`.
    #[arg(
        long,
        env = "RELAY_ALLOWED_CREATORS",
        value_delimiter = ',',
        global = true
    )]
    allowed_creator: Vec<Pubkey>,
    /// Mirror the daemon's `--trusted-program`.
    #[arg(
        long,
        env = "RELAY_TRUSTED_PROGRAMS",
        value_delimiter = ',',
        global = true
    )]
    trusted_program: Vec<Pubkey>,
    /// Mirror the daemon's `--payout-address`.
    #[arg(long, env = "RELAY_PAYOUT_ADDRESS", global = true)]
    payout_address: Option<Pubkey>,
    /// Mirror the daemon's `--no-guard`.
    #[arg(long, global = true)]
    no_guard: bool,
    /// A running daemon's metrics endpoint, for the state a fresh process
    /// cannot see: backoff, contention delay, profitability.
    #[arg(long, env = "RELAY_METRICS_URL", global = true)]
    metrics_url: Option<String>,
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
    match &cli.command {
        Command::Watch(WatchCommand::List { rejected }) => watch_list(&cli.common, *rejected).await,
        Command::Watch(WatchCommand::Get { target }) => watch_get(&cli.common, *target).await,
        Command::Condition(ConditionCommand::List { due }) => {
            condition_list(&cli.common, *due).await
        }
        Command::Condition(ConditionCommand::Explain {
            target,
            index,
            offset,
        }) => explain(&cli.common, *target, *index, *offset).await,
        Command::Condition(ConditionCommand::Run {
            target,
            index,
            offset,
            send,
        }) => run_condition(&cli.common, *target, *index, *offset, *send).await,
        Command::Guard { payout, nonce } => guard(&cli.common, *payout, *nonce).await,
        Command::Clock => clock(&cli.common).await,
        Command::Doctor => doctor(&cli.common).await,
    }
}

/// A turner configured exactly as the daemon would be, with its registry
/// loaded. Everything else in this file is presentation.
type Loaded = (
    Turner<std::sync::Arc<dyn ChainSource>>,
    TurnerConfig,
    RefreshSummary,
);

async fn turner(common: &Common) -> Result<Loaded> {
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

/// What a rejection means and what to do about it. A watch rejected at
/// refresh is invisible to every other command — it is not fetched, not
/// subscribed, and not cranked — so this is the only place an operator
/// finds out it exists at all.
fn reject_advice(reason: &RejectReason) -> &'static str {
    match reason {
        RejectReason::ProgramNotAllowed => {
            "its target program is not in --target-program (this turner's allowlist)"
        }
        RejectReason::ProgramBlocked => "its target program is in --blocked-program",
        RejectReason::CreatorNotAllowed => "whoever registered it is not in --allowed-creator",
        RejectReason::TargetNotAllowed => "its target is not in this turner's target allowlist",
        RejectReason::TargetTooLarge => "the target account is bigger than --max-target-bytes",
        RejectReason::TargetMissing => {
            "the target account does not exist — it was closed after the watch was registered"
        }
        RejectReason::OwnerDrift => {
            "the target's owner no longer matches the program recorded at registration; \
             re-register the watch"
        }
        RejectReason::Unparseable => {
            "the bytes at the watch offset are not a readable condition block: the offset \
             is wrong, the target's layout changed without re-registering, or a wake kind \
             is one this build does not know"
        }
        RejectReason::PaysTooLittle => "no active condition in the block pays --min-crank-payment",
        RejectReason::OverCapacity => "--max-watches was reached before this one",
    }
}

fn reject_name(reason: &RejectReason) -> &'static str {
    match reason {
        RejectReason::ProgramNotAllowed => "program not allowed",
        RejectReason::ProgramBlocked => "program blocked",
        RejectReason::CreatorNotAllowed => "creator not allowed",
        RejectReason::TargetNotAllowed => "target not allowed",
        RejectReason::TargetTooLarge => "target too large",
        RejectReason::TargetMissing => "target missing",
        RejectReason::OwnerDrift => "owner drift",
        RejectReason::Unparseable => "unparseable block",
        RejectReason::PaysTooLittle => "pays too little",
        RejectReason::OverCapacity => "over capacity",
    }
}

/// The reason this exact target was rejected, if it was. This is the case
/// that would otherwise be a dead end: the watch is registered on chain, so
/// the operator is right that it exists, but it never enters the tracked set
/// and so is absent from every other view.
fn rejection_error(summary: &RefreshSummary, target: &Pubkey) -> Option<anyhow::Error> {
    summary
        .rejected
        .iter()
        .find(|(watch, _)| watch.target == *target)
        .map(|(watch, reason)| {
            anyhow!(
                "{target} IS registered (offset {}, program {}) but this turner rejected it: \
                 {} — {}",
                watch.offset,
                watch.target_program,
                reject_name(reason),
                reject_advice(reason)
            )
        })
}

/// Print the rejected watches, if any. Returns whether it printed.
fn print_rejected(summary: &RefreshSummary) -> bool {
    if summary.rejected.is_empty() {
        return false;
    }
    println!(
        "\n{} registered watch(es) rejected by this configuration\n",
        summary.rejected.len()
    );
    let rows: Vec<Vec<String>> = summary
        .rejected
        .iter()
        .map(|(watch, reason)| {
            vec![
                watch.target.to_string(),
                watch.target_program.to_string(),
                watch.offset.to_string(),
                reject_name(reason).to_string(),
            ]
        })
        .collect();
    render::table(&["TARGET", "PROGRAM", "OFFSET", "REJECTED"], &rows);
    println!();
    summary.rejected.iter().for_each(|(watch, reason)| {
        println!("  {}: {}", watch.target, reject_advice(reason));
    });
    true
}

async fn watch_list(common: &Common, rejected: bool) -> Result<()> {
    let (turner, config, summary) = turner(common).await?;
    let watches = turner.watches();
    if common.json {
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({
                "relay_program": config.relay_program.to_string(),
                "watches": watches.iter().map(watch_json).collect::<Vec<_>>(),
            }))?
        );
        return Ok(());
    }
    println!(
        "{} watch(es) tracked by this configuration\n",
        watches.len()
    );
    let rows: Vec<Vec<String>> = watches
        .iter()
        .map(|w| {
            vec![
                w.target.to_string(),
                w.target_program.to_string(),
                w.creator.to_string(),
                w.offset.to_string(),
            ]
        })
        .collect();
    render::table(&["TARGET", "PROGRAM", "CREATOR", "OFFSET"], &rows);
    if rejected && !print_rejected(&summary) {
        println!(
            "\nNo watch was rejected: every one registered against {} is tracked.",
            config.relay_program
        );
    }
    if !rejected && !summary.rejected.is_empty() {
        println!(
            "\n{} registered watch(es) are being rejected by this configuration — \
             run with --rejected to see why.",
            summary.rejected.len()
        );
    }
    Ok(())
}

fn watch_json(w: &Watch) -> serde_json::Value {
    json!({
        "target": w.target.to_string(),
        "target_program": w.target_program.to_string(),
        "creator": w.creator.to_string(),
        "offset": w.offset,
    })
}

async fn watch_get(common: &Common, target: Pubkey) -> Result<()> {
    let (turner, _, summary) = turner(common).await?;
    if let Some(err) = rejection_error(&summary, &target) {
        return Err(err);
    }
    let watch = *turner
        .watches()
        .iter()
        .find(|w| w.target == target)
        .ok_or_else(|| {
            anyhow!(
                "no watch for {target} at all — nothing is registered against it. \
                 Check --program-id, and that the creator's transaction landed."
            )
        })?;
    let explanations = explain_all(&turner, &[watch]).await;
    if common.json {
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({
                "watch": watch_json(&watch),
                "conditions": explanations.iter().map(explanation_json).collect::<Vec<_>>(),
            }))?
        );
        return Ok(());
    }
    println!("target     {}", watch.target);
    println!("program    {}", watch.target_program);
    println!("creator    {}", watch.creator);
    println!("offset     {}\n", watch.offset);
    print_condition_table(&explanations);
    Ok(())
}

async fn condition_list(common: &Common, due_only: bool) -> Result<()> {
    let (turner, _, summary) = turner(common).await?;
    let watches: Vec<Watch> = turner.watches().to_vec();
    let mut explanations = explain_all(&turner, &watches).await;
    if due_only {
        explanations.retain(|e| matches!(e.verdict, Verdict::WouldSend { .. }));
    }
    if common.json {
        println!(
            "{}",
            serde_json::to_string_pretty(
                &explanations
                    .iter()
                    .map(explanation_json)
                    .collect::<Vec<_>>()
            )?
        );
        return Ok(());
    }
    println!("{} condition(s)\n", explanations.len());
    print_condition_table(&explanations);
    if !due_only {
        println!(
            "\nBackoff, contention delay, and profitability are per-process and \
             not visible here — pass --metrics-url to read them off a running \
             daemon, or use `relay condition explain` for one condition."
        );
        if !summary.rejected.is_empty() {
            println!(
                "{} registered watch(es) are rejected outright and have no conditions \
                 listed above — `relay watch list --rejected`.",
                summary.rejected.len()
            );
        }
    }
    Ok(())
}

/// Explain every condition of every watch. Sequential on purpose: this is a
/// debugging tool, and a stampede of RPC calls from an operator's laptop is
/// a worse failure than a slow command.
async fn explain_all(
    turner: &Turner<std::sync::Arc<dyn ChainSource>>,
    watches: &[Watch],
) -> Vec<Explanation> {
    let mut out = Vec::new();
    for watch in watches {
        // Conditions per block are fixed-size and small; walk until the
        // index runs past the end.
        for index in 0..u8::MAX {
            match turner.explain((watch.target, watch.offset, index)).await {
                Ok(explanation) => out.push(explanation),
                Err(_) => break,
            }
        }
    }
    out
}

fn print_condition_table(explanations: &[Explanation]) {
    let rows: Vec<Vec<String>> = explanations
        .iter()
        .map(|e| {
            let wake = render::wake_detail(&e.condition, &e.clock, e.watched_now.as_deref());
            vec![
                format!("{}", e.key.2),
                truncate(&e.key.0.to_string()),
                wake.kind.to_string(),
                wake.remaining
                    .map_or_else(|| "-".to_string(), |n| n.to_string()),
                e.condition.min_payment().to_string(),
                render::verdict_line(&e.verdict),
            ]
        })
        .collect();
    render::table(
        &["IDX", "TARGET", "WAKE", "REMAIN", "PAYS", "STATUS"],
        &rows,
    );
}

fn truncate(pubkey: &str) -> String {
    format!("{}…{}", &pubkey[..8], &pubkey[pubkey.len() - 4..])
}

fn explanation_json(e: &Explanation) -> serde_json::Value {
    let wake = render::wake_detail(&e.condition, &e.clock, e.watched_now.as_deref());
    json!({
        "target": e.key.0.to_string(),
        "offset": e.key.1,
        "index": e.key.2,
        "program": e.program.to_string(),
        "creator": e.creator.to_string(),
        "active": e.condition.is_active(),
        "min_payment": e.condition.min_payment(),
        "wake": {
            "kind": wake.kind,
            "waiting_for": wake.waiting_for,
            "chain_reads": wake.chain_reads,
            "remaining": wake.remaining.map(|n| n as i64),
        },
        "clock": {"slot": e.clock.slot, "unix_timestamp": e.clock.unix_timestamp},
        "verdict": render::verdict_json(&e.verdict),
    })
}

/// The command this tool exists for.
async fn explain(common: &Common, target: Pubkey, index: u8, offset: Option<u32>) -> Result<()> {
    let (turner, config, summary) = turner(common).await?;
    if let Some(err) = rejection_error(&summary, &target) {
        return Err(err);
    }
    let offset = resolve_offset(&turner, target, offset)?;
    let explanation = turner.explain((target, offset, index)).await?;

    if common.json {
        let mut value = explanation_json(&explanation);
        value["daemon"] = daemon_state(common, &explanation.program).await;
        println!("{}", serde_json::to_string_pretty(&value)?);
        return Ok(());
    }

    let e = &explanation;
    let wake = render::wake_detail(&e.condition, &e.clock, e.watched_now.as_deref());
    println!(
        "condition  {} offset {} index {}",
        e.key.0, e.key.1, e.key.2
    );
    println!("program    {}", e.program);
    println!("creator    {}", e.creator);
    println!(
        "chain      slot {}, unix_timestamp {}\n",
        e.clock.slot, e.clock.unix_timestamp
    );

    // The gates, in the order the daemon applies them.
    println!("gates");
    gate("registered", true, "found in the watch registry");
    gate(
        "active",
        e.condition.is_active(),
        if e.condition.is_active() {
            "active flag set"
        } else {
            "active flag CLEAR — the target program has switched this condition off"
        },
    );
    let pays = e.condition.min_payment() >= config.min_crank_payment;
    gate(
        "pays enough",
        pays,
        &format!(
            "advertises {} lamports, this config requires {}",
            e.condition.min_payment(),
            config.min_crank_payment
        ),
    );
    let due = !matches!(
        e.verdict,
        Verdict::Skipped(relay_crank_turner::SkipReason::NotDue)
    );
    gate(
        "wake due",
        due,
        &format!(
            "{}: waiting for {}, chain reads {}{}",
            wake.kind,
            wake.waiting_for,
            wake.chain_reads,
            wake.remaining
                .map_or_else(String::new, |n| format!(" ({n} to go)")),
        ),
    );
    println!();

    match &e.verdict {
        Verdict::WouldSend {
            min_payment,
            units,
            instructions,
        } => {
            println!(
                "VERDICT  ready to crank — resolver found work, simulation passed, \
                 pays {min_payment} lamports for {units} compute units"
            );
            println!("\ntransaction it would send ({} ix):", instructions.len());
            instructions.iter().enumerate().for_each(|(i, ix)| {
                println!("  {i}. {} ({} accounts)", ix.program_id, ix.accounts.len());
            });
            println!(
                "\nIf the daemon is not sending this, the difference is its own state \
                 or its config. Check the daemon section below, and confirm this \
                 command was run with the same --min-crank-payment, filters, \
                 --payout-address and --trusted-program as the daemon."
            );
        }
        Verdict::NoWork => println!(
            "VERDICT  wake fired but the resolver reported nothing to do. This is \
             the designed cheap path for a conservative hint — it costs a local \
             simulation and no transaction. If you expected work, the bug is in \
             the target program's resolver, not in relay."
        ),
        Verdict::Skipped(reason) => {
            println!("VERDICT  skipped: {}", render::skip_reason(reason));
            println!("\n{}", skip_advice(reason));
        }
        Verdict::Failed { stage, error } => {
            println!(
                "VERDICT  failed at {}\n\n{error}",
                render::stage_name(stage)
            );
            println!(
                "\nA failure here is what stops the daemon too, and it will keep \
                 retrying under exponential backoff. Fix the target program or the \
                 accounts it resolves."
            );
        }
    }

    if e.stateless {
        println!(
            "\nnote  this process has no tick history, so a change-wake always reads \
             as due and neither backoff nor contention delay can be reported. Those \
             are daemon state:"
        );
    }
    print_daemon_state(common, &e.program).await;
    Ok(())
}

fn gate(name: &str, ok: bool, detail: &str) {
    println!(
        "  [{}] {:<12} {}",
        if ok { "ok" } else { "XX" },
        name,
        detail
    );
}

fn skip_advice(reason: &relay_crank_turner::SkipReason) -> &'static str {
    use relay_crank_turner::SkipReason as R;
    match reason {
        R::NotDue => {
            "The wake has not come due. For a timestamp or slot wake, compare the \
             two numbers above. For a change wake, the daemon fires when the \
             watched bytes differ from what it last saw — if the target program \
             never writes that range, the wake never fires, and the condition \
             wants an every-slots fallback."
        }
        R::Inactive => {
            "The target program cleared the condition's active flag. Nothing on the \
             relay side will crank it until the program sets it again."
        }
        R::BelowMinPayment => {
            "This turner is configured to ignore work this cheap. Lower \
             --min-crank-payment, or have the target program advertise more."
        }
        R::ParseFailed => {
            "The bytes at the watch offset are not a readable condition block. \
             Either the offset is wrong (check `relay watch get`), the target's \
             layout changed without re-registering, or the wake kind is one this \
             build does not know."
        }
        R::Backoff => {
            "Suppressed by this process's own backoff, which a fresh CLI run \
             should not normally hit. In the daemon it means a recent failure or \
             a just-sent crank."
        }
        R::ContentionDelay => {
            "Held back on purpose: this program's transactions keep reverting, so \
             the turner is arriving late to let a rival land first and be caught \
             by simulation instead of a burned fee. It decays as cranks land \
             again. Set --contention-step-slots 0 on the daemon to disable."
        }
        R::Unprofitable => {
            "This program's recent cranks cost more than they paid, and the \
             daemon's --min-program-profit floor stopped it."
        }
        R::NoSafePayout => {
            "The program is not trusted and no --payout-address is set, so there \
             is nowhere safe for it to pay: signer status is transaction-global, \
             so an untrusted executor handed the fee payer could drain it. Set a \
             payout account that never signs, or trust the program explicitly."
        }
        R::ExecutorNamedSigner => {
            "The resolver put a transaction signer in the executor's account \
             list. That is a drain attempt, and the turner refused to build it. \
             Treat the target program as hostile."
        }
    }
}

async fn run_condition(
    common: &Common,
    target: Pubkey,
    index: u8,
    offset: Option<u32>,
    send: bool,
) -> Result<()> {
    let (turner, _, summary) = turner(common).await?;
    if let Some(err) = rejection_error(&summary, &target) {
        return Err(err);
    }
    let offset = resolve_offset(&turner, target, offset)?;
    let explanation = turner.explain((target, offset, index)).await?;
    match &explanation.verdict {
        Verdict::WouldSend {
            min_payment,
            units,
            instructions,
        } => {
            println!(
                "ready: {} instruction(s), pays {min_payment} lamports, {units} CU",
                instructions.len()
            );
            if !send {
                println!("dry run — pass --send to submit");
                return Ok(());
            }
            if common.keypair.is_none() {
                return Err(anyhow!(
                    "--send needs --keypair: an ephemeral key cannot pay for a transaction"
                ));
            }
            let signature = turner.send_explained(&explanation).await?;
            println!("sent {signature}");
        }
        other => {
            println!("nothing to run: {}", render::verdict_line(other));
            println!("run `relay condition explain` for why");
        }
    }
    Ok(())
}

/// Watches are keyed by (target, offset); with one block per target the
/// offset is discoverable, so it should not have to be typed.
fn resolve_offset(
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

async fn guard(common: &Common, payout: Pubkey, nonce: u8) -> Result<()> {
    let (turner, _, _) = turner(common).await?;
    let address = turner.guard_address(payout, nonce);
    let account = turner.source().get_multiple_accounts(&[address]).await?;
    let Some(account) = account.into_iter().next().flatten() else {
        println!("guard {address} does not exist yet (created on first guarded crank)");
        return Ok(());
    };
    // GuardV0: 8 disc + payout(32) + snapshot(8) + armed(1) + bump(1) + nonce(1).
    // Sliced rather than indexed: the address is a PDA, but what lives
    // there is whatever the chain says, and a debugging tool must report
    // that rather than panic on it.
    let fields = account.data.get(40..49).ok_or_else(|| {
        anyhow!(
            "{address} is only {} bytes: not a guard",
            account.data.len()
        )
    })?;
    let snapshot = u64::from_le_bytes(fields[..8].try_into().expect("eight bytes"));
    let armed = fields[8] != 0;
    if common.json {
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({
                "address": address.to_string(),
                "payout": payout.to_string(),
                "nonce": nonce,
                "armed": armed,
                "snapshot": snapshot,
            }))?
        );
        return Ok(());
    }
    println!("guard     {address}");
    println!("payout    {payout}");
    println!("nonce     {nonce}");
    println!("armed     {armed}");
    println!("snapshot  {snapshot} lamports");
    if armed {
        println!(
            "\nAn armed guard between transactions means a begin_guard landed \
             without its assert_paid — normally impossible, since they share a \
             transaction. Harmless: the next begin_guard re-arms it."
        );
    }
    Ok(())
}

async fn clock(common: &Common) -> Result<()> {
    let (turner, _, _) = turner(common).await?;
    let clock = turner.source().clock().await?;
    if common.json {
        println!(
            "{}",
            serde_json::to_string_pretty(
                &json!({"slot": clock.slot, "unix_timestamp": clock.unix_timestamp})
            )?
        );
    } else {
        println!("slot            {}", clock.slot);
        println!("unix_timestamp  {}", clock.unix_timestamp);
    }
    Ok(())
}

async fn doctor(common: &Common) -> Result<()> {
    let (turner, config, summary) = turner(common).await?;
    let watches: Vec<Watch> = turner.watches().to_vec();
    let explanations = explain_all(&turner, &watches).await;

    let count = |f: &dyn Fn(&Explanation) -> bool| explanations.iter().filter(|e| f(e)).count();
    let ready = count(&|e| matches!(e.verdict, Verdict::WouldSend { .. }));
    let no_work = count(&|e| matches!(e.verdict, Verdict::NoWork));
    let failed = count(&|e| matches!(e.verdict, Verdict::Failed { .. }));
    let inactive = count(&|e| !e.condition.is_active());

    if common.json {
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({
                "relay_program": config.relay_program.to_string(),
                "watches": watches.len(),
                "conditions": explanations.len(),
                "ready": ready,
                "no_work": no_work,
                "failed": failed,
                "inactive": inactive,
                "rejected": summary.rejected.iter().map(|(watch, reason)| json!({
                    "target": watch.target.to_string(),
                    "offset": watch.offset,
                    "reason": reject_name(reason),
                    "advice": reject_advice(reason),
                })).collect::<Vec<_>>(),
                "detail": explanations.iter().map(explanation_json).collect::<Vec<_>>(),
            }))?
        );
        return Ok(());
    }

    println!("relay program  {}", config.relay_program);
    println!("watches        {}", watches.len());
    println!(
        "conditions     {} ({ready} ready, {no_work} no work, {failed} failing, {inactive} inactive)\n",
        explanations.len()
    );
    if watches.is_empty() {
        println!(
            "No watches tracked. Either nothing is registered against this program \
             id, or every watch is being rejected — check --program-id first."
        );
        print_rejected(&summary);
        print_daemon_state(common, &config.relay_program).await;
        return Ok(());
    }
    print_condition_table(&explanations);

    let failing: Vec<&Explanation> = explanations
        .iter()
        .filter(|e| matches!(e.verdict, Verdict::Failed { .. }))
        .collect();
    if !failing.is_empty() {
        println!("\nfailing conditions");
        failing.iter().for_each(|e| {
            if let Verdict::Failed { stage, error } = &e.verdict {
                println!(
                    "  {} idx {} at {}: {}",
                    truncate(&e.key.0.to_string()),
                    e.key.2,
                    render::stage_name(stage),
                    error.lines().next().unwrap_or_default()
                );
            }
        });
    }
    print_rejected(&summary);
    print_daemon_state(common, &config.relay_program).await;
    Ok(())
}

// --- the daemon's own state, which a fresh process cannot infer ---

async fn print_daemon_state(common: &Common, program: &Pubkey) {
    let Some(url) = &common.metrics_url else {
        println!(
            "\nPass --metrics-url http://host:9899/metrics to also report the \
             running daemon's backoff, contention delay and profitability."
        );
        return;
    };
    match scrape(url).await {
        Ok(metrics) => {
            println!("\ndaemon ({url})");
            let label = short_program(program);
            let show = |name: &str, needle: &str| {
                let value: Vec<&str> = metrics.lines().filter(|l| l.starts_with(needle)).collect();
                if value.is_empty() {
                    println!("  {name}: (no samples)");
                } else {
                    value.iter().for_each(|line| println!("  {line}"));
                }
            };
            show("contention delay", "relay_contention_delay_slots");
            show("cranks", "relay_cranks_total");
            show("failures", "relay_crank_failures_total");
            show("transactions", "relay_transactions_total");
            show("cache", "chain_cache_reads_total");
            show("reorgs", "chain_reorgs_total");
            if !metrics.contains(&label) {
                println!(
                    "  note: no samples labelled {label} — this daemon has never \
                     cranked the program you asked about, which usually means its \
                     filters exclude it."
                );
            }
        }
        Err(err) => println!("\ndaemon ({url}): unreachable ({err:#})"),
    }
}

/// Prometheus labels programs by a short prefix, matching the turner.
fn short_program(program: &Pubkey) -> String {
    program.to_string().chars().take(8).collect()
}

async fn daemon_state(common: &Common, program: &Pubkey) -> serde_json::Value {
    let Some(url) = &common.metrics_url else {
        return json!(null);
    };
    match scrape(url).await {
        Ok(metrics) => json!({
            "url": url,
            "program_label": short_program(program),
            "metrics": metrics
                .lines()
                .filter(|l| l.starts_with("relay_") || l.starts_with("chain_"))
                .collect::<Vec<_>>(),
        }),
        Err(err) => json!({"url": url, "error": format!("{err:#}")}),
    }
}

/// A one-shot HTTP GET. Deliberately hand-rolled: a debugging CLI should not
/// drag a TLS stack in to read a plaintext metrics endpoint on a private
/// network.
async fn scrape(url: &str) -> Result<String> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let rest = url
        .strip_prefix("http://")
        .ok_or_else(|| anyhow!("metrics url must be http://host:port[/metrics]"))?;
    let (authority, path) = rest.split_once('/').unwrap_or((rest, "metrics"));
    let mut socket = tokio::net::TcpStream::connect(authority)
        .await
        .with_context(|| format!("connect {authority}"))?;
    socket
        .write_all(
            format!("GET /{path} HTTP/1.1\r\nHost: {authority}\r\nConnection: close\r\n\r\n")
                .as_bytes(),
        )
        .await?;
    let mut body = String::new();
    socket.read_to_string(&mut body).await?;
    Ok(body)
}
