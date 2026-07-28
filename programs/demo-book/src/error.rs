use anchor_lang_v2::prelude::*;

#[error_code]
pub enum DemoError {
    #[msg("Book is at entry capacity")]
    BookFull,
    #[msg("No live entry with that id")]
    EntryNotFound,
    #[msg("Entry is not expired")]
    EntryNotExpired,
    #[msg("Sweep called with no entry ids")]
    NothingToSweep,
    #[msg("Entry count is below the evict threshold")]
    BelowEvictThreshold,
    #[msg("Book cannot pay the keeper and stay rent-exempt")]
    InsufficientTreasury,
    #[msg("Condition block does not fit the region")]
    ConditionRegionOverflow,
    #[msg("Signer does not match the book authority")]
    InvalidAuthority,
}
