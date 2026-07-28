use anchor_lang_v2::prelude::*;

use crate::error::DemoError;
use crate::state::BookV0;

#[derive(Accounts)]
pub struct SetPaymentV0 {
    #[account(mut)]
    pub book: Account<BookV0>,
    #[account(address = book.authority @ DemoError::InvalidAuthority)]
    pub authority: Signer,
}

#[derive(Clone, Copy, wincode::SchemaRead, wincode::SchemaWrite)]
pub struct SetPaymentArgsV0 {
    pub payment_per_crank: u64,
}

/// Updates what executors actually pay WITHOUT rewriting the condition
/// block's `min_payment`. This is a deliberate divergence hook for testing
/// `crank_v0`'s payment assertion (the wrapper must fail when the executor
/// underpays the advertised amount) — a real program would rewrite its
/// conditions here.
pub fn handle_set_payment_v0(
    ctx: &mut Context<SetPaymentV0>,
    args: SetPaymentArgsV0,
) -> Result<()> {
    ctx.accounts.book.payment_per_crank = args.payment_per_crank;
    Ok(())
}
