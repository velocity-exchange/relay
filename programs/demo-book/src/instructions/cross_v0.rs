use anchor_lang_v2::prelude::*;

use crate::error::DemoError;
use crate::state::{pay_keeper, BookV0, SIDE_ASK, SIDE_BID};

#[derive(Accounts)]
pub struct CrossV0 {
    /// Payment recipient (see `sweep_v0` on the no-signer contract).
    #[account(mut)]
    pub keeper: UncheckedAccount,
    #[account(mut)]
    pub book: Account<BookV0>,
}

#[derive(Clone, Copy, wincode::SchemaRead, wincode::SchemaWrite)]
pub struct CrossArgsV0 {
    pub bid_id: u64,
    pub ask_id: u64,
}

/// Executor for the cross condition: match a crossed bid/ask pair.
///
/// Re-checks the crossing on chain rather than trusting the resolver's
/// choice — a turner simulating against a slightly stale book could name a
/// pair that no longer crosses, and this is what makes that harmless
/// (the transaction fails, nothing is half-done).
pub fn handle_cross_v0(ctx: &mut Context<CrossV0>, args: CrossArgsV0) -> Result<()> {
    {
        let book = &mut *ctx.accounts.book;
        let bid = book
            .find_live(args.bid_id)
            .ok_or(DemoError::EntryNotFound)?;
        let ask = book
            .find_live(args.ask_id)
            .ok_or(DemoError::EntryNotFound)?;
        require!(book.entry_sides[bid] == SIDE_BID, DemoError::WrongSide);
        require!(book.entry_sides[ask] == SIDE_ASK, DemoError::WrongSide);
        require!(
            book.entry_prices[bid] >= book.entry_prices[ask],
            DemoError::NotCrossing
        );
        book.remove(bid);
        book.remove(ask);
    }

    let payment = ctx.accounts.book.payment_per_crank;
    pay_keeper(
        ctx.accounts.book.view(),
        ctx.accounts.keeper.as_ref(),
        payment,
    )
}
