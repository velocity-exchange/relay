use anchor_lang_v2::pinocchio::cpi::set_return_data;
use anchor_lang_v2::prelude::*;
use relay_spec::ResolvedCrankV0;

use crate::state::BookV0;

#[derive(Accounts)]
pub struct ResolveEvictV0 {
    pub book: Account<BookV0>,
}

/// Resolver for the evict condition: work exists once the book is at/above
/// its soft cap; the victim is the oldest live entry.
pub fn handle_resolve_evict_v0(ctx: &mut Context<ResolveEvictV0>) -> Result<()> {
    let own_address = *ctx.accounts.book.view().address();
    let book = &*ctx.accounts.book;

    let victim = if book.entry_count >= book.evict_threshold {
        book.oldest_live()
    } else {
        None
    };

    let resolved = match victim {
        Some((_, id)) => ResolvedCrankV0 {
            work: true,
            accounts: BookV0::executor_accounts(&own_address),
            // Executor args: the borsh wire of `EvictArgsV0 { id: u64 }`.
            data: id.to_le_bytes().to_vec(),
        },
        None => ResolvedCrankV0::no_work(),
    };
    let mut buf = [0u8; 128];
    let n = resolved.write_into(&mut buf).expect("fits");
    set_return_data(&buf[..n]);
    Ok(())
}
