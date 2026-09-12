//! One-shot health sweep: the registry, every condition's status, and
//! anything that looks wrong.

use anyhow::Result;
use serde_json::json;

use relay_crank_turner::{Explanation, TurnerConfig, Verdict, Watch};

use crate::commands::condition::{explain_all, explanation_json, print_condition_table, truncate};
use crate::daemon::print_daemon_state;
use crate::rejects::{print_rejected, reject_advice, reject_name};
use crate::render;
use crate::session::{turner, Common};

/// How many conditions are in each state. Counted once and passed around,
/// so the summary line and the JSON cannot drift apart.
struct Tally {
    ready: usize,
    no_work: usize,
    failed: usize,
    inactive: usize,
}

impl Tally {
    fn of(explanations: &[Explanation]) -> Self {
        let count = |f: &dyn Fn(&Explanation) -> bool| explanations.iter().filter(|e| f(e)).count();
        Self {
            ready: count(&|e| matches!(e.verdict, Verdict::WouldSend { .. })),
            no_work: count(&|e| matches!(e.verdict, Verdict::NoWork)),
            failed: count(&|e| matches!(e.verdict, Verdict::Failed { .. })),
            inactive: count(&|e| !e.condition.is_active()),
        }
    }
}

pub async fn doctor(common: &Common) -> Result<()> {
    let (turner, config, summary) = turner(common).await?;
    let watches: Vec<Watch> = turner.watches().to_vec();
    let explanations = explain_all(&turner, &watches).await;
    let tally = Tally::of(&explanations);

    if common.json {
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({
                "relay_program": config.relay_program.to_string(),
                "watches": watches.len(),
                "conditions": explanations.len(),
                "ready": tally.ready,
                "no_work": tally.no_work,
                "failed": tally.failed,
                "inactive": tally.inactive,
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

    print_summary(&config, watches.len(), &explanations, &tally);
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
    print_failing(&explanations);
    print_rejected(&summary);
    print_daemon_state(common, &config.relay_program).await;
    Ok(())
}

fn print_summary(
    config: &TurnerConfig,
    watches: usize,
    explanations: &[Explanation],
    tally: &Tally,
) {
    let Tally {
        ready,
        no_work,
        failed,
        inactive,
    } = *tally;
    println!("relay program  {}", config.relay_program);
    println!("watches        {watches}");
    println!(
        "conditions     {} ({ready} ready, {no_work} no work, {failed} failing, {inactive} inactive)\n",
        explanations.len()
    );
}

/// The one-line cause of each failure, so a sweep says what to go and look
/// at rather than only how many things are wrong.
fn print_failing(explanations: &[Explanation]) {
    let failing: Vec<&Explanation> = explanations
        .iter()
        .filter(|e| matches!(e.verdict, Verdict::Failed { .. }))
        .collect();
    if failing.is_empty() {
        return;
    }
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
