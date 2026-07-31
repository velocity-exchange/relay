//! Book state. Fixed-capacity parallel arrays, the condition block embedded
//! as **typed pod fields** (a wake update is a single field store —
//! `self.conditions[SWEEP].wake_ts = new_min` — never a serialization
//! pass), and a scratch region resolvers stage their payloads in.
//!
//! Entries are two-sided quotes, so the book can cross. Three conditions
//! ride on it, one per wake kind that matters in practice:
//!
//! - **sweep** (`AtTimestamp`) — reclaim expired entries.
//! - **evict** (`OnAccountChange` on `entry_count`) — trim at a soft cap.
//! - **cross** (`OnAccountChange` on `version`) — whenever the book
//!   changes *at all*, look for a crossed bid/ask and match it. `version`
//!   is bumped by every mutation precisely so one 8-byte watch means "the
//!   book moved"; watching the whole account would work too, but this
//!   costs the turner an 8-byte diff instead of a 2KB one.

use anchor_lang_v2::prelude::*;
use relay_spec::{
    AccountRefV0, ConditionBlockHeaderV0, ConditionV0, CrankSpecV0, ResolvedCrankV0,
    ResponsePointerV0, KEEPER_PLACEHOLDER, RESPONSE_POINTER_LEN,
};
use static_assertions::const_assert_eq;

use crate::error::DemoError;

pub const MAX_ENTRIES: usize = 32;
pub const NUM_CONDITIONS: usize = 3;

/// Scratch bytes resolvers stage payloads in. Only ever written during
/// simulation, so this costs rent but never chain writes.
pub const STAGING_BYTES: usize = 1024;

/// Sweep batch bound per crank. Bounded by the transaction account budget,
/// not the staging region — the wake re-fires until the backlog drains.
pub const MAX_SWEEP_IDS: usize = 8;

pub const SIDE_BID: u8 = 0;
pub const SIDE_ASK: u8 = 1;

#[account]
pub struct BookV0 {
    /// Admin able to add/cancel entries and reconfigure.
    pub authority: Address,
    /// Lamports paid to the keeper per successful crank, from this
    /// account's balance (the book is its own treasury — fund it by
    /// transferring lamports in).
    pub payment_per_crank: u64,
    /// Starts at 1 so id 0 never matches a live entry.
    pub next_entry_id: u64,
    /// Bumped by every mutation. The cross condition watches these eight
    /// bytes, which is how "the book changed at all" becomes a wake.
    pub version: u64,
    /// Sweep wake hint: min over inserted expiries, NOT repaired on cancel
    /// (only the sweep executor recomputes the true minimum). Only ever
    /// early — an early hint costs a no-op simulation; a late one would be
    /// a liveness bug. Mirrored into `conditions[SWEEP_CONDITION].wake_ts`.
    pub next_expiry_ts: i64,
    pub entry_count: u32,
    /// Once `entry_count >= evict_threshold`, `evict_v0` may remove the
    /// oldest entry for a reward (soft-cap pattern).
    pub evict_threshold: u32,
    pub entry_ids: [u64; MAX_ENTRIES],
    pub entry_expiries: [i64; MAX_ENTRIES],
    pub entry_prices: [u64; MAX_ENTRIES],
    /// 1 = live, 0 = free.
    pub entry_live: [u8; MAX_ENTRIES],
    /// [`SIDE_BID`] or [`SIDE_ASK`].
    pub entry_sides: [u8; MAX_ENTRIES],
    /// The condition block a `WatchV0` points at, embedded as typed pod
    /// state. MUST stay contiguous and 8-aligned (`cond_header` then
    /// `conditions`, nothing between).
    pub cond_header: ConditionBlockHeaderV0,
    pub conditions: [ConditionV0; NUM_CONDITIONS],
    /// Resolver staging region (see [`STAGING_BYTES`]).
    pub staging: [u8; STAGING_BYTES],
}

const_assert_eq!(core::mem::size_of::<BookV0>(), 6360);

/// Account-data offset of the condition block (what to register the watch
/// at). 8-aligned, as the zero-copy read path requires.
pub const CONDITIONS_OFFSET: usize = 8 + core::mem::offset_of!(BookV0, cond_header);
const_assert_eq!(CONDITIONS_OFFSET % 8, 0);
const_assert_eq!(
    core::mem::offset_of!(BookV0, conditions),
    core::mem::offset_of!(BookV0, cond_header) + relay_spec::BLOCK_HEADER_LEN
);

