//! The watch registry: what is registered to be cranked.

use anyhow::{anyhow, Result};
use solana_sdk::pubkey::Pubkey;

use relay_crank_turner::Watch;

use crate::commands::condition::{explain_all, explanation_json, print_condition_table};
use crate::rejects::print_rejected;
use crate::rejects::rejection_error;
use crate::render;
use crate::session::{turner, Common};
use serde_json::json;

pub async fn watch_list(common: &Common, rejected: bool) -> Result<()> {
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

pub async fn watch_get(common: &Common, target: Pubkey) -> Result<()> {
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
