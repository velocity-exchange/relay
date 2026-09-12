//! The command this tool exists for: why is this condition cranking, or not.
//!
//! The gates are printed in the order the daemon applies them, and every
//! verdict comes from `Turner::explain` — the daemon's own `decide` and crank
//! path — so this cannot disagree with production about whether a condition
//! is due.

use anyhow::Result;
use solana_sdk::pubkey::Pubkey;

use relay_crank_turner::{Explanation, SkipReason, TurnerConfig, Verdict};

use crate::commands::condition::explanation_json;
use crate::daemon::{daemon_state, print_daemon_state};
use crate::rejects::rejection_error;
use crate::render;
use crate::session::{resolve_offset, turner, Common};

/// Why is this condition cranking, or not? Walks every gate.
pub async fn explain(
    common: &Common,
    target: Pubkey,
    index: u8,
    offset: Option<u32>,
) -> Result<()> {
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

    print_identity(&explanation);
    print_gates(&explanation, &config);
    print_verdict(&explanation);

    if explanation.stateless {
        println!(
            "\nnote  this process has no tick history, so a change-wake always reads \
             as due and neither backoff nor contention delay can be reported. Those \
             are daemon state:"
        );
    }
    print_daemon_state(common, &explanation.program).await;
    Ok(())
}

/// Which condition this is, and what the chain reads right now.
fn print_identity(e: &Explanation) {
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
}

/// The gates, in the order the daemon applies them.
fn print_gates(e: &Explanation, config: &TurnerConfig) {
    let wake = render::wake_detail(&e.condition, &e.clock, e.watched_now.as_deref());
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
    gate(
        "pays enough",
        e.condition.min_payment() >= config.min_crank_payment,
        &format!(
            "advertises {} lamports, this config requires {}",
            e.condition.min_payment(),
            config.min_crank_payment
        ),
    );
    gate(
        "wake due",
        !matches!(e.verdict, Verdict::Skipped(SkipReason::NotDue)),
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
}

/// What the turner concluded, and what to do about it.
fn print_verdict(e: &Explanation) {
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
        R::PaymentBelowCost => {
            "The crank pays less than this turner would spend landing it — the \
             base fee plus the priority fee it is bidding, plus any configured \
             margin. It is skipped in simulation rather than landed at a loss. \
             Either the fee market has moved above what the target program \
             offers, or --crank-margin is set higher than the work is worth."
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
