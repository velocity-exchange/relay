use anchor_lang_v2::prelude::*;

use crate::state::WatchV0;

#[derive(Accounts)]
pub struct RegisterWatchV0 {
    pub creator: Signer,
    /// Account carrying the condition block. Its *contents* are not
    /// validated here — a watch pointing at data that doesn't parse as a
    /// condition block is inert (turners skip it), so registration stays
    /// permissionless and the registry can never be wedged by garbage. Its
    /// **owner** is recorded, so turners can filter the registry by program
    /// without trusting the creator.
    pub target: UncheckedAccount,
    /// Pre-created zeroed account of exactly [`crate::state::WATCH_ACCOUNT_LEN`]
    /// bytes, owned by this program.
    #[account(zeroed)]
    pub watch: Account<WatchV0>,
}

#[derive(Clone, Copy, wincode::SchemaRead, wincode::SchemaWrite)]
pub struct RegisterWatchArgsV0 {
    pub offset: u32,
}

pub fn handle_register_watch_v0(
    ctx: &mut Context<RegisterWatchV0>,
    args: RegisterWatchArgsV0,
) -> Result<()> {
    let creator = *ctx.accounts.creator.address();
    let target = *ctx.accounts.target.address();
    // Read from the account, never from args: a creator must not be able
    // to claim someone else's program to slip past a turner's allowlist.
    let target_program = *ctx.accounts.target.owner();
    let watch = &mut *ctx.accounts.watch;
    watch.creator = creator;
    watch.target = target;
    watch.target_program = target_program;
    watch.offset = args.offset;
    Ok(())
}
