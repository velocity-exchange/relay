use anchor_lang_v2::prelude::*;
use relay_spec::{ResolvedCrankV0, ResponsePointerV0, RESPONSE_POINTER_LEN};

use crate::state::BookV0;

#[derive(Accounts)]
pub struct ResolveEvictV0 {
    /// Writable for staging only — see `resolve_sweep_v0`.
    #[account(mut)]
    pub book: Account<BookV0>,
}

/// Resolver for the evict condition: work exists once the book is at/above
/// its soft cap; the victim is the oldest live entry.
pub fn handle_resolve_evict_v0(
    ctx: &mut Context<ResolveEvictV0>,
) -> Result<[u8; RESPONSE_POINTER_LEN]> {
    let own_address = *ctx.accounts.book.view().address();
    let book = &mut *ctx.accounts.book;

    let victim = (book.entry_count >= book.evict_threshold)
        .then(|| book.oldest_live())
        .flatten();

    match victim {
        Some((_, id)) => book.stage_payload(&ResolvedCrankV0 {
            accounts: BookV0::executor_accounts(&own_address),
            // Executor args: the borsh wire of `EvictArgsV0 { id: u64 }`.
            data: id.to_le_bytes().to_vec(),
        }),
        None => Ok(ResponsePointerV0::no_work().to_bytes()),
    }
}
