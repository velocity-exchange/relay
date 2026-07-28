use anchor_lang_v2::prelude::*;

use crate::error::RelayError;
use crate::state::WatchV0;

#[derive(Accounts)]
pub struct CloseWatchV0 {
    #[account(mut)]
    pub watch: Account<WatchV0>,
    /// Receives the watch account's rent.
    #[account(mut, address = watch.registrar @ RelayError::InvalidRegistrar)]
    pub registrar: Signer,
}

pub fn handle_close_watch_v0(ctx: &mut Context<CloseWatchV0>) -> Result<()> {
    let destination = *ctx.accounts.registrar.as_ref();
    ctx.accounts.watch.close(destination)?;
    Ok(())
}
