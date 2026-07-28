use anchor_lang_v2::prelude::*;

use crate::state::GuardV0;

#[derive(Accounts)]
#[instruction(args: BeginGuardArgsV0)]
pub struct BeginGuardV0 {
    /// The keeper whose payment is being guarded, and the fee payer. Signs
    /// so nobody else can arm (or fund) a guard on its behalf.
    #[account(mut)]
    pub keeper: Signer,
    /// Scratch account holding the pre-execution balance. Created on first
    /// use; reused forever after.
    #[account(
        init_if_needed,
        payer = keeper,
        seeds = [crate::state::GUARD_SEED, keeper.address().as_ref(), &[args.nonce]],
        bump,
    )]
    pub guard: Account<GuardV0>,
    pub system_program: Program<System>,
}

#[derive(Clone, Copy, wincode::SchemaRead, wincode::SchemaWrite)]
pub struct BeginGuardArgsV0 {
    /// Which of the keeper's guards to arm. A turner running cranks
    /// concurrently uses a different nonce per in-flight transaction so
    /// they don't serialize on one write lock.
    pub nonce: u8,
}

/// Snapshot the keeper's balance so [`super::assert_paid_v0`] can measure
/// what the executor paid.
///
/// Reading inside execution (rather than trusting a caller-supplied
/// "before" value) makes the pair fee-agnostic: the transaction fee is
/// already deducted by the time this runs, so the delta the trailing guard
/// computes is exactly what the executor moved.
pub fn handle_begin_guard_v0(
    ctx: &mut Context<BeginGuardV0>,
    args: BeginGuardArgsV0,
) -> Result<()> {
    let keeper = *ctx.accounts.keeper.address();
    let lamports = ctx.accounts.keeper.lamports();
    let bump = ctx.bumps.guard;
    let guard = &mut *ctx.accounts.guard;
    guard.keeper = keeper;
    guard.snapshot = lamports;
    guard.armed = 1;
    guard.bump = bump;
    guard.nonce = args.nonce;
    Ok(())
}
