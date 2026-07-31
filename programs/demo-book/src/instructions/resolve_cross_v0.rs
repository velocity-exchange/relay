use anchor_lang_v2::prelude::*;
use relay_spec::{ResolvedCrankV0, ResponsePointerV0, RESPONSE_POINTER_LEN};

use crate::state::BookV0;

#[derive(Accounts)]
pub struct ResolveCrossV0 {
    /// Writable for staging only — see `resolve_sweep_v0`.
    #[account(mut)]
    pub book: Account<BookV0>,
}

/// Resolver for the cross condition. Its wake is any change to the book at
/// all, so this runs often and answers "is the book crossed?" cheaply —
/// which is affordable exactly because the turner simulates locally.
pub fn handle_resolve_cross_v0(
    ctx: &mut Context<ResolveCrossV0>,
) -> Result<[u8; RESPONSE_POINTER_LEN]> {
    let own_address = *ctx.accounts.book.view().address();
    let book = &mut *ctx.accounts.book;

    match book.crossed() {
        Some((bid_id, ask_id)) => {
            // Executor args: the borsh wire of `CrossArgsV0 { bid_id, ask_id }`.
            let data = bid_id
                .to_le_bytes()
                .into_iter()
                .chain(ask_id.to_le_bytes())
                .collect();
            book.stage_payload(&ResolvedCrankV0 {
                accounts: BookV0::executor_accounts(&own_address),
                data,
            })
        }
        None => Ok(ResponsePointerV0::no_work().to_bytes()),
    }
}
