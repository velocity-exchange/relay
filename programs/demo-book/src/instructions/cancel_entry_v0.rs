use anchor_lang_v2::prelude::*;

use crate::error::DemoError;
use crate::state::BookV0;

#[derive(Accounts)]
pub struct CancelEntryV0 {
    #[account(mut)]
    pub book: Account<BookV0>,
    #[account(address = book.authority @ DemoError::InvalidAuthority)]
    pub authority: Signer,
}

#[derive(Clone, Copy, wincode::SchemaRead, wincode::SchemaWrite)]
pub struct CancelEntryArgsV0 {
    pub id: u64,
}

/// Deliberately does NOT repair `next_expiry_ts`: removal may only push the
/// true minimum later, so the hint goes stale-early — the direction the
/// contract allows. The turner's next sweep attempt resolves to no-work and
/// the executor repairs the hint when real work next appears.
pub fn handle_cancel_entry_v0(
    ctx: &mut Context<CancelEntryV0>,
    args: CancelEntryArgsV0,
) -> Result<()> {
    let book = &mut *ctx.accounts.book;
    let slot = book.find_live(args.id).ok_or(DemoError::EntryNotFound)?;
    book.remove(slot);
    Ok(())
}
