//! Resolve, build, and simulate one condition — and, with `--send`, submit
//! exactly what was shown rather than re-deriving it.

use anyhow::{anyhow, Result};
use solana_sdk::pubkey::Pubkey;

use relay_crank_turner::Verdict;

use crate::rejects::rejection_error;
use crate::render;
use crate::session::{resolve_offset, turner, Common};

pub async fn run_condition(
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
