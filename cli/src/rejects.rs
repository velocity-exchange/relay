//! Why a registered watch never enters the tracked set.
//!
//! A watch rejected at refresh is invisible to every other command — it is
//! not fetched, not subscribed, and not cranked — so this is the only place
//! an operator finds out it exists at all.

use anyhow::anyhow;
use solana_sdk::pubkey::Pubkey;

use relay_crank_turner::{RefreshSummary, RejectReason};

use crate::render;

/// What a rejection means and what to do about it. A watch rejected at
/// refresh is invisible to every other command — it is not fetched, not
/// subscribed, and not cranked — so this is the only place an operator
/// finds out it exists at all.
pub fn reject_advice(reason: &RejectReason) -> &'static str {
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

pub fn reject_name(reason: &RejectReason) -> &'static str {
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
pub fn rejection_error(summary: &RefreshSummary, target: &Pubkey) -> Option<anyhow::Error> {
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
pub fn print_rejected(summary: &RefreshSummary) -> bool {
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
