use anchor_lang_v2::prelude::*;

use crate::error::RelayError;
use crate::state::GuardV0;

#[derive(Accounts)]
#[instruction(args: AssertPaidArgsV0)]
pub struct AssertPaidV0 {
    /// The keeper whose payment is asserted. Not required to sign: the
    /// assertion only ever *fails* a transaction, so anyone may add it.
    pub keeper: UncheckedAccount,
    #[account(
        mut,
        seeds = [crate::state::GUARD_SEED, keeper.address().as_ref(), &[args.nonce]],
        bump = guard.bump,
        constraint = guard.keeper == *keeper.address() @ RelayError::GuardKeeperMismatch,
    )]
    pub guard: Account<GuardV0>,
}

#[derive(Clone, Copy, wincode::SchemaRead, wincode::SchemaWrite)]
pub struct AssertPaidArgsV0 {
    /// Lamports the keeper must have gained since the guard was armed.
    /// This is the turner's own price, not the target's advertised one —
    /// a turner asserts what it is willing to work for.
    pub min_payment: u64,
    pub nonce: u8,
}

/// Assert the keeper gained at least `min_payment` since
/// [`super::begin_guard_v0`] armed the guard, then disarm.
///
/// This replaces wrapping the executor in a CPI: the turner submits the
/// executor instruction directly and brackets it with guards, so relay
/// costs no CPI depth (which the executor's own call stack needs) and no
/// per-invoke overhead. Failing the assertion reverts the whole
/// transaction, including the executor's work.
pub fn handle_assert_paid_v0(
    ctx: &mut Context<AssertPaidV0>,
    args: AssertPaidArgsV0,
) -> Result<()> {
    let now = ctx.accounts.keeper.lamports();
    let guard = &mut *ctx.accounts.guard;
    require!(guard.armed != 0, RelayError::GuardNotArmed);
    let paid = now.saturating_sub(guard.snapshot);
    require!(
        paid >= args.min_payment,
        RelayError::InsufficientKeeperPayment
    );
    // Disarm so a trailing guard without a matching arm in the same
    // transaction fails loudly instead of measuring against stale state.
    guard.armed = 0;
    guard.snapshot = 0;
    Ok(())
}