/// Account-data offset of the staging region — the `offset` a resolver's
/// [`ResponsePointerV0`] carries.
pub const STAGING_OFFSET: usize = 8 + core::mem::offset_of!(BookV0, staging);

/// Account-data offset of `entry_count` — the evict condition's
/// change-watch range.
pub const ENTRY_COUNT_OFFSET: usize = 8 + core::mem::offset_of!(BookV0, entry_count);

/// Account-data offset of `version` — the cross condition's change-watch
/// range.
pub const VERSION_OFFSET: usize = 8 + core::mem::offset_of!(BookV0, version);

pub const BOOK_ACCOUNT_LEN: usize = 8 + core::mem::size_of::<BookV0>();

/// Sweep condition index in the block.
pub const SWEEP_CONDITION: u8 = 0;
/// Evict condition index in the block.
pub const EVICT_CONDITION: u8 = 1;
/// Cross condition index in the block.
pub const CROSS_CONDITION: u8 = 2;

/// The book is the only account any resolver takes, so a staged payload
/// always points at resolver account 0.
pub const STAGING_ACCOUNT_INDEX: u8 = 0;

fn disc(d: &'static [u8]) -> [u8; 8] {
    d.try_into().expect("anchor discriminators are 8 bytes")
}

impl BookV0 {
    pub fn find_live(&self, id: u64) -> Option<usize> {
        (0..MAX_ENTRIES).find(|&i| self.entry_live[i] == 1 && self.entry_ids[i] == id)
    }

    pub fn insert(&mut self, expiry_ts: i64, price: u64, side: u8) -> Result<u64> {
        let slot = (0..MAX_ENTRIES)
            .find(|&i| self.entry_live[i] == 0)
            .ok_or(DemoError::BookFull)?;
        let id = self.next_entry_id;
        self.next_entry_id += 1;
        self.entry_ids[slot] = id;
        self.entry_expiries[slot] = expiry_ts;
        self.entry_prices[slot] = price;
        self.entry_sides[slot] = side;
        self.entry_live[slot] = 1;
        self.entry_count += 1;
        self.touch();
        if expiry_ts < self.next_expiry_ts {
            self.next_expiry_ts = expiry_ts;
            // The single-store wake update the pod layout exists for.
            self.conditions[SWEEP_CONDITION as usize].wake_ts = expiry_ts;
        }
        Ok(id)
    }

    pub fn remove(&mut self, slot: usize) {
        self.entry_live[slot] = 0;
        self.entry_count -= 1;
        self.touch();
    }

    /// Record that the book moved, firing the cross condition's wake.
    pub fn touch(&mut self) {
        self.version = self.version.wrapping_add(1);
    }

    /// True minimum expiry over live entries (`i64::MAX` when empty).
    pub fn true_next_expiry(&self) -> i64 {
        (0..MAX_ENTRIES)
            .filter(|&i| self.entry_live[i] == 1)
            .map(|i| self.entry_expiries[i])
            .min()
            .unwrap_or(i64::MAX)
    }

    /// Repair the sweep hint to the true minimum (executor-side).
    pub fn repair_next_expiry(&mut self) {
        self.next_expiry_ts = self.true_next_expiry();
        self.conditions[SWEEP_CONDITION as usize].wake_ts = self.next_expiry_ts;
    }

    /// Live entry with the smallest id (the eviction victim).
    pub fn oldest_live(&self) -> Option<(usize, u64)> {
        (0..MAX_ENTRIES)
            .filter(|&i| self.entry_live[i] == 1)
            .map(|i| (i, self.entry_ids[i]))
            .min_by_key(|&(_, id)| id)
    }

