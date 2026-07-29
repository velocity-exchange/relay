use anchor_lang_v2::prelude::*;

use crate::state::GuardV0;

#[derive(Accounts)]
#[instruction(args: BeginGuardArgsV0)]
pub struct BeginGuardV0 {
    /// Funds the guard account on first use. This is the turner's fee
    /// payer, and it signs — but it is deliberately NOT the account whose
    /// payment is measured.
    #[account(mut)]
    pub payer: Signer,
    /// The account the executor must pay. Not a signer, and it must not be
    /// one: signer status is transaction-global, so anything that signs
    /// this transaction is exposed to every instruction in it, including
    /// an untrusted executor's. Keeping the payout separate from the payer
    /// is what makes a hostile executor unable to touch either.
    pub payout: UncheckedAccount,
    /// Scratch account holding the pre-execution balance. Created on first
    /// use; reused forever after. Seeded by the payout so each recipient
    /// has its own.
    #[account(
        init_if_needed,
        payer = payer,
        seeds = [crate::state::GUARD_SEED, payout.address().as_ref(), &[args.nonce]],
        bump,
    )]
    pub guard: Account<GuardV0>,
    pub system_program: Program<System>,
}

#[derive(Clone, Copy, wincode::SchemaRead, wincode::SchemaWrite)]
pub struct BeginGuardArgsV0 {
    /// Which of the payout's guards to arm. A turner running cranks
    /// concurrently uses a different nonce per in-flight transaction so
    /// they don't serialize on one write lock.
    pub nonce: u8,
}

/// Snapshot the payout account's balance so [`super::assert_paid_v0`] can
/// measure what the executor paid.
///
/// Reading inside execution (rather than trusting a caller-supplied
/// "before" value) makes the pair fee-agnostic: the transaction fee is
/// already deducted by the time this runs, and it comes out of the payer,
/// not the payout — so the delta the trailing guard computes is exactly
/// what the executor moved.
pub fn handle_begin_guard_v0(
    ctx: &mut Context<BeginGuardV0>,
    args: BeginGuardArgsV0,
) -> Result<()> {
    let payout = *ctx.accounts.payout.address();
    let lamports = ctx.accounts.payout.lamports();
    let bump = ctx.bumps.guard;
    let guard = &mut *ctx.accounts.guard;
    guard.payout = payout;
    guard.snapshot = lamports;
    guard.armed = 1;
    guard.bump = bump;
    guard.nonce = args.nonce;
    Ok(())
}
