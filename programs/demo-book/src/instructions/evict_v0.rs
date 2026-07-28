use anchor_lang_v2::prelude::*;

use crate::error::DemoError;
use crate::state::{pay_keeper, BookV0};

#[derive(Accounts)]
pub struct EvictV0 {
    /// Payment recipient (see `sweep_v0` on the no-signer contract).
    #[account(mut)]
    pub keeper: UncheckedAccount,
    #[account(mut)]
    pub book: Account<BookV0>,
}

#[derive(Clone, Copy, wincode::SchemaRead, wincode::SchemaWrite)]
pub struct EvictArgsV0 {
    pub id: u64,
}

/// Executor for the evict condition. Fails below the threshold, so it can
/// never shrink a book that isn't at its soft cap. Does not repair the sweep
/// hint (eviction can only push the true minimum later — stale-early is
/// allowed); `entry_count` changing re-fires the dirty wake by itself.
pub fn handle_evict_v0(ctx: &mut Context<EvictV0>, args: EvictArgsV0) -> Result<()> {
    {
        let book = &mut *ctx.accounts.book;
        require!(
            book.entry_count >= book.evict_threshold,
            DemoError::BelowEvictThreshold
        );
        let slot = book.find_live(args.id).ok_or(DemoError::EntryNotFound)?;
        book.remove(slot);
    }

    let payment = ctx.accounts.book.payment_per_crank;
    pay_keeper(
        ctx.accounts.book.view(),
        ctx.accounts.keeper.as_ref(),
        payment,
    )
}
