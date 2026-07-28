use anchor_lang_v2::prelude::*;

#[error_code]
pub enum RelayError {
    #[msg("Target account data does not hold a valid condition block at the given offset")]
    InvalidConditionBlock,
    #[msg("Condition index is out of bounds for the block")]
    ConditionIndexOutOfBounds,
    #[msg("Condition is marked inactive")]
    ConditionInactive,
    #[msg("Executor program account does not match the condition's executor program")]
    ExecutorProgramMismatch,
    #[msg("Executor program may not be relay itself")]
    SelfReentry,
    #[msg("Keeper index is out of bounds for the executor account list")]
    KeeperIndexOutOfBounds,
    #[msg("Executor paid the keeper less than the condition's min_payment")]
    InsufficientKeeperPayment,
    #[msg("Signer does not match the watch registrar")]
    InvalidRegistrar,
}
