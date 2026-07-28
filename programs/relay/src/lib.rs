//! relay program (Anchor v2 / anchor-next).
//!
//! Two jobs, both small on purpose:
//!
//! 1. **Watch registry** — `WatchV0` accounts pointing crank turners at a
//!    `(target account, offset)` where a target program embeds a
//!    [`relay_spec::ConditionBlockV0`]. Registration is permissionless:
//!    a watch pointing at garbage parses as garbage and turners skip it, so
//!    there is nothing to gate. The registrar can close its watch and
//!    reclaim rent.
//! 2. **`crank_v0`** — a payment-asserting wrapper around a condition's
//!    executor instruction. It reads the condition from the target account,
//!    CPIs the executor with every account `is_signer: false`, and asserts
//!    the keeper account's lamports grew by at least `min_payment`. Turners
//!    that wrap their cranks get sim-time payment verification for free
//!    (sim success ⇒ paid) and armor against the sim-to-land race.
//!
//! `crank_v0` grants nothing: no signer privilege is forwarded, so anything
//! it can make an executor do, anyone could already do with a direct
//! transaction. Turners MAY submit executors unwrapped — the wrapper is
//! optional armor, not a toll booth.

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

    pub fn crank_v0(ctx: &mut Context<CrankV0>, args: CrankArgsV0) -> Result<()> {
        instructions::crank_v0::handle_crank_v0(ctx, args)
    }
}
