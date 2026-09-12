//! Wire format for relay condition blocks.
//!
//! A target program embeds a condition block — [`ConditionBlockHeaderV0`]
//! followed by a fixed array of [`ConditionV0`] — at a fixed, **8-aligned**
//! offset in one of its accounts, and registers `(account, offset)` with the
//! relay program as a `WatchV0`. A crank turner finds watches, reads the
//! conditions, and for each due condition simulates the **resolver**
//! instruction, telling it which condition fired ([`FiredConditionV0`],
//! appended to the resolver's instruction data); the resolver stages a
//! [`ResolvedCrankV0`] payload in one of its own writable accounts and
//! returns a [`ResponsePointerV0`] locating it. The payload names the
//! **executor** — program, discriminator, accounts, args — so the identity
//! of the instruction to run is the resolver's answer rather than a literal
//! in the condition. The turner reads the staged bytes out of the
//! simulation's post-execution account state, then submits that executor
//! directly, bracketed by relay's payment guards (`begin_guard_v0` …
//! executor … `assert_paid_v0`) so an underpaying crank reverts.
//!
//! **Why staging instead of raw return data:** return data is capped at
//! 1024 bytes, which bounds how many accounts/args a resolver could name —
//! exactly the wrong thing to bound, since batch cranks (sweep every
//! expired order, each with its own owner) grow with the work. The pointer
//! is 10 bytes; the payload can be as large as the staging region. The
//! resolver is only ever *simulated*, so the staging write never lands on
//! chain: no state bloat, no rent, and no write contention between
//! competing turners.
//!
//! Everything on the evaluation path is **zero-copy pod**: fixed-size
//! `#[repr(C)]` structs with **alignment 1** (every multi-byte scalar is
//! stored as a little-endian byte array, read back through an accessor) and
//! no interior padding. Alignment 1 is what makes the read path uniform:
//! any reader — a program looking at its own account data, a turner looking
//! at bytes off an RPC response — casts the region in place with
//! [`read_block`], at any offset, with no copy and no separate unaligned
//! path to keep in step.
//!
//! Design notes live in the repo's DESIGN.md. Two properties matter here:
//!
//! - **Wake hints are hints.** The instruction is the authoritative
//!   predicate; a hint firing early only costs a simulation. Programs must
//!   never let a hint fire *late* (work exists but no wake) — when in
//!   doubt, pair with a [`WakeKind::EverySlots`] fallback condition.
//! - **The block is program-owned state.** The same handlers that change
//!   the underlying state update the conditions describing it; nothing
//!   external needs to stay in sync.
//!
//! This crate is `no_std` and depends only on `bytemuck`.

#![cfg_attr(not(test), no_std)]

extern crate alloc;

#[cfg(feature = "idl-build-v2")]
extern crate anchor_lang_v2 as anchor_lang;

pub use bytemuck;

pub mod account_ref;
pub mod block;
pub mod condition;
pub mod fired;
pub mod guard;
pub mod host;
pub mod resolved;
pub mod resolver;
pub mod watch;

#[cfg(test)]
mod test_support;

// The crate's surface is flat: every reader names `relay_spec::ConditionV0`
// rather than a module path, and the modules exist to keep one subject per
// file. Re-export everything so a split moves no caller's imports.
pub use account_ref::{AccountRefV0, ACCOUNT_REF_LEN};
pub use block::{
    block_space, read_block, read_block_mut, write_block, ConditionBlockHeaderV0, BLOCK_HEADER_LEN,
};
pub use condition::{
    read_watched_value, value_crossed, ConditionV0, CrankSpecV0, WakeKind, WakeView, WatchValue,
    WatchedRegion, CONDITION_LEN,
};
pub use fired::{encode_resolver_data, FiredConditionV0, FIRED_CONDITION_LEN, RESOLVER_DATA_LEN};
pub use guard::{
    encode_assert_paid_v0_data, encode_begin_guard_v0_data, ASSERT_PAID_V0_DISCRIMINATOR,
    BEGIN_GUARD_V0_DISCRIMINATOR, GUARD_SEED,
};
pub use host::{ConditionBlock, RelayBlockV0};
pub use resolved::{
    stage_into, ResolvedCrankV0, ResponsePointerV0, RESOLVED_HEADER_LEN, RESPONSE_POINTER_LEN,
};
pub use resolver::{
    read_resolver_stripe, resolver_stripe_stride, resolver_stripes_len, write_resolver_stripe,
    ResolverListV0, MAX_INDIRECT_RESOLVER_ACCOUNTS,
};
pub use watch::{
    WatchV0, WATCH_CREATOR_OFFSET, WATCH_TARGET_OFFSET, WATCH_TARGET_PROGRAM_OFFSET,
    WATCH_V0_DISCRIMINATOR, WATCH_V0_LEN,
};

/// Magic bytes opening every condition block.
pub const MAGIC: [u8; 8] = *b"RELAY-V0";

/// Current wire version.
pub const SPEC_VERSION: u8 = 0;

/// Sentinel address in a resolver's staged account list that the turner
/// replaces with its keeper (payment recipient) before submitting.
pub const KEEPER_PLACEHOLDER: [u8; 32] = *b"relay/keeper/placeholder\0\0\0\0\0\0\0\0";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SpecError {
    /// Buffer ended before the structure did.
    Truncated,
    /// Magic bytes did not match [`MAGIC`].
    BadMagic,
    /// Version byte above [`SPEC_VERSION`].
    UnsupportedVersion,
    /// Wake kind byte out of range.
    BadWakeKind,
    /// A zero-copy cast failed. Unreachable for a v0 block — every type in
    /// one is alignment 1, so no offset can be wrong for it — and kept only
    /// so the error surface does not shift under callers matching on it,
    /// and so a future version with stricter types has somewhere to land.
    Misaligned,
    /// A count field exceeds its fixed capacity, or a payload exceeds the
    /// region it must fit in.
    TooLarge,
    /// Account discriminator mismatch.
    BadDiscriminator,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn layout_is_pinned() {
        assert_eq!(CONDITION_LEN, 192);
        assert_eq!(BLOCK_HEADER_LEN, 16);
        assert_eq!(ACCOUNT_REF_LEN, 33);
        assert_eq!(RESPONSE_POINTER_LEN, 10);
        assert_eq!(RESOLVED_HEADER_LEN, 43);
        assert_eq!(FIRED_CONDITION_LEN, 37);
        assert_eq!(block_space(2), 400);
    }

    /// Alignment 1 everywhere on the evaluation path is what makes one
    /// reader enough: [`read_block`] casts a block out of *any* buffer at
    /// *any* offset, so nothing needs a copying sibling.
    #[test]
    fn the_wire_types_are_alignment_one() {
        assert_eq!(core::mem::align_of::<ConditionBlockHeaderV0>(), 1);
        assert_eq!(core::mem::align_of::<ConditionV0>(), 1);
        assert_eq!(core::mem::align_of::<AccountRefV0>(), 1);
        assert_eq!(core::mem::align_of::<ResponsePointerV0>(), 1);
        assert_eq!(core::mem::align_of::<FiredConditionV0>(), 1);
    }
}
