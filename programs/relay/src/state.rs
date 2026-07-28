//! Registry state. A watch is discovery metadata only — the condition block
//! it points at is the source of truth for everything else.

use anchor_lang_v2::prelude::*;
use static_assertions::const_assert_eq;

#[account]
pub struct WatchV0 {
    /// Who registered (and may close) this watch.
    pub registrar: Address,
    /// Account carrying a `ConditionBlockV0` in its data.
    pub target: Address,
    /// Byte offset of the block within the target's account data.
    pub offset: u32,
    pub _pad: [u8; 4],
}

const_assert_eq!(core::mem::size_of::<WatchV0>(), 72);

/// Full account size: `[disc: 8][WatchV0]`. Must stay in lockstep with
/// `relay_spec::WATCH_V0_LEN` — the spec crate is how turners parse
/// these accounts without depending on this crate.
pub const WATCH_ACCOUNT_LEN: usize = 8 + core::mem::size_of::<WatchV0>();

const_assert_eq!(WATCH_ACCOUNT_LEN, relay_spec::WATCH_V0_LEN);
