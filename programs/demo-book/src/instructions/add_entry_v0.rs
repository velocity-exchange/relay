use anchor_lang_v2::prelude::*;

use crate::error::DemoError;
use crate::state::BookV0;

#[derive(Accounts)]
pub struct AddEntryV0 {
    #[account(mut)]
    pub book: Account<BookV0>,
    #[account(address = book.authority @ DemoError::InvalidAuthority)]
    pub authority: Signer,
}

#[derive(Clone, Copy, wincode::SchemaRead, wincode::SchemaWrite)]
pub struct AddEntryArgsV0 {
    /// May already be in the past — such an entry is sweepable immediately.
    pub expiry_ts: i64,
}

/// `insert` maintains the min-over-inserts hint itself: when this entry
/// brings the minimum down, the sweep wake update is a single field store
/// into the pod condition block.
pub fn handle_add_entry_v0(ctx: &mut Context<AddEntryV0>, args: AddEntryArgsV0) -> Result<()> {
    ctx.accounts.book.insert(args.expiry_ts)?;
    Ok(())
}
