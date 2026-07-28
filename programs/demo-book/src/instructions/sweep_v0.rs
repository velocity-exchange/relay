use anchor_lang_v2::prelude::*;

use crate::error::DemoError;
use crate::state::{pay_keeper, BookV0};

#[derive(Accounts)]
pub struct SweepV0 {
    /// Payment recipient. No signer requirement anywhere — cranks are
    /// permissionless; whoever lands the transaction names the keeper.
    #[account(mut)]
    pub keeper: UncheckedAccount,
    #[account(mut)]
    pub book: Account<BookV0>,
}

#[derive(Clone, wincode::SchemaRead, wincode::SchemaWrite)]
pub struct SweepArgsV0 {
    pub ids: Vec<u64>,
}

/// Executor for the sweep condition. The instruction is the authoritative
/// predicate: it fails unless every named entry is genuinely expired, so a
/// turner simulating against a stale view filters itself out. Repairs the
/// `next_expiry_ts` hint to the true minimum as it goes.
pub fn handle_sweep_v0(ctx: &mut Context<SweepV0>, args: SweepArgsV0) -> Result<()> {
    require!(!args.ids.is_empty(), DemoError::NothingToSweep);
    let now = Clock::get()?.unix_timestamp;

    {
        let book = &mut *ctx.accounts.book;
        args.ids.iter().try_for_each(|id| -> Result<()> {
            let slot = book.find_live(*id).ok_or(DemoError::EntryNotFound)?;
            require!(book.entry_expiries[slot] <= now, DemoError::EntryNotExpired);
            book.remove(slot);
            Ok(())
        })?;
        book.repair_next_expiry();
    }

    let payment = ctx.accounts.book.payment_per_crank;
    pay_keeper(
        ctx.accounts.book.view(),
        ctx.accounts.keeper.as_ref(),
        payment,
    )
}
