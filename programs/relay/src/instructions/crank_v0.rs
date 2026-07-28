use anchor_lang_v2::pinocchio::cpi::invoke_with_slice;
use anchor_lang_v2::pinocchio::instruction::{InstructionAccount, InstructionView};
use anchor_lang_v2::{address_eq, prelude::*};
use relay_spec::ConditionV0;

use crate::error::RelayError;

#[derive(Accounts)]
pub struct CrankV0 {
    /// Account carrying the condition block. Deliberately NOT constrained to
    /// any owner: whoever controls an account's data controls the conditions
    /// in it, and since no signer privilege is ever forwarded, a hostile
    /// block can't make an executor do anything a direct transaction
    /// couldn't.
    pub target: UncheckedAccount,
    /// Must match the condition's `executor_program`.
    pub executor_program: UncheckedAccount,
    // Remaining accounts: the executor's account list, in resolver order.
    // The keeper (payment recipient) is one of them, named by
    // `args.keeper_index`. It is intentionally not a declared field: the
    // executor mutates it, and a declared-mut field duplicated in the
    // remaining region is rejected by the account walker.
}

#[derive(Clone, wincode::SchemaRead, wincode::SchemaWrite)]
pub struct CrankArgsV0 {
    /// Byte offset of the condition block in `target`'s data (8-aligned).
    pub offset: u32,
    /// Which condition in the block is being cranked.
    pub condition_index: u8,
    /// Index into the remaining accounts of the keeper whose payment is
    /// asserted.
    pub keeper_index: u8,
    /// Executor args after the 8-byte discriminator (the resolver's
    /// `ResolvedCrankV0.data`).
    pub data: Vec<u8>,
}

pub fn handle_crank_v0(ctx: &mut Context<CrankV0>, args: CrankArgsV0) -> Result<()> {
    // Zero-copy read of the condition, copied out (280 bytes, stack) inside
    // a scope so the data borrow is released before the CPI (the executor
    // typically writes to `target`).
    let condition: ConditionV0 = {
        let data = ctx.accounts.target.try_borrow()?;
        let (_, conditions) = relay_spec::read_block(&data, args.offset as usize)
            .map_err(|_| RelayError::InvalidConditionBlock)?;
        *conditions
            .get(args.condition_index as usize)
            .ok_or(RelayError::ConditionIndexOutOfBounds)?
    };
    require!(condition.is_active(), RelayError::ConditionInactive);

    let executor_program = Address::new_from_array(condition.executor_program);
    require!(
        address_eq(ctx.accounts.executor_program.address(), &executor_program),
        RelayError::ExecutorProgramMismatch
    );
    require!(
        !address_eq(&executor_program, ctx.program_id),
        RelayError::SelfReentry
    );

    let remaining = ctx.remaining_accounts()?;
    let keeper = remaining
        .get(args.keeper_index as usize)
        .ok_or(RelayError::KeeperIndexOutOfBounds)?;
    let keeper_before = keeper.lamports();

    // Forward every account with `is_signer: false` — signer status
    // propagates through CPI, and this instruction must grant nothing.
    let metas: Vec<InstructionAccount> = remaining
        .iter()
        .map(|view| InstructionAccount::new(view.address(), view.is_writable(), false))
        .collect();
    let mut data = Vec::with_capacity(8 + args.data.len());
    data.extend_from_slice(&condition.executor_disc);
    data.extend_from_slice(&args.data);
    let instruction = InstructionView {
        program_id: &executor_program,
        accounts: &metas,
        data: &data,
    };
    invoke_with_slice(&instruction, &remaining)?;

    let paid = remaining[args.keeper_index as usize]
        .lamports()
        .saturating_sub(keeper_before);
    require!(
        paid >= condition.min_payment,
        RelayError::InsufficientKeeperPayment
    );
    Ok(())
}