    fn live_side(&self, side: u8) -> impl Iterator<Item = usize> + '_ {
        (0..MAX_ENTRIES).filter(move |&i| self.entry_live[i] == 1 && self.entry_sides[i] == side)
    }

    /// Highest bid, lowest ask — ties broken by id so the choice is
    /// deterministic and a turner's simulation matches what lands.
    pub fn best_bid(&self) -> Option<usize> {
        self.live_side(SIDE_BID)
            .max_by_key(|&i| (self.entry_prices[i], core::cmp::Reverse(self.entry_ids[i])))
    }

    pub fn best_ask(&self) -> Option<usize> {
        self.live_side(SIDE_ASK)
            .min_by_key(|&i| (self.entry_prices[i], self.entry_ids[i]))
    }

    /// The crossed pair, if the book is crossed.
    pub fn crossed(&self) -> Option<(u64, u64)> {
        let bid = self.best_bid()?;
        let ask = self.best_ask()?;
        (self.entry_prices[bid] >= self.entry_prices[ask])
            .then_some((self.entry_ids[bid], self.entry_ids[ask]))
    }

    /// Stage a resolver payload and return the pointer to it. The turner
    /// reads `staging[..len]` out of the simulation's post-execution
    /// account state.
    pub fn stage(&mut self, resolved: &ResolvedCrankV0) -> Result<[u8; RESPONSE_POINTER_LEN]> {
        let len = resolved
            .write_into(&mut self.staging)
            .map_err(|_| DemoError::StagingOverflow)?;
        Ok(
            ResponsePointerV0::new(STAGING_ACCOUNT_INDEX, STAGING_OFFSET as u32, len as u32)
                .to_bytes(),
        )
    }

    /// One-time condition block initialization (wake inputs are updated in
    /// place afterwards, never by rewriting the block).
    pub fn init_conditions(&mut self, own_address: &Address) {
        let program: [u8; 32] = *crate::ID.as_array();
        let book: [u8; 32] = *own_address.as_array();
        // Writable: resolvers stage their payload into the book itself.
        let resolver_accounts = [AccountRefV0::writable(book)];
        let spec = |resolver: &'static [u8], executor: &'static [u8]| CrankSpecV0 {
            resolver_program: program,
            resolver_disc: disc(resolver),
            executor_program: program,
            executor_disc: disc(executor),
            min_payment: self.payment_per_crank,
        };
        self.cond_header = ConditionBlockHeaderV0::new(NUM_CONDITIONS as u8);
        // Element-by-element: a `[ConditionV0; 3]` literal materializes the
        // whole 3 x 1.5KB array in the caller's stack frame, past the 4KB
        // limit. One temporary at a time stays comfortably inside it.
        self.conditions[SWEEP_CONDITION as usize] = ConditionV0::at_timestamp(
            self.next_expiry_ts,
            spec(
                crate::instruction::ResolveSweepV0::DISCRIMINATOR,
                crate::instruction::SweepV0::DISCRIMINATOR,
            ),
            &resolver_accounts,
        );
        self.conditions[EVICT_CONDITION as usize] = ConditionV0::on_account_change(
            book,
            ENTRY_COUNT_OFFSET as u32,
            4,
            spec(
                crate::instruction::ResolveEvictV0::DISCRIMINATOR,
                crate::instruction::EvictV0::DISCRIMINATOR,
            ),
            &resolver_accounts,
        );
        // Cross: any change to the book at all.
        self.conditions[CROSS_CONDITION as usize] = ConditionV0::on_account_change(
            book,
            VERSION_OFFSET as u32,
            8,
            spec(
                crate::instruction::ResolveCrossV0::DISCRIMINATOR,
                crate::instruction::CrossV0::DISCRIMINATOR,
            ),
            &resolver_accounts,
        );
    }

    /// Executor account list shared by every resolver: keeper placeholder
    /// (writable, receives payment) then the book (writable).
    pub fn executor_accounts(own_address: &Address) -> Vec<AccountRefV0> {
        vec![
            AccountRefV0::writable(KEEPER_PLACEHOLDER),
            AccountRefV0::writable(*own_address.as_array()),
        ]
    }
}

/// Pay the keeper from the book's lamports, keeping the book rent-exempt.
pub fn pay_keeper(
    book_view: &anchor_lang_v2::pinocchio::account::AccountView,
    keeper_view: &anchor_lang_v2::pinocchio::account::AccountView,
    amount: u64,
) -> Result<()> {
    use anchor_lang_v2::pinocchio::sysvars::{rent::Rent, Sysvar};
    let min = Rent::get()?.try_minimum_balance(book_view.data_len())?;
    let book_lamports = book_view.lamports();
    require!(
        book_lamports >= min.saturating_add(amount),
        DemoError::InsufficientTreasury
    );
    // AccountView is a Copy handle over runtime memory; lamport writes go
    // through regardless of which copy performs them (same pattern as
    // anchor's own `close`).
    let mut book = *book_view;
    book.set_lamports(book_lamports - amount);
    let mut keeper = *keeper_view;
    keeper.set_lamports(keeper_view.lamports() + amount);
    Ok(())
}
