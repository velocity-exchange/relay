use anchor_lang_v2::prelude::*;
use relay_spec::{ResolvedCrankV0, ResponsePointerV0, RESPONSE_POINTER_LEN};

use crate::error::DemoError;
use crate::state::{
    BookV0, CONDITIONS_OFFSET, CROSS_CONDITION, EVICT_CONDITION, MAX_ENTRIES, MAX_SWEEP_IDS,
    SWEEP_CONDITION,
};

/// The condition that fired, as the turner appends it to this instruction's
/// data after the discriminator — byte-for-byte
/// [`relay_spec::FiredConditionV0`] (32-byte target, u32 block offset, u8
/// slot index).
#[derive(Clone, Copy, wincode::SchemaRead, wincode::SchemaWrite)]
pub struct FiredConditionArgsV0 {
    pub target: [u8; 32],
    pub block_offset: u32,
    pub index: u8,
}

#[derive(Accounts)]
pub struct ResolveV0 {
    /// Writable only because the payload is staged here. The instruction is
    /// otherwise read-only, and turners only ever *simulate* it — the
    /// staged bytes are read out of the simulation's post-execution account
    /// state and never land on chain.
    #[account(mut)]
    pub book: Account<BookV0>,
}

/// One resolver for all three of the book's conditions.
///
/// This is what returning the executor identity buys: the block's three
/// slots differ only in their wake and their index, and the instruction to
/// run is this handler's answer rather than a literal in each condition.
/// Adding a fourth crank is a wake, an arm here, and nothing else.
///
/// The fired identity is an **argument, not a capability**: it is checked
/// against the account actually held before anything is resolved. Getting it
/// wrong could only produce an answer about a condition that is not due,
/// which costs the turner a simulation — but a resolver that acted on an
/// index it does not serve would stage nonsense, so it is rejected outright.
pub fn handle_resolve_v0(
    ctx: &mut Context<ResolveV0>,
    args: FiredConditionArgsV0,
) -> Result<[u8; RESPONSE_POINTER_LEN]> {
    let own_address = *ctx.accounts.book.view().address();
    require!(
        args.target == *own_address.as_array() && args.block_offset as usize == CONDITIONS_OFFSET,
        DemoError::UnknownCondition
    );
    let now = Clock::get()?.unix_timestamp;
    let book = &mut *ctx.accounts.book;

    let resolved = match args.index {
        SWEEP_CONDITION => resolve_sweep(book, &own_address, now),
        EVICT_CONDITION => resolve_evict(book, &own_address),
        CROSS_CONDITION => resolve_cross(book, &own_address),
        _ => return Err(DemoError::UnknownCondition.into()),
    };
    match resolved {
        Some(resolved) => book.stage_payload(&resolved),
        // No staging write at all on the cheap path.
        None => Ok(ResponsePointerV0::no_work().to_bytes()),
    }
}

/// Sweep: enumerate expired entries, one batch's worth. The wake re-fires
/// until the backlog drains.
fn resolve_sweep(book: &BookV0, own_address: &Address, now: i64) -> Option<ResolvedCrankV0> {
    let ids: Vec<u64> = (0..MAX_ENTRIES)
        .filter(|&i| book.entry_live[i] == 1 && book.entry_expiries[i] <= now)
        .map(|i| book.entry_ids[i])
        .take(MAX_SWEEP_IDS)
        .collect();
    if ids.is_empty() {
        return None;
    }
    // Executor args: the borsh wire of `SweepArgsV0 { ids: Vec<u64> }`.
    let data = (ids.len() as u32)
        .to_le_bytes()
        .into_iter()
        .chain(ids.iter().flat_map(|id| id.to_le_bytes()))
        .collect();
    Some(BookV0::resolved(
        own_address,
        crate::instruction::SweepV0::DISCRIMINATOR,
        data,
    ))
}

/// Evict: work exists once the book is at/above its soft cap; the victim is
/// the oldest live entry.
fn resolve_evict(book: &BookV0, own_address: &Address) -> Option<ResolvedCrankV0> {
    let (_, id) = (book.entry_count >= book.evict_threshold)
        .then(|| book.oldest_live())
        .flatten()?;
    Some(BookV0::resolved(
        own_address,
        crate::instruction::EvictV0::DISCRIMINATOR,
        // Executor args: the borsh wire of `EvictArgsV0 { id: u64 }`.
        id.to_le_bytes().to_vec(),
    ))
}

/// Cross: the wake is any change to the book at all, so this runs often and
/// answers "is the book crossed?" cheaply — affordable exactly because the
/// turner simulates locally.
fn resolve_cross(book: &BookV0, own_address: &Address) -> Option<ResolvedCrankV0> {
    let (bid_id, ask_id) = book.crossed()?;
    // Executor args: the borsh wire of `CrossArgsV0 { bid_id, ask_id }`.
    let data = bid_id
        .to_le_bytes()
        .into_iter()
        .chain(ask_id.to_le_bytes())
        .collect();
    Some(BookV0::resolved(
        own_address,
        crate::instruction::CrossV0::DISCRIMINATOR,
        data,
    ))
}
