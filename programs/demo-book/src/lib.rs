//! Reference relay target program: a book of expiring entries.
//!
//! Demonstrates the full condition-embedding pattern the spec crate
//! describes, one instance of each wake kind:
//!
//! - **Sweep** ([`WakeV0::AtTimestamp`]): entries expire at `expiry_ts`. The
//!   book keeps a `next_expiry_ts` hint maintained min-over-inserts — cheap,
//!   conservative (only ever early, never late), and deliberately NOT
//!   repaired on cancel. The sweep executor recomputes the true minimum as
//!   it walks, repairing the hint. `resolve_sweep_v0` enumerates expired
//!   entries and returns the executor's account list + args;
//!   `sweep_v0` frees them and pays the keeper from the book's own
//!   lamports.
//! - **Evict** ([`WakeV0::OnAccountChange`] on `entry_count`): once the book
//!   holds at least `evict_threshold` entries, the oldest entry can be
//!   evicted for a reward — the CLOB soft-cap pattern.
//! - **Cross** ([`WakeV0::OnAccountChange`] on `version`, which every
//!   mutation bumps): whenever the book changes at all, check whether the
//!   best bid crosses the best ask and match them if so.
//!
//! Executors require no signer: cranks are permissionless, priced by
//! `payment_per_crank`, and the instruction itself is the authoritative
//! predicate (both fail without work, so a turner's simulation filters
//! no-ops).
//!
//! [`WakeV0::AtTimestamp`]: relay_spec::WakeV0::AtTimestamp
//! [`WakeV0::OnAccountChange`]: relay_spec::WakeV0::OnAccountChange

use anchor_lang_v2::prelude::*;

pub mod error;
pub mod instructions;
pub mod state;

pub use anchor_lang_v2;
pub use instructions::*;

declare_id!("6PqZZeykcFwncPxs5LjjxzQshdRV29mpsFtmT3QS9jRZ");

#[program]
pub mod demo_book {
    use super::*;

    pub fn initialize_book_v0(
        ctx: &mut Context<InitializeBookV0>,
        args: InitializeBookArgsV0,
    ) -> Result<()> {
        instructions::initialize_book_v0::handle_initialize_book_v0(ctx, args)
    }

    pub fn add_entry_v0(ctx: &mut Context<AddEntryV0>, args: AddEntryArgsV0) -> Result<()> {
        instructions::add_entry_v0::handle_add_entry_v0(ctx, args)
    }

    pub fn cancel_entry_v0(
        ctx: &mut Context<CancelEntryV0>,
        args: CancelEntryArgsV0,
    ) -> Result<()> {
        instructions::cancel_entry_v0::handle_cancel_entry_v0(ctx, args)
    }

    pub fn set_payment_v0(ctx: &mut Context<SetPaymentV0>, args: SetPaymentArgsV0) -> Result<()> {
        instructions::set_payment_v0::handle_set_payment_v0(ctx, args)
    }

    pub fn resolve_sweep_v0(
        ctx: &mut Context<ResolveSweepV0>,
    ) -> Result<[u8; relay_spec::RESPONSE_POINTER_LEN]> {
        instructions::resolve_sweep_v0::handle_resolve_sweep_v0(ctx)
    }

    pub fn sweep_v0(ctx: &mut Context<SweepV0>, args: SweepArgsV0) -> Result<()> {
        instructions::sweep_v0::handle_sweep_v0(ctx, args)
    }

    pub fn resolve_evict_v0(
        ctx: &mut Context<ResolveEvictV0>,
    ) -> Result<[u8; relay_spec::RESPONSE_POINTER_LEN]> {
        instructions::resolve_evict_v0::handle_resolve_evict_v0(ctx)
    }

    pub fn evict_v0(ctx: &mut Context<EvictV0>, args: EvictArgsV0) -> Result<()> {
        instructions::evict_v0::handle_evict_v0(ctx, args)
    }

    pub fn resolve_cross_v0(
        ctx: &mut Context<ResolveCrossV0>,
    ) -> Result<[u8; relay_spec::RESPONSE_POINTER_LEN]> {
        instructions::resolve_cross_v0::handle_resolve_cross_v0(ctx)
    }

    pub fn cross_v0(ctx: &mut Context<CrossV0>, args: CrossArgsV0) -> Result<()> {
        instructions::cross_v0::handle_cross_v0(ctx, args)
    }
}
