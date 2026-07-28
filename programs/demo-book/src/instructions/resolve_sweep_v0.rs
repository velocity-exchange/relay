use anchor_lang_v2::pinocchio::cpi::set_return_data;
use anchor_lang_v2::prelude::*;
use relay_spec::ResolvedCrankV0;

use crate::state::{BookV0, MAX_SWEEP_IDS};

#[derive(Accounts)]
pub struct ResolveSweepV0 {
    pub book: Account<BookV0>,
}

/// Resolver for the sweep condition: enumerate expired entries and hand the
/// turner the executor's account list + args via return data. Read-only,
/// signerless, only ever simulated — but harmless if actually sent.
pub fn handle_resolve_sweep_v0(ctx: &mut Context<ResolveSweepV0>) -> Result<()> {
    let now = Clock::get()?.unix_timestamp;
    let own_address = *ctx.accounts.book.view().address();
    let book = &*ctx.accounts.book;

    let ids: Vec<u64> = (0..crate::state::MAX_ENTRIES)
        .filter(|&i| book.entry_live[i] == 1 && book.entry_expiries[i] <= now)
        .map(|i| book.entry_ids[i])
        .take(MAX_SWEEP_IDS)
        .collect();

    let resolved = if ids.is_empty() {
        ResolvedCrankV0::no_work()
    } else {
        // Executor args: the borsh wire of `SweepArgsV0 { ids: Vec<u64> }`.
        let data = (ids.len() as u32)
            .to_le_bytes()
            .into_iter()
            .chain(ids.iter().flat_map(|id| id.to_le_bytes()))
            .collect();
        ResolvedCrankV0 {
            work: true,
            accounts: BookV0::executor_accounts(&own_address),
            data,
        }
    };
    let mut buf = [0u8; 256];
    let n = resolved.write_into(&mut buf).expect("fits");
    set_return_data(&buf[..n]);
    Ok(())
}
