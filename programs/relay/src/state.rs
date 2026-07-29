//! Registry state. A watch is discovery metadata only — the condition block
//! it points at is the source of truth for everything else.

use anchor_lang_v2::prelude::*;
use static_assertions::const_assert_eq;

#[account]
pub struct WatchV0 {
    /// Owner program of `target`, recorded at registration from the account
    /// itself (not caller-supplied). Sits first so turners can memcmp-filter
    /// the registry server-side and never receive — let alone fetch — the
    /// watches of programs they don't crank.
    pub target_program: Address,
    /// Account carrying a condition block in its data.
    pub target: Address,
    /// Who registered (and may close) this watch.
    pub registrar: Address,
    /// Byte offset of the block within the target's account data.
    pub offset: u32,
    pub _pad: [u8; 4],
}

const_assert_eq!(core::mem::size_of::<WatchV0>(), 104);

/// Full account size: `[disc: 8][WatchV0]`. Must stay in lockstep with
/// `relay_spec::WATCH_V0_LEN` — the spec crate is how turners parse
/// these accounts without depending on this crate.
pub const WATCH_ACCOUNT_LEN: usize = 8 + core::mem::size_of::<WatchV0>();

const_assert_eq!(WATCH_ACCOUNT_LEN, relay_spec::WATCH_V0_LEN);
const_assert_eq!(
    8 + core::mem::offset_of!(WatchV0, target_program),
    relay_spec::WATCH_TARGET_PROGRAM_OFFSET
);

/// PDA seed prefix for [`GuardV0`].
pub const GUARD_SEED: &[u8] = relay_spec::GUARD_SEED;

/// Scratch account bracketing a crank: `begin_guard_v0` records the payout
/// account's balance here, `assert_paid_v0` measures the delta against it.
///
/// Nothing here survives a failed transaction (all state reverts), and a
/// successful one disarms it — so a guard is pure scratch that happens to
/// need an address. One per `(keeper, nonce)`; the nonce lets a turner run
/// concurrent cranks without serializing on a single write lock.
#[account]
pub struct GuardV0 {
    /// The account being paid — never a transaction signer.
    pub payout: Address,
    /// Payout lamports at arm time.
    pub snapshot: u64,
    /// 0 = disarmed. A trailing guard with no matching arm fails.
    pub armed: u8,
    pub bump: u8,
    pub nonce: u8,
    pub _pad: [u8; 5],
}

const_assert_eq!(core::mem::size_of::<GuardV0>(), 48);

pub const GUARD_ACCOUNT_LEN: usize = 8 + core::mem::size_of::<GuardV0>();
