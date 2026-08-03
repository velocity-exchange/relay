//! relay program (Anchor v2 / anchor-next).
//!
//! Two jobs, both small on purpose:
//!
//! 1. **Watch registry** — `WatchV0` accounts pointing crank turners at a
//!    `(target account, offset)` where a target program embeds a condition
//!    block. Registration is permissionless: a watch pointing at garbage
//!    parses as garbage and turners skip it, so there is nothing to gate.
//!    The creator can close its watch and reclaim rent.
//! 2. **Payment guards** — `begin_guard_v0` / `assert_paid_v0`, bracketing
//!    the executor instruction rather than wrapping it. The first snapshots
//!    the payout account's lamports, the second reverts the whole
//!    transaction unless they grew by at least the turner's asserted
//!    `min_payment`.
//!
//! Guards assert around the call, they do not mediate it: relay is never in
//! the executor's CPI stack, so all four levels stay available to the
//! executor's own calls. Guards are optional (`--no-guard`) — a turner that
//! trusts its simulation can submit the bare executor, and relay is then
//! not involved in execution at all.

use anchor_lang_v2::prelude::*;

pub mod error;
pub mod instructions;
pub mod state;

pub use instructions::*;
// Re-exported so integration tests can reach wincode/litesvm glue through the
// crate without their own git dep.
pub use anchor_lang_v2;

declare_id!("4D5tPhw9sqkdkR5CpmP427TH6y9p9AMuKUukUEHn3Mpu");

#[program]
pub mod relay {
    use super::*;

    pub fn register_watch_v0(
        ctx: &mut Context<RegisterWatchV0>,
        args: RegisterWatchArgsV0,
    ) -> Result<()> {
        instructions::register_watch_v0::handle_register_watch_v0(ctx, args)
    }

    pub fn close_watch_v0(ctx: &mut Context<CloseWatchV0>) -> Result<()> {
        instructions::close_watch_v0::handle_close_watch_v0(ctx)
    }

    pub fn begin_guard_v0(ctx: &mut Context<BeginGuardV0>, args: BeginGuardArgsV0) -> Result<()> {
        instructions::begin_guard_v0::handle_begin_guard_v0(ctx, args)
    }

    pub fn assert_paid_v0(ctx: &mut Context<AssertPaidV0>, args: AssertPaidArgsV0) -> Result<()> {
        instructions::assert_paid_v0::handle_assert_paid_v0(ctx, args)
    }
}
