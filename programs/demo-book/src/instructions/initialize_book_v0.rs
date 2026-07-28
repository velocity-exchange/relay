use anchor_lang_v2::prelude::*;

use crate::state::BookV0;

#[derive(Accounts)]
pub struct InitializeBookV0 {
    pub authority: Signer,
    /// Pre-created zeroed account of exactly [`crate::state::BOOK_ACCOUNT_LEN`]
    /// bytes, owned by this program. Fund it with extra lamports beyond rent
    /// exemption — that surplus is the crank-payment treasury.
    #[account(zeroed)]
    pub book: Account<BookV0>,
}

#[derive(Clone, Copy, wincode::SchemaRead, wincode::SchemaWrite)]
pub struct InitializeBookArgsV0 {
    pub payment_per_crank: u64,
    pub evict_threshold: u32,
}

pub fn handle_initialize_book_v0(
    ctx: &mut Context<InitializeBookV0>,
    args: InitializeBookArgsV0,
) -> Result<()> {
    let authority = *ctx.accounts.authority.address();
    let own_address = *ctx.accounts.book.view().address();
    let book = &mut *ctx.accounts.book;
    book.authority = authority;
    book.payment_per_crank = args.payment_per_crank;
    book.evict_threshold = args.evict_threshold;
    book.next_entry_id = 1;
    book.next_expiry_ts = i64::MAX;
    book.init_conditions(&own_address);
    Ok(())
}
