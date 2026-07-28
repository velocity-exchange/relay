use anchor_lang_v2::prelude::*;
use relay_spec::{ResolvedCrankV0, ResponsePointerV0, RESPONSE_POINTER_LEN};

use crate::state::{BookV0, MAX_SWEEP_IDS};

#[derive(Accounts)]
pub struct ResolveSweepV0 {
    /// Writable only because the payload is staged here. The instruction is
    /// otherwise read-only, and turners only ever *simulate* it — the
    /// staged bytes are read out of the simulation's post-execution
    /// account state and never land on chain.
    #[account(mut)]
    pub book: Account<BookV0>,
}

/// Resolver for the sweep condition: enumerate expired entries, stage the
/// executor's account list + args, and return a pointer to them.
pub fn handle_resolve_sweep_v0(
    ctx: &mut Context<ResolveSweepV0>,
) -> Result<[u8; RESPONSE_POINTER_LEN]> {
    let now = Clock::get()?.unix_timestamp;
    let own_address = *ctx.accounts.book.view().address();
    let book = &mut *ctx.accounts.book;

    let ids: Vec<u64> = (0..crate::state::MAX_ENTRIES)
        .filter(|&i| book.entry_live[i] == 1 && book.entry_expiries[i] <= now)
        .map(|i| book.entry_ids[i])
        .take(MAX_SWEEP_IDS)
        .collect();

    if ids.is_empty() {
        // No staging write at all on the cheap path.
        return Ok(ResponsePointerV0::no_work().to_bytes());
    }
    // Executor args: the borsh wire of `SweepArgsV0 { ids: Vec<u64> }`.
    let data = (ids.len() as u32)
        .to_le_bytes()
        .into_iter()
        .chain(ids.iter().flat_map(|id| id.to_le_bytes()))
        .collect();
    book.stage(&ResolvedCrankV0 {
        accounts: BookV0::executor_accounts(&own_address),
        data,
    })
}
