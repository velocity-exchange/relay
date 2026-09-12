//! Raw chain state the turner reads but does not interpret: a payment
//! guard's account, and the clock that timestamp and slot wakes compare
//! against.

use anyhow::{anyhow, Result};
use serde_json::json;
use solana_sdk::pubkey::Pubkey;

use relay_chain_source::ChainSource;

use crate::session::{turner, Common};

pub async fn guard(common: &Common, payout: Pubkey, nonce: u8) -> Result<()> {
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

pub async fn clock(common: &Common) -> Result<()> {
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
