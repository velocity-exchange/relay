use anchor_lang_v2::prelude::*;

#[error_code]
pub enum RelayError {
    #[msg("Executor paid the keeper less than the asserted min_payment")]
    InsufficientKeeperPayment,
    #[msg("Guard was not armed by begin_guard_v0 in this transaction")]
    GuardNotArmed,
    #[msg("Guard does not belong to the given keeper")]
    GuardKeeperMismatch,
    #[msg("Signer does not match the watch registrar")]
    InvalidRegistrar,
}
