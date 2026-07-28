use anchor_lang_v2::prelude::*;

use crate::state::WatchV0;

#[derive(Accounts)]
pub struct RegisterWatchV0 {
    pub registrar: Signer,
    /// Account carrying the condition block. Nothing about it is validated
    /// here: a watch pointing at data that doesn't parse as a condition
    /// block is inert (turners skip it), so registration stays
    /// permissionless and the registry can never be wedged by garbage.
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
    let registrar = *ctx.accounts.registrar.address();
    let target = *ctx.accounts.target.address();
    let watch = &mut *ctx.accounts.watch;
    watch.registrar = registrar;
    watch.target = target;
    watch.offset = args.offset;
    Ok(())
}
