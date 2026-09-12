//! Every condition across every watch, and what the turner would do about
//! each.

use anyhow::Result;

use relay_chain_source::ChainSource;
use relay_crank_turner::{Explanation, Turner, Verdict};

use crate::render;
use crate::session::{turner, Common};
use relay_crank_turner::Watch;
use serde_json::json;

pub async fn condition_list(common: &Common, due_only: bool) -> Result<()> {
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
pub async fn explain_all(
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

pub fn print_condition_table(explanations: &[Explanation]) {
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

pub fn truncate(pubkey: &str) -> String {
    format!("{}…{}", &pubkey[..8], &pubkey[pubkey.len() - 4..])
}

pub fn explanation_json(e: &Explanation) -> serde_json::Value {
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
