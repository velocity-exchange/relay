use anchor_lang_v2::prelude::*;

use crate::error::RelayError;
use crate::state::WatchV0;

#[derive(Accounts)]
pub struct CloseWatchV0 {
    #[account(mut)]
    pub watch: Account<WatchV0>,
    /// Receives the watch account's rent.
    #[account(mut, address = watch.creator @ RelayError::InvalidCreator)]
    pub creator: Signer,
}

pub fn handle_close_watch_v0(ctx: &mut Context<CloseWatchV0>) -> Result<()> {
    let destination = *ctx.accounts.creator.as_ref();
    ctx.accounts.watch.close(destination)?;
    Ok(())
}
