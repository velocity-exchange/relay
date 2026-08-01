//! Wire format for relay condition blocks.
//!
//! A target program embeds a condition block — [`ConditionBlockHeaderV0`]
//! followed by a fixed array of [`ConditionV0`] — at a fixed, **8-aligned**
//! offset in one of its accounts, and registers `(account, offset)` with the
//! relay program as a `WatchV0`. A crank turner finds watches, reads the
//! conditions, and for each due condition simulates the **resolver**
//! instruction; the resolver stages a [`ResolvedCrankV0`] payload in one of
//! its own writable accounts and returns a [`ResponsePointerV0`] locating
//! it. The turner reads the staged bytes out of the simulation's
//! post-execution account state, then submits the **executor** directly,
//! bracketed by relay's payment guards (`begin_guard_v0` … executor …
//! `assert_paid_v0`) so an underpaying crank reverts.
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
//! `#[repr(C)]` structs with natural alignment and no interior padding, so
//! programs read conditions in place (`bytemuck::from_bytes`) and update a
//! wake with a single field store — no serialization pass, no heap.
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

use alloc::vec::Vec;

use bytemuck::{Pod, Zeroable};

pub use bytemuck;

/// Magic bytes opening every condition block.
pub const MAGIC: [u8; 8] = *b"RELAY-V0";

/// Current wire version.
pub const SPEC_VERSION: u8 = 0;

/// Sentinel address in a resolver's staged account list that the turner
/// replaces with its keeper (payment recipient) before submitting.
pub const KEEPER_PLACEHOLDER: [u8; 32] = *b"relay/keeper/placeholder\0\0\0\0\0\0\0\0";

/// Fixed inline slots for a resolver's account list — sized for the common
/// resolver (the conditions account, a couple of state inputs). A resolver
/// needing more (e.g. one that CPIs an external quoter's registered
/// `quote_v0` surface under simulation) stores its list *next to the
/// condition block* on the same account and points at it with
/// `resolver_list_offset` — one copy per account instead of a fat inline
/// array per condition, read fresh by the turner on every attempt.
/// Where a condition's resolver account list lives: `count`
/// [`AccountRefV0`]s at `offset` bytes into the account holding the block.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ResolverListV0 {
    pub offset: u32,
    pub count: u8,
}

impl ResolverListV0 {
    pub fn new(offset: u32, count: u8) -> Self {
        Self { offset, count }
    }
}

/// Ceiling on an indirect resolver account list (`resolver_list_offset`).
pub const MAX_INDIRECT_RESOLVER_ACCOUNTS: usize = 64;

/// Anchor instruction discriminators of relay's payment guards. Pinned
/// here so turners don't need a hash dependency; the program's test suite
/// asserts they match the generated constants.
pub const BEGIN_GUARD_V0_DISCRIMINATOR: [u8; 8] = [151, 249, 122, 144, 42, 38, 241, 176];
pub const ASSERT_PAID_V0_DISCRIMINATOR: [u8; 8] = [117, 202, 135, 43, 41, 100, 21, 141];

/// PDA seed prefix for a keeper's guard account: `["guard", keeper, nonce]`.
pub const GUARD_SEED: &[u8] = b"guard";

/// Anchor account discriminator of relay's `WatchV0`
/// (`sha256("account:WatchV0")[..8]`).
pub const WATCH_V0_DISCRIMINATOR: [u8; 8] = [177, 10, 201, 159, 2, 232, 62, 244];

/// Serialized length of a `WatchV0` account:
/// `[disc: 8][target_program: 32][target: 32][registrar: 32][offset: u32][_pad: 4]`.
pub const WATCH_V0_LEN: usize = 112;

/// Byte offset of `target_program` in a `WatchV0` account. `target_program`
/// leads the struct so turners can memcmp-filter the registry server-side
/// (`getProgramAccounts` filters, geyser account filters) and never receive
/// the watches of programs they don't crank — the cheapest possible way to
/// ignore another protocol's expensive conditions.
pub const WATCH_TARGET_PROGRAM_OFFSET: usize = 8;

/// Byte offset of `target` in a `WatchV0` account.
pub const WATCH_TARGET_OFFSET: usize = 40;

/// Byte offset of `registrar` in a `WatchV0` account — memcmp-filterable
/// too, for turners that key off who registered rather than what program
/// owns the target.
pub const WATCH_REGISTRAR_OFFSET: usize = 72;

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
    /// Block offset (or buffer base) is not 8-aligned — zero-copy reads
    /// require the block to start on an 8-byte boundary.
    Misaligned,
    /// A count field exceeds its fixed capacity, or a payload exceeds the
    /// region it must fit in.
    TooLarge,
    /// Account discriminator mismatch.
    BadDiscriminator,
}

// --- condition block (zero-copy pod) ---

/// Block header. The block is `[header][ConditionV0; num_conditions]`,
/// contiguous, at an 8-aligned offset.
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Pod, Zeroable)]
pub struct ConditionBlockHeaderV0 {
    pub magic: [u8; 8],
    pub version: u8,
    pub num_conditions: u8,
    pub _pad: [u8; 6],
}

pub const BLOCK_HEADER_LEN: usize = core::mem::size_of::<ConditionBlockHeaderV0>();
const _: () = assert!(BLOCK_HEADER_LEN == 16);

impl ConditionBlockHeaderV0 {
    pub fn new(num_conditions: u8) -> Self {
        Self {
            magic: MAGIC,
            version: SPEC_VERSION,
            num_conditions,
            _pad: [0; 6],
        }
    }

    pub fn validate(&self) -> Result<(), SpecError> {
        if self.magic != MAGIC {
            return Err(SpecError::BadMagic);
        }
        if self.version > SPEC_VERSION {
            return Err(SpecError::UnsupportedVersion);
        }
        Ok(())
    }
}

/// One account in an instruction's account list. Never a signer: crank
/// executors are permissionless by contract, and relay's `crank_v0`
/// forwards every CPI account with `is_signer: false`. Align-1 (33 bytes)
/// so fixed arrays of it pack without padding.
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Pod, Zeroable)]
pub struct AccountRefV0 {
    pub address: [u8; 32],
    /// 0 = readonly, nonzero = writable.
    pub writable: u8,
}

pub const ACCOUNT_REF_LEN: usize = core::mem::size_of::<AccountRefV0>();
const _: () = assert!(ACCOUNT_REF_LEN == 33);

impl AccountRefV0 {
    pub fn readonly(address: [u8; 32]) -> Self {
        Self {
            address,
            writable: 0,
        }
    }

    pub fn writable(address: [u8; 32]) -> Self {
        Self {
            address,
            writable: 1,
        }
    }

    pub fn is_writable(&self) -> bool {
        self.writable != 0
    }
}

/// Wake kinds (the `wake_kind` byte on [`ConditionV0`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WakeKind {
    /// Due once the chain clock reaches `wake_ts`. The program updates the
    /// field in place (e.g. a min-over-inserts next-expiry; the executor
    /// recomputes the true value as it works, repairing the hint).
    AtTimestamp = 0,
    /// Due when `wake_account.data[wake_offset..wake_offset + wake_len]`
    /// differs from the turner's last-seen copy. The watched account may be
    /// the condition account itself or any other (e.g. an oracle).
    OnAccountChange = 1,
    /// Due every `wake_slot` slots — the fallback / pure-poll hint.
    EverySlots = 2,
    /// Due once the chain reaches slot `wake_slot` (absolute). The
    /// slot-denominated sibling of [`WakeKind::AtTimestamp`], updated in
    /// place the same way — for deadlines a program tracks in slots
    /// (activation delays, auction ends) rather than wall time.
    AtSlot = 3,
    /// Due while the watched value sits at-or-beyond a threshold: the
    /// bytes at `wake_account.data[wake_offset..wake_offset + wake_len]`
    /// read as a little-endian integer (widths 1/2/4/8; signed and
    /// sign-extended by default, unsigned and zero-extended when
    /// `wake_value_unsigned` is set — declared by the [`WatchValue`] the
    /// condition was built with), compared against `wake_ts` reinterpreted
    /// as the threshold, in the direction `wake_cmp` selects (0 = due when
    /// value >= threshold, 1 = due when value <= threshold).
    ///
    /// This is the wake for price-like conditions — trigger orders,
    /// liquidation thresholds — where `OnAccountChange` would wake on
    /// every oracle tick and burn a resolver simulation per condition per
    /// tick even when the price is nowhere near. Level-triggered: the
    /// condition stays due while the value is beyond the threshold, so the
    /// program must deactivate or re-point the condition once the work is
    /// done (and the turner's no-work backoff bounds the cost of a due
    /// condition whose resolver finds nothing).
    OnValueCross = 4,
}

/// Copied, alloc-free view of a condition's wake for evaluation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WakeView {
    AtTimestamp {
        unix_ts: i64,
    },
    OnAccountChange {
        address: [u8; 32],
        offset: u32,
        len: u32,
    },
    EverySlots {
        slots: u64,
    },
    AtSlot {
        slot: u64,
    },
    OnValueCross {
        address: [u8; 32],
        offset: u32,
        len: u32,
        threshold: WatchValue,
        /// 0 = due when value >= threshold, 1 = due when value <= threshold.
        cmp: u8,
    },
}

/// A watched integer with its signedness. On-chain fields are unsigned at
/// least as often as they are signed (token amounts, counters, most u64
/// prices), and sign-extending an unsigned field with its top bit set
/// inverts every comparison — so the threshold carries which domain it
/// lives in, and the watched bytes are always read in that same domain.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WatchValue {
    Signed(i64),
    Unsigned(u64),
}

impl WatchValue {
    pub fn is_unsigned(&self) -> bool {
        matches!(self, WatchValue::Unsigned(_))
    }

    /// Both domains fit in i128, so comparisons are uniform.
    pub fn widened(self) -> i128 {
        match self {
            WatchValue::Signed(v) => v as i128,
            WatchValue::Unsigned(v) => v as i128,
        }
    }
}

impl core::fmt::Display for WatchValue {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            WatchValue::Signed(v) => write!(f, "{v}"),
            WatchValue::Unsigned(v) => write!(f, "{v}u"),
        }
    }
}

/// Read a watched region as the little-endian integer
/// [`WakeKind::OnValueCross`] compares (widths 1/2/4/8; sign-extended when
/// signed, zero-extended when unsigned). `None` for any other width — an
/// unreadable value is never due.
pub fn read_watched_value(bytes: &[u8], unsigned: bool) -> Option<WatchValue> {
    if unsigned {
        let value = match bytes.len() {
            1 => u8::from_le_bytes(bytes.try_into().ok()?) as u64,
            2 => u16::from_le_bytes(bytes.try_into().ok()?) as u64,
            4 => u32::from_le_bytes(bytes.try_into().ok()?) as u64,
            8 => u64::from_le_bytes(bytes.try_into().ok()?),
            _ => return None,
        };
        return Some(WatchValue::Unsigned(value));
    }
    let value = match bytes.len() {
        1 => i8::from_le_bytes(bytes.try_into().ok()?) as i64,
        2 => i16::from_le_bytes(bytes.try_into().ok()?) as i64,
        4 => i32::from_le_bytes(bytes.try_into().ok()?) as i64,
        8 => i64::from_le_bytes(bytes.try_into().ok()?),
        _ => return None,
    };
    Some(WatchValue::Signed(value))
}

/// Whether an [`WakeKind::OnValueCross`] condition is due for `value`.
pub fn value_crossed(value: WatchValue, threshold: WatchValue, cmp: u8) -> bool {
    match cmp {
        0 => value.widened() >= threshold.widened(),
        _ => value.widened() <= threshold.widened(),
    }
}

/// Everything about a condition except its wake — the arguments shared by
/// all the constructors.
#[derive(Debug, Clone, Copy)]
pub struct CrankSpecV0 {
    pub resolver_program: [u8; 32],
    pub resolver_disc: [u8; 8],
    pub executor_program: [u8; 32],
    pub executor_disc: [u8; 8],
    pub min_payment: u64,
}

/// One crankable condition. Fixed 280 bytes, natural alignment 8, no
/// interior padding: read it in place, update wake inputs with single field
/// stores. Wake variants are flattened into dedicated fields (selected by
/// `wake_kind`) rather than an enum, precisely so a program can do
/// `conditions[i].wake_ts = new_min` and touch nothing else.
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Pod, Zeroable)]
pub struct ConditionV0 {
    /// Lamports the executor must pay the keeper. `crank_v0` asserts the
    /// keeper's balance grew by at least this much; turners use it to
    /// decide whether a crank is worth the fee.
    pub(crate) min_payment: u64,
    /// [`WakeKind::AtTimestamp`] input.
    pub(crate) wake_ts: i64,
    /// [`WakeKind::EverySlots`] input (interval) or [`WakeKind::AtSlot`]
    /// input (absolute slot) — selected by `wake_kind`.
    pub(crate) wake_slot: u64,
    /// [`WakeKind::OnAccountChange`] inputs.
    pub(crate) wake_account: [u8; 32],
    pub(crate) wake_offset: u32,
    pub(crate) wake_len: u32,
    /// Instruction the turner simulates to discover work. Stages its
    /// payload in one of `resolver_accounts` and returns a
    /// [`ResponsePointerV0`].
    pub(crate) resolver_program: [u8; 32],
    pub(crate) resolver_disc: [u8; 8],
    /// Instruction that does the work and pays the keeper. Its account list
    /// and trailing args come from the resolver's staged payload.
    pub(crate) executor_program: [u8; 32],
    pub(crate) executor_disc: [u8; 8],
    /// How many [`AccountRefV0`]s make up the resolver's account list. The
    /// list itself lives at `resolver_list_offset`; see there.
    pub(crate) num_resolver_accounts: u8,
    /// A [`WakeKind`] value.
    pub(crate) wake_kind: u8,
    /// 0 = inactive (skipped by turners, rejected by `crank_v0`).
    pub(crate) active: u8,
    /// [`WakeKind::OnValueCross`] comparator: 0 = due when value >=
    /// threshold (`wake_ts`), 1 = due when value <= threshold.
    pub(crate) wake_cmp: u8,
    /// Byte offset, within *this condition block's own account*, of the
    /// `num_resolver_accounts` [`AccountRefV0`]s that form the resolver's
    /// account list. The staging account must be among them and marked
    /// writable.
    ///
    /// The list is always indirect. Conditions used to carry four inline
    /// refs, which cost 132 of every condition's 288 bytes whether or not
    /// they were used — and any resolver needing more than four had to
    /// point somewhere anyway. One list per account, shared by every
    /// condition that wants it, is both smaller and the only mechanism to
    /// reason about.
    pub(crate) resolver_list_offset: u32,
    /// [`WakeKind::OnValueCross`]: nonzero = the watched bytes and the
    /// threshold (`wake_ts`'s bits reinterpreted as `u64`) are unsigned.
    /// Zero — the value every pre-existing block already holds — keeps the
    /// original signed reading.
    pub(crate) wake_value_unsigned: u8,
    /// Reserved. Zero on write, ignored on read — room to add fields
    /// without moving every existing one or resizing every account that
    /// holds a block.
    pub(crate) _reserved: [u8; 39],
}

pub const CONDITION_LEN: usize = core::mem::size_of::<ConditionV0>();
const _: () = assert!(CONDITION_LEN == 192);
const _: () = assert!(core::mem::align_of::<ConditionV0>() == 8);

impl ConditionV0 {
    fn base(spec: CrankSpecV0, resolvers: ResolverListV0) -> Self {
        Self {
            min_payment: spec.min_payment,
            wake_ts: 0,
            wake_slot: 0,
            wake_account: [0; 32],
            wake_offset: 0,
            wake_len: 0,
            resolver_program: spec.resolver_program,
            resolver_disc: spec.resolver_disc,
            executor_program: spec.executor_program,
            executor_disc: spec.executor_disc,
            num_resolver_accounts: resolvers.count,
            wake_kind: 0,
            active: 1,
            wake_cmp: 0,
            resolver_list_offset: resolvers.offset,
            wake_value_unsigned: 0,
            _reserved: [0; 39],
        }
    }

    pub fn at_timestamp(unix_ts: i64, spec: CrankSpecV0, resolvers: ResolverListV0) -> Self {
        let mut c = Self::base(spec, resolvers);
        c.wake_kind = WakeKind::AtTimestamp as u8;
        c.wake_ts = unix_ts;
        c
    }

    pub fn on_account_change(
        watched: [u8; 32],
        offset: u32,
        len: u32,
        spec: CrankSpecV0,
        resolvers: ResolverListV0,
    ) -> Self {
        let mut c = Self::base(spec, resolvers);
        c.wake_kind = WakeKind::OnAccountChange as u8;
        c.wake_account = watched;
        c.wake_offset = offset;
        c.wake_len = len;
        c
    }

    /// Level-triggered value threshold (see [`WakeKind::OnValueCross`]):
    /// due while the watched value is at-or-beyond `threshold` in the
    /// direction `cmp` selects (0 = >=, 1 = <=). The threshold's
    /// [`WatchValue`] variant declares the watched field's signedness —
    /// the caller knows the layout it is watching, and reading an unsigned
    /// field sign-extended inverts the comparison once the top bit is set.
    #[allow(clippy::too_many_arguments)]
    pub fn on_value_cross(
        watched: [u8; 32],
        offset: u32,
        len: u32,
        threshold: WatchValue,
        cmp: u8,
        spec: CrankSpecV0,
        resolvers: ResolverListV0,
    ) -> Self {
        let mut c = Self::base(spec, resolvers);
        c.wake_kind = WakeKind::OnValueCross as u8;
        c.wake_account = watched;
        c.wake_offset = offset;
        c.wake_len = len;
        c.wake_cmp = cmp;
        match threshold {
            WatchValue::Signed(value) => c.wake_ts = value,
            WatchValue::Unsigned(value) => {
                c.wake_ts = value as i64;
                c.wake_value_unsigned = 1;
            }
        }
        c
    }

    pub fn every_slots(slots: u64, spec: CrankSpecV0, resolvers: ResolverListV0) -> Self {
        let mut c = Self::base(spec, resolvers);
        c.wake_kind = WakeKind::EverySlots as u8;
        c.wake_slot = slots;
        c
    }

    pub fn at_slot(slot: u64, spec: CrankSpecV0, resolvers: ResolverListV0) -> Self {
        let mut c = Self::base(spec, resolvers);
        c.wake_kind = WakeKind::AtSlot as u8;
        c.wake_slot = slot;
        c
    }

    /// Lamports the executor must pay the keeper.
    pub fn min_payment(&self) -> u64 {
        self.min_payment
    }

    /// Where this condition's resolver account list lives.
    pub fn resolvers(&self) -> ResolverListV0 {
        ResolverListV0::new(self.resolver_list_offset, self.num_resolver_accounts)
    }

    /// The programs and discriminators to simulate and then submit.
    pub fn crank_spec(&self) -> CrankSpecV0 {
        CrankSpecV0 {
            resolver_program: self.resolver_program,
            resolver_disc: self.resolver_disc,
            executor_program: self.executor_program,
            executor_disc: self.executor_disc,
            min_payment: self.min_payment,
        }
    }

    /// Point this condition at a different resolver account list.
    pub fn set_resolvers(&mut self, resolvers: ResolverListV0) {
        self.resolver_list_offset = resolvers.offset;
        self.num_resolver_accounts = resolvers.count;
    }

    /// Re-price the keeper fee (hosts re-price in place on reconfigure).
    pub fn set_min_payment(&mut self, lamports: u64) {
        self.min_payment = lamports;
    }

    /// Go quiet. A level-triggered wake fires until this is called.
    pub fn deactivate(&mut self) {
        self.active = 0;
    }

    /// Rewrite the wake, by variant. The wire format overloads its fields
    /// — `wake_ts` is a timestamp for one kind and a threshold for another
    /// — so this is the only sanctioned way to change one: it always
    /// writes the discriminant and the fields together, and clears what
    /// the new variant does not use.
    pub fn set_wake(&mut self, wake: WakeView) {
        self.wake_ts = 0;
        self.wake_slot = 0;
        self.wake_account = [0; 32];
        self.wake_offset = 0;
        self.wake_len = 0;
        self.wake_cmp = 0;
        self.wake_value_unsigned = 0;
        match wake {
            WakeView::AtTimestamp { unix_ts } => {
                self.wake_kind = WakeKind::AtTimestamp as u8;
                self.wake_ts = unix_ts;
            }
            WakeView::OnAccountChange {
                address,
                offset,
                len,
            } => {
                self.wake_kind = WakeKind::OnAccountChange as u8;
                self.wake_account = address;
                self.wake_offset = offset;
                self.wake_len = len;
            }
            WakeView::EverySlots { slots } => {
                self.wake_kind = WakeKind::EverySlots as u8;
                self.wake_slot = slots;
            }
            WakeView::AtSlot { slot } => {
                self.wake_kind = WakeKind::AtSlot as u8;
                self.wake_slot = slot;
            }
            WakeView::OnValueCross {
                address,
                offset,
                len,
                threshold,
                cmp,
            } => {
                self.wake_kind = WakeKind::OnValueCross as u8;
                self.wake_account = address;
                self.wake_offset = offset;
                self.wake_len = len;
                self.wake_cmp = cmp;
                match threshold {
                    WatchValue::Signed(value) => self.wake_ts = value,
                    WatchValue::Unsigned(value) => {
                        self.wake_ts = value as i64;
                        self.wake_value_unsigned = 1;
                    }
                }
            }
        }
    }

    pub fn is_active(&self) -> bool {
        self.active != 0
    }

    pub fn wake(&self) -> Result<WakeView, SpecError> {
        match self.wake_kind {
            0 => Ok(WakeView::AtTimestamp {
                unix_ts: self.wake_ts,
            }),
            1 => Ok(WakeView::OnAccountChange {
                address: self.wake_account,
                offset: self.wake_offset,
                len: self.wake_len,
            }),
            2 => Ok(WakeView::EverySlots {
                slots: self.wake_slot,
            }),
            3 => Ok(WakeView::AtSlot {
                slot: self.wake_slot,
            }),
            4 => Ok(WakeView::OnValueCross {
                address: self.wake_account,
                offset: self.wake_offset,
                len: self.wake_len,
                threshold: if self.wake_value_unsigned != 0 {
                    WatchValue::Unsigned(self.wake_ts as u64)
                } else {
                    WatchValue::Signed(self.wake_ts)
                },
                cmp: self.wake_cmp,
            }),
            _ => Err(SpecError::BadWakeKind),
        }
    }
}

/// Implement [`ConditionBlock`] for an account that keeps its block in a
/// byte field, and pin the invariants the reader depends on.
///
/// Hosts were writing this by hand: the impl, plus a set of offset
/// constants derived by adding up the lengths of every preceding field.
/// That arithmetic is the one part of hosting a block that is easy to get
/// silently wrong — a stale summand puts a region's offset a few bytes off
/// and the failure surfaces as a turner reading garbage, far away. Here it
/// comes from `offset_of!` instead, so it cannot disagree with the struct.
///
/// ```ignore
/// condition_block!(UserConditionsV0, block, USER_CONDITIONS);
/// ```
#[macro_export]
macro_rules! condition_block {
    ($ty:ty, $field:ident, $n:expr) => {
        impl $crate::ConditionBlock for $ty {
            const NUM_CONDITIONS: usize = $n;

            fn block(&self) -> &[u8] {
                &self.$field
            }

            fn block_mut(&mut self) -> &mut [u8] {
                &mut self.$field
            }
        }

        const _: () = {
            // The block must be 8-aligned within the account for the
            // zero-copy readers, and big enough for the conditions it
            // claims to hold.
            assert!($crate::block_offset!($ty, $field) % 8 == 0);
        };
    };
}

/// Account-data offset of a field: past anchor's 8-byte discriminator.
/// Every region a condition points at — resolver lists, staging — must be
/// described this way rather than by summing field lengths.
#[macro_export]
macro_rules! block_offset {
    ($ty:ty, $field:ident) => {
        8 + core::mem::offset_of!($ty, $field)
    };
}

/// Write a resolved crank into a staging buffer and describe where it
/// landed.
///
/// Staging does not have to live on the block's own account. It is written
/// only under simulation and read back out of the simulated post-state, so
/// nothing about it needs to persist and concurrent turners cannot collide
/// — which means one shared scratch account can serve every block instead
/// of every account carrying its own kilobytes of scratch. `account_index`
/// is the slot that account occupies in the resolver's account list, and
/// `offset` is where in its data the payload starts; together they are
/// what a [`ResponsePointerV0`] means.
pub fn stage_into(
    staging: &mut [u8],
    account_index: u8,
    offset: u32,
    resolved: &ResolvedCrankV0,
) -> Result<[u8; RESPONSE_POINTER_LEN], SpecError> {
    let len = resolved.write_into(staging)?;
    Ok(ResponsePointerV0::new(account_index, offset, len as u32).to_bytes())
}

/// Bytes a block of `n` conditions occupies.
pub const fn block_space(n: usize) -> usize {
    BLOCK_HEADER_LEN + n * CONDITION_LEN
}

/// Zero-copy view of a block at `data[offset..]`. The block must sit on an
/// 8-byte boundary — guaranteed on-chain for account data at an 8-aligned
/// offset; off-chain callers with arbitrary buffers use
/// [`read_conditions_unaligned`].
pub fn read_block(
    data: &[u8],
    offset: usize,
) -> Result<(&ConditionBlockHeaderV0, &[ConditionV0]), SpecError> {
    let header_end = offset
        .checked_add(BLOCK_HEADER_LEN)
        .ok_or(SpecError::Truncated)?;
    if header_end > data.len() {
        return Err(SpecError::Truncated);
    }
    let header: &ConditionBlockHeaderV0 =
        bytemuck::try_from_bytes(&data[offset..header_end]).map_err(|_| SpecError::Misaligned)?;
    header.validate()?;
    let n = header.num_conditions as usize;
    let end = header_end
        .checked_add(n * CONDITION_LEN)
        .ok_or(SpecError::Truncated)?;
    if end > data.len() {
        return Err(SpecError::Truncated);
    }
    let conditions =
        bytemuck::try_cast_slice(&data[header_end..end]).map_err(|_| SpecError::Misaligned)?;
    Ok((header, conditions))
}

/// Mutable zero-copy view of the conditions, for programs updating wake
/// inputs in place.
pub fn read_block_mut(data: &mut [u8], offset: usize) -> Result<&mut [ConditionV0], SpecError> {
    let n = {
        let (header, _) = read_block(data, offset)?;
        header.num_conditions as usize
    };
    let start = offset + BLOCK_HEADER_LEN;
    bytemuck::try_cast_slice_mut(&mut data[start..start + n * CONDITION_LEN])
        .map_err(|_| SpecError::Misaligned)
}

/// Write a fresh block (header + conditions) into a region. Errors if the
/// region is too small; trailing bytes are left untouched.
pub fn write_block(region: &mut [u8], conditions: &[ConditionV0]) -> Result<usize, SpecError> {
    if conditions.len() > u8::MAX as usize {
        return Err(SpecError::TooLarge);
    }
    let total = block_space(conditions.len());
    if total > region.len() {
        return Err(SpecError::TooLarge);
    }
    region[..BLOCK_HEADER_LEN].copy_from_slice(bytemuck::bytes_of(&ConditionBlockHeaderV0::new(
        conditions.len() as u8,
    )));
    region[BLOCK_HEADER_LEN..total].copy_from_slice(bytemuck::cast_slice(conditions));
    Ok(total)
}

/// A program account that hosts a condition block plus a resolver staging
/// region — the shape every relay-integrated program repeats.
///
/// Implement the three accessors and the two consts; everything a program
/// actually calls (stamp the header, write/read a condition slot,
/// deactivate one, stage a resolved crank) comes for free. Without this,
/// each hosting account hand-rolls the same sixty lines of offset
/// arithmetic — and an off-by-one in it is a silently unreadable block.
///
/// The block must live at an 8-aligned account-data offset (see
/// [`read_block`]).
///
/// Staging is deliberately not part of this. A host does not have to
/// carry scratch of its own — see [`stage_into`] — so where a resolved
/// crank lands is the resolver's decision, not a property of the account
/// holding the conditions.
pub trait ConditionBlock {
    /// Conditions the block holds. Slots are addressed by index, so this
    /// is fixed per account type.
    const NUM_CONDITIONS: usize;

    fn block(&self) -> &[u8];
    fn block_mut(&mut self) -> &mut [u8];

    /// Bring a block written by an older spec up to the current one, in
    /// place, and report whether anything changed.
    ///
    /// Hosts should call this before reading a block they did not write in
    /// the same instruction. Today every supported version is the current
    /// one, so it only re-stamps a header — but the call site is the point:
    /// when a v1 arrives, hosts that already call this get the conversion
    /// without changing, and hosts that never called it were going to
    /// misread the block anyway.
    fn migrate(&mut self) -> Result<bool, SpecError> {
        // Read the header bytewise rather than through `read_block`: a
        // migration must work on a block whose alignment is whatever the
        // old layout left, which is the case this exists to handle.
        let bytes = self.block();
        let head = bytes.get(..BLOCK_HEADER_LEN).ok_or(SpecError::Truncated)?;
        if head[..8] != MAGIC {
            return Err(SpecError::BadMagic);
        }
        let (version, num_conditions) = (head[8], head[9] as usize);
        if version > SPEC_VERSION {
            return Err(SpecError::UnsupportedVersion);
        }
        if version == SPEC_VERSION && num_conditions == Self::NUM_CONDITIONS {
            return Ok(false);
        }
        // Only same-shape re-stamping is possible at v0; a real conversion
        // lands here when the layout actually changes.
        if num_conditions != Self::NUM_CONDITIONS {
            return Err(SpecError::TooLarge);
        }
        self.init_header()?;
        Ok(true)
    }

    /// Stamp the spec header. Conditions are written separately, by index,
    /// so a freshly zeroed account is a valid (inactive) block from the
    /// first write.
    fn init_header(&mut self) -> Result<(), SpecError> {
        let header = ConditionBlockHeaderV0::new(Self::NUM_CONDITIONS as u8);
        let block = self.block_mut();
        if block.len() < BLOCK_HEADER_LEN {
            return Err(SpecError::Truncated);
        }
        block[..BLOCK_HEADER_LEN].copy_from_slice(bytemuck::bytes_of(&header));
        Ok(())
    }

    /// Overwrite one condition slot.
    fn write_condition(&mut self, index: usize, condition: &ConditionV0) -> Result<(), SpecError> {
        let start = Self::slot_start(index)?;
        let block = self.block_mut();
        if start + CONDITION_LEN > block.len() {
            return Err(SpecError::Truncated);
        }
        block[start..start + CONDITION_LEN].copy_from_slice(bytemuck::bytes_of(condition));
        Ok(())
    }

    /// Read one condition slot back (copying — the region may be
    /// unaligned in an account's data).
    fn read_condition(&self, index: usize) -> Result<ConditionV0, SpecError> {
        let start = Self::slot_start(index)?;
        let block = self.block();
        if start + CONDITION_LEN > block.len() {
            return Err(SpecError::Truncated);
        }
        let mut condition = ConditionV0::zeroed();
        bytemuck::bytes_of_mut(&mut condition)
            .copy_from_slice(&block[start..start + CONDITION_LEN]);
        Ok(condition)
    }

    /// Zero a slot — the way work that is done goes quiet. A
    /// level-triggered wake (see [`WakeKind::OnValueCross`]) that is left
    /// active after its work is finished keeps waking turners, so
    /// releasing the slot is part of the executor's job, not an
    /// optimization.
    fn deactivate_condition(&mut self, index: usize) -> Result<(), SpecError> {
        let start = Self::slot_start(index)?;
        let block = self.block_mut();
        if start + CONDITION_LEN > block.len() {
            return Err(SpecError::Truncated);
        }
        block[start..start + CONDITION_LEN].fill(0);
        Ok(())
    }

    /// Mutate one slot's wake inputs in place (the min-fold/repair
    /// pattern): read, apply, write back.
    fn update_condition(
        &mut self,
        index: usize,
        f: impl FnOnce(&mut ConditionV0),
    ) -> Result<(), SpecError> {
        let mut condition = self.read_condition(index)?;
        f(&mut condition);
        self.write_condition(index, &condition)
    }

    /// Write a resolver's payload into the staging region and return the
    /// pointer bytes to set as return data.

    #[doc(hidden)]
    fn slot_start(index: usize) -> Result<usize, SpecError> {
        if index >= Self::NUM_CONDITIONS {
            return Err(SpecError::TooLarge);
        }
        Ok(BLOCK_HEADER_LEN + index * CONDITION_LEN)
    }
}

/// Everything an account must host for relay, in one field: the spec
/// header, the condition slots, and a resolver account list region the
/// conditions point at. Declare it, call [`Self::init`] once at account
/// creation with the field's own account offset, and the offset arithmetic
/// hosts used to hand-roll (three regions, each with its own length
/// constant and alignment note) disappears:
///
/// ```ignore
/// #[account(zero_copy)]
/// pub struct MyThing {
///     pub relay: RelayBlockV0<NUM_CONDITIONS, 8>,
///     // ... host fields ...
/// }
/// // once, at account creation:
/// my_thing.relay.init(relay_spec::block_offset!(MyThing, relay) as u32)?;
/// // register the watch at my_thing.relay.account_offset()
/// // point conditions at the built-in region:
/// let list = my_thing.relay.write_resolvers(&refs)?;
/// ```
///
/// Everything is stored as byte arrays, so the type is alignment-1 and
/// carries no padding for any parameter choice — genuinely `Pod`, strict
/// enough for anchor v2's `#[account]`. `RESOLVER_CAPACITY` is a capacity
/// (live count is tracked separately, conditions carry their own counts)
/// and must be a multiple of 8, which keeps the total size 8-divisible so
/// the field never forces padding into its host.
///
/// The condition surface ([`ConditionBlock`]) is implemented on it, and
/// [`Self::init`] stamps both the header and the field's account offset,
/// which is what lets [`Self::write_resolvers`] return a
/// [`ResolverListV0`] without the host doing offset math again. Hosts
/// with extra list regions of their own (per-slot lists, oversized shared
/// maps) keep building `ResolverListV0`s from [`block_offset!`] as before
/// — the built-in region is the common case, not a limit.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct RelayBlockV0<const CONDITIONS: usize, const RESOLVER_CAPACITY: usize> {
    header: [u8; BLOCK_HEADER_LEN],
    conditions: [[u8; CONDITION_LEN]; CONDITIONS],
    resolvers: [[u8; ACCOUNT_REF_LEN]; RESOLVER_CAPACITY],
    /// This field's own byte offset within its account (past the
    /// discriminator), stamped by [`Self::init`].
    self_offset: [u8; 4],
    /// Live entries in `resolvers`.
    resolver_count: u8,
    _reserved: [u8; 11],
}

// Sound for any parameters: every field is a byte array, so the struct is
// alignment-1 and can contain no padding.
unsafe impl<const C: usize, const R: usize> Zeroable for RelayBlockV0<C, R> {}
unsafe impl<const C: usize, const R: usize> Pod for RelayBlockV0<C, R> {}

impl<const C: usize, const R: usize> Default for RelayBlockV0<C, R> {
    fn default() -> Self {
        Self::zeroed()
    }
}

impl<const C: usize, const R: usize> RelayBlockV0<C, R> {
    /// See the type docs: a capacity granularity of 8 keeps the total size
    /// 8-divisible, so hosting this field never smuggles padding into a
    /// strict-Pod account struct. Referenced by every method, so a bad
    /// parameter fails compilation at the first use.
    const LAYOUT: () = assert!(
        R % 8 == 0,
        "RelayBlockV0 resolver capacity must be a multiple of 8"
    );

    pub const SIZE: usize = BLOCK_HEADER_LEN + C * CONDITION_LEN + R * ACCOUNT_REF_LEN + 16;

    /// Stamp the spec header and record where in its account this field
    /// lives (`block_offset!(Host, field)`). Call once at account
    /// creation; conditions are written separately, by index.
    pub fn init(&mut self, account_offset: u32) -> Result<(), SpecError> {
        #[allow(clippy::let_unit_value)]
        let _ = Self::LAYOUT;
        self.self_offset = account_offset.to_le_bytes();
        self.init_header()
    }

    /// The account-data offset this block was initialized at — what a
    /// `WatchV0` registration points at.
    pub fn account_offset(&self) -> u32 {
        u32::from_le_bytes(self.self_offset)
    }

    /// Store the resolver account list in the built-in region and describe
    /// it the way a [`ConditionV0`] wants it. Requires [`Self::init`] to
    /// have stamped the offset — an unstamped block cannot self-describe.
    pub fn write_resolvers(&mut self, refs: &[AccountRefV0]) -> Result<ResolverListV0, SpecError> {
        #[allow(clippy::let_unit_value)]
        let _ = Self::LAYOUT;
        if refs.len() > R || refs.len() > MAX_INDIRECT_RESOLVER_ACCOUNTS {
            return Err(SpecError::TooLarge);
        }
        if self.account_offset() == 0 {
            return Err(SpecError::Truncated);
        }
        for (slot, r) in self.resolvers.iter_mut().zip(refs) {
            slot[..32].copy_from_slice(&r.address);
            slot[32] = r.writable;
        }
        self.resolver_count = refs.len() as u8;
        let region = BLOCK_HEADER_LEN + C * CONDITION_LEN;
        Ok(ResolverListV0::new(
            self.account_offset() + region as u32,
            refs.len() as u8,
        ))
    }

    /// The stored resolver list, as last written.
    pub fn resolver_refs(&self) -> Vec<AccountRefV0> {
        self.resolvers[..(self.resolver_count as usize).min(R)]
            .iter()
            .map(|slot| AccountRefV0 {
                address: slot[..32].try_into().expect("32-byte address"),
                writable: slot[32],
            })
            .collect()
    }
}

impl<const C: usize, const R: usize> ConditionBlock for RelayBlockV0<C, R> {
    const NUM_CONDITIONS: usize = C;

    fn block(&self) -> &[u8] {
        &bytemuck::bytes_of(self)[..BLOCK_HEADER_LEN + C * CONDITION_LEN]
    }

    fn block_mut(&mut self) -> &mut [u8] {
        &mut bytemuck::bytes_of_mut(self)[..BLOCK_HEADER_LEN + C * CONDITION_LEN]
    }
}

/// Copying reader for arbitrary (possibly unaligned) buffers — the
/// off-chain path. 280 bytes copied per condition; negligible off-chain.
pub fn read_conditions_unaligned(
    data: &[u8],
    offset: usize,
) -> Result<Vec<ConditionV0>, SpecError> {
    let header_end = offset
        .checked_add(BLOCK_HEADER_LEN)
        .ok_or(SpecError::Truncated)?;
    if header_end > data.len() {
        return Err(SpecError::Truncated);
    }
    let header: ConditionBlockHeaderV0 = bytemuck::pod_read_unaligned(&data[offset..header_end]);
    header.validate()?;
    let n = header.num_conditions as usize;
    let end = header_end
        .checked_add(n * CONDITION_LEN)
        .ok_or(SpecError::Truncated)?;
    if end > data.len() {
        return Err(SpecError::Truncated);
    }
    Ok((0..n)
        .map(|i| {
            let start = header_end + i * CONDITION_LEN;
            bytemuck::pod_read_unaligned(&data[start..start + CONDITION_LEN])
        })
        .collect())
}

// --- resolver output ---

/// A resolver's return data: whether there is work, and where the payload
/// was staged. Align-1 pod, 10 bytes — well inside the return-data cap no
/// matter how large the payload is.
///
/// `account_index` indexes the condition's `resolver_accounts`; `offset`
/// and `len` are a byte range in that account's **post-simulation** data.
/// A no-work result needs no staging write at all.
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Pod, Zeroable)]
pub struct ResponsePointerV0 {
    /// 0 = nothing to do right now.
    pub work: u8,
    pub account_index: u8,
    /// LE byte arrays: keeps the struct align-1 so it reads out of return
    /// data (or any buffer) without alignment games.
    pub offset: [u8; 4],
    pub len: [u8; 4],
}

pub const RESPONSE_POINTER_LEN: usize = core::mem::size_of::<ResponsePointerV0>();
const _: () = assert!(RESPONSE_POINTER_LEN == 10);
const _: () = assert!(core::mem::align_of::<ResponsePointerV0>() == 1);

impl ResponsePointerV0 {
    pub fn no_work() -> Self {
        Self::zeroed()
    }

    pub fn new(account_index: u8, offset: u32, len: u32) -> Self {
        Self {
            work: 1,
            account_index,
            offset: offset.to_le_bytes(),
            len: len.to_le_bytes(),
        }
    }

    pub fn has_work(&self) -> bool {
        self.work != 0
    }

    pub fn offset(&self) -> u32 {
        u32::from_le_bytes(self.offset)
    }

    pub fn len(&self) -> u32 {
        u32::from_le_bytes(self.len)
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn to_bytes(&self) -> [u8; RESPONSE_POINTER_LEN] {
        let mut out = [0u8; RESPONSE_POINTER_LEN];
        out.copy_from_slice(bytemuck::bytes_of(self));
        out
    }

    /// Parse from return data. Trailing bytes are ignored; RPC transports
    /// that strip trailing zeros are handled by zero-extending short input.
    pub fn read(bytes: &[u8]) -> Result<Self, SpecError> {
        if bytes.is_empty() {
            return Err(SpecError::Truncated);
        }
        let mut buf = [0u8; RESPONSE_POINTER_LEN];
        let n = bytes.len().min(RESPONSE_POINTER_LEN);
        buf[..n].copy_from_slice(&bytes[..n]);
        Ok(bytemuck::pod_read_unaligned(&buf))
    }
}

/// The staged payload a [`ResponsePointerV0`] points at. Align-1 wire:
/// `[num_accounts: u8][data_len: u16 LE][AccountRefV0; num][data bytes]`
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ResolvedCrankV0 {
    /// Full executor account list, in order. [`KEEPER_PLACEHOLDER`] entries
    /// are replaced with the turner's keeper.
    pub accounts: Vec<AccountRefV0>,
    /// Executor args after the 8-byte discriminator.
    pub data: Vec<u8>,
}

pub const RESOLVED_HEADER_LEN: usize = 3;

impl ResolvedCrankV0 {
    /// Bytes this payload needs when staged.
    pub fn encoded_len(&self) -> usize {
        RESOLVED_HEADER_LEN + self.accounts.len() * ACCOUNT_REF_LEN + self.data.len()
    }

    pub fn read(bytes: &[u8]) -> Result<Self, SpecError> {
        if bytes.len() < RESOLVED_HEADER_LEN {
            return Err(SpecError::Truncated);
        }
        let n = bytes[0] as usize;
        let data_len = u16::from_le_bytes([bytes[1], bytes[2]]) as usize;
        let accounts_end = RESOLVED_HEADER_LEN + n * ACCOUNT_REF_LEN;
        let end = accounts_end + data_len;
        if end > bytes.len() {
            return Err(SpecError::Truncated);
        }
        let accounts = (0..n)
            .map(|i| {
                let start = RESOLVED_HEADER_LEN + i * ACCOUNT_REF_LEN;
                bytemuck::pod_read_unaligned(&bytes[start..start + ACCOUNT_REF_LEN])
            })
            .collect();
        Ok(Self {
            accounts,
            data: bytes[accounts_end..end].to_vec(),
        })
    }

    /// Stage into a program-owned region (no alloc). Returns the used
    /// length, which becomes the pointer's `len`.
    pub fn write_into(&self, region: &mut [u8]) -> Result<usize, SpecError> {
        if self.accounts.len() > u8::MAX as usize || self.data.len() > u16::MAX as usize {
            return Err(SpecError::TooLarge);
        }
        let accounts_end = RESOLVED_HEADER_LEN + self.accounts.len() * ACCOUNT_REF_LEN;
        let end = accounts_end + self.data.len();
        if end > region.len() {
            return Err(SpecError::TooLarge);
        }
        region[0] = self.accounts.len() as u8;
        region[1..3].copy_from_slice(&(self.data.len() as u16).to_le_bytes());
        self.accounts.iter().enumerate().for_each(|(i, a)| {
            let start = RESOLVED_HEADER_LEN + i * ACCOUNT_REF_LEN;
            region[start..start + ACCOUNT_REF_LEN].copy_from_slice(bytemuck::bytes_of(a));
        });
        region[accounts_end..end].copy_from_slice(&self.data);
        Ok(end)
    }

    pub fn to_bytes(&self) -> Vec<u8> {
        let mut buf = alloc::vec![0u8; self.encoded_len()];
        self.write_into(&mut buf).expect("sized to fit");
        buf
    }
}

// --- registry ---

/// A parsed relay `WatchV0` registry account.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WatchV0 {
    /// Owner program of `target`, recorded by the relay program from the
    /// account itself at registration — a registrar cannot forge it.
    pub target_program: [u8; 32],
    pub target: [u8; 32],
    pub registrar: [u8; 32],
    pub offset: u32,
}

impl WatchV0 {
    /// Parse a `WatchV0` from raw account data (discriminator included).
    pub fn read_from_account(data: &[u8]) -> Result<Self, SpecError> {
        if data.len() < WATCH_V0_LEN {
            return Err(SpecError::Truncated);
        }
        if data[..8] != WATCH_V0_DISCRIMINATOR {
            return Err(SpecError::BadDiscriminator);
        }
        let field = |offset: usize| -> [u8; 32] { data[offset..offset + 32].try_into().unwrap() };
        Ok(Self {
            target_program: field(WATCH_TARGET_PROGRAM_OFFSET),
            target: field(WATCH_TARGET_OFFSET),
            registrar: field(WATCH_REGISTRAR_OFFSET),
            offset: u32::from_le_bytes(data[104..108].try_into().unwrap()),
        })
    }
}

/// Encode `begin_guard_v0`'s instruction data:
/// `[BEGIN_GUARD_V0_DISCRIMINATOR][nonce: u8]`.
pub fn encode_begin_guard_v0_data(nonce: u8) -> [u8; 9] {
    let mut out = [0u8; 9];
    out[..8].copy_from_slice(&BEGIN_GUARD_V0_DISCRIMINATOR);
    out[8] = nonce;
    out
}

/// Encode `assert_paid_v0`'s instruction data:
/// `[ASSERT_PAID_V0_DISCRIMINATOR][min_payment: u64][nonce: u8]`.
pub fn encode_assert_paid_v0_data(min_payment: u64, nonce: u8) -> [u8; 17] {
    let mut out = [0u8; 17];
    out[..8].copy_from_slice(&ASSERT_PAID_V0_DISCRIMINATOR);
    out[8..16].copy_from_slice(&min_payment.to_le_bytes());
    out[16] = nonce;
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec(min_payment: u64) -> CrankSpecV0 {
        CrankSpecV0 {
            resolver_program: [1; 32],
            resolver_disc: [9; 8],
            executor_program: [1; 32],
            executor_disc: [7; 8],
            min_payment,
        }
    }

    fn sample_conditions() -> Vec<ConditionV0> {
        vec![
            ConditionV0::at_timestamp(i64::MAX, spec(5000), ResolverListV0::new(64, 1)),
            ConditionV0::on_account_change([3; 32], 48, 4, spec(1), ResolverListV0::new(64, 1)),
            ConditionV0::every_slots(300, spec(0), ResolverListV0::new(0, 0)),
        ]
    }

    #[test]
    fn layout_is_pinned() {
        assert_eq!(CONDITION_LEN, 192);
        assert_eq!(BLOCK_HEADER_LEN, 16);
        assert_eq!(ACCOUNT_REF_LEN, 33);
        assert_eq!(RESPONSE_POINTER_LEN, 10);
        assert_eq!(block_space(2), 400);
    }

    #[test]
    fn block_round_trip_aligned() {
        let conditions = sample_conditions();
        // 8-aligned backing store.
        let mut region = vec![0u64; block_space(conditions.len()).div_ceil(8)];
        let bytes: &mut [u8] = bytemuck::cast_slice_mut(&mut region);
        let n = write_block(bytes, &conditions).unwrap();
        assert_eq!(n, block_space(3));

        let (header, parsed) = read_block(bytes, 0).unwrap();
        assert_eq!(header.num_conditions, 3);
        assert_eq!(parsed, &conditions[..]);

        // In-place wake update through the mut view.
        let parsed_mut = read_block_mut(bytes, 0).unwrap();
        parsed_mut[0].wake_ts = 42;
        let (_, parsed) = read_block(bytes, 0).unwrap();
        assert_eq!(parsed[0].wake(), Ok(WakeView::AtTimestamp { unix_ts: 42 }));
    }

    #[test]
    fn unaligned_reader_matches() {
        let conditions = sample_conditions();
        let mut region = vec![0u8; block_space(conditions.len()) + 1];
        write_block(&mut region[1..], &conditions).unwrap();
        // Offset 1: hopelessly misaligned, still reads via the copying path.
        let parsed = read_conditions_unaligned(&region, 1).unwrap();
        assert_eq!(parsed, conditions);
    }

    #[test]
    fn wake_views() {
        let conditions = sample_conditions();
        assert_eq!(
            conditions[0].wake(),
            Ok(WakeView::AtTimestamp { unix_ts: i64::MAX })
        );
        assert_eq!(
            conditions[1].wake(),
            Ok(WakeView::OnAccountChange {
                address: [3; 32],
                offset: 48,
                len: 4
            })
        );
        assert_eq!(
            conditions[2].wake(),
            Ok(WakeView::EverySlots { slots: 300 })
        );
        assert_eq!(
            ConditionV0::at_slot(500, spec(0), ResolverListV0::new(0, 0)).wake(),
            Ok(WakeView::AtSlot { slot: 500 })
        );
        let mut bad = conditions[0];
        bad.wake_kind = 9;
        assert_eq!(bad.wake(), Err(SpecError::BadWakeKind));
    }

    /// The wire fields are sealed; a wake is written by variant, and
    /// writing one clears what the previous variant used. `wake_ts` is a
    /// timestamp for one kind and a threshold for another — that overload
    /// is exactly what a caller must not have to remember.
    #[test]
    fn set_wake_replaces_the_variant_wholesale() {
        let mut c = ConditionV0::on_value_cross(
            [7; 32],
            8,
            8,
            WatchValue::Signed(1234),
            1,
            spec(0),
            ResolverListV0::new(64, 1),
        );
        assert_eq!(
            c.wake(),
            Ok(WakeView::OnValueCross {
                address: [7; 32],
                offset: 8,
                len: 8,
                threshold: WatchValue::Signed(1234),
                cmp: 1,
            })
        );
        c.set_wake(WakeView::EverySlots { slots: 300 });
        assert_eq!(c.wake(), Ok(WakeView::EverySlots { slots: 300 }));
        // The threshold did not survive as a stale timestamp.
        c.set_wake(WakeView::AtTimestamp { unix_ts: 5 });
        assert_eq!(c.wake(), Ok(WakeView::AtTimestamp { unix_ts: 5 }));
    }

    /// The one-field host: byte-exact size, no padding for the derives to
    /// choke on, and a self-described resolver list a turner can follow
    /// from the raw account bytes.
    #[test]
    fn relay_block_hosts_conditions_and_resolvers_in_one_field() {
        type Block = RelayBlockV0<3, 8>;
        assert_eq!(core::mem::size_of::<Block>(), Block::SIZE);
        assert_eq!(core::mem::align_of::<Block>(), 1);
        assert_eq!(Block::SIZE % 8, 0);

        // A host account: discriminator, a field, then the block.
        const BLOCK_OFFSET: usize = 8 + 16;
        let mut block = Block::zeroed();
        // Unstamped blocks refuse to self-describe.
        assert!(block
            .write_resolvers(&[AccountRefV0::writable([1; 32])])
            .is_err());
        block.init(BLOCK_OFFSET as u32).unwrap();
        assert_eq!(block.account_offset(), BLOCK_OFFSET as u32);

        let refs = [
            AccountRefV0::writable([1; 32]),
            AccountRefV0::readonly([2; 32]),
        ];
        let list = block.write_resolvers(&refs).unwrap();
        assert_eq!(list.count, 2);
        assert_eq!(
            list.offset as usize,
            BLOCK_OFFSET + BLOCK_HEADER_LEN + 3 * CONDITION_LEN
        );
        assert_eq!(block.resolver_refs(), refs.to_vec());

        block
            .write_condition(
                0,
                &ConditionV0::at_timestamp(
                    42,
                    CrankSpecV0 {
                        resolver_program: [1; 32],
                        resolver_disc: [2; 8],
                        executor_program: [3; 32],
                        executor_disc: [4; 8],
                        min_payment: 5,
                    },
                    list,
                ),
            )
            .unwrap();

        // Turner-shaped read: raw account bytes, block at its offset, the
        // resolver list followed from the condition's own pointer.
        let mut account = alloc::vec![0u8; BLOCK_OFFSET + Block::SIZE];
        account[BLOCK_OFFSET..BLOCK_OFFSET + Block::SIZE]
            .copy_from_slice(bytemuck::bytes_of(&block));
        let conditions = read_conditions_unaligned(&account, BLOCK_OFFSET).unwrap();
        assert_eq!(conditions.len(), 3);
        assert_eq!(
            conditions[0].wake(),
            Ok(WakeView::AtTimestamp { unix_ts: 42 })
        );
        let resolvers = conditions[0].resolvers();
        let start = resolvers.offset as usize;
        let first: AccountRefV0 =
            bytemuck::pod_read_unaligned(&account[start..start + ACCOUNT_REF_LEN]);
        assert_eq!(first, refs[0]);
    }

    /// An unsigned watched field with its top bit set must not read as
    /// negative. A u64 amount past `i64::MAX` sign-extended flips every
    /// comparison: `value >= threshold` reports not-due exactly when the
    /// value is at its largest.
    #[test]
    fn unsigned_watches_compare_in_the_unsigned_domain() {
        let big: u64 = i64::MAX as u64 + 5; // top bit set
        let bytes = big.to_le_bytes();

        // Sign-extended (the old reading) this is negative and >= fails.
        let signed = read_watched_value(&bytes, false).unwrap();
        assert!(!value_crossed(signed, WatchValue::Signed(100), 0));

        // Declared unsigned it compares correctly, in both directions.
        let unsigned = read_watched_value(&bytes, true).unwrap();
        assert_eq!(unsigned, WatchValue::Unsigned(big));
        assert!(value_crossed(unsigned, WatchValue::Unsigned(100), 0));
        assert!(!value_crossed(unsigned, WatchValue::Unsigned(u64::MAX), 0));
        assert!(value_crossed(unsigned, WatchValue::Unsigned(u64::MAX), 1));

        // And the condition round-trips the declaration through the wire.
        let c = ConditionV0::on_value_cross(
            [7; 32],
            8,
            8,
            WatchValue::Unsigned(big),
            0,
            spec(0),
            ResolverListV0::new(64, 1),
        );
        let Ok(WakeView::OnValueCross { threshold, .. }) = c.wake() else {
            panic!("wrong wake kind");
        };
        assert_eq!(threshold, WatchValue::Unsigned(big));

        // Narrow unsigned widths zero-extend.
        assert_eq!(
            read_watched_value(&0xFFu8.to_le_bytes(), true),
            Some(WatchValue::Unsigned(255))
        );
        assert_eq!(
            read_watched_value(&0xFFFFu16.to_le_bytes(), true),
            Some(WatchValue::Unsigned(65_535))
        );
    }

    /// Rewriting an unsigned value-cross wake to any other variant clears
    /// the signedness flag with the rest of the wake fields.
    #[test]
    fn set_wake_clears_the_signedness_flag() {
        let mut c = ConditionV0::on_value_cross(
            [7; 32],
            8,
            8,
            WatchValue::Unsigned(9),
            0,
            spec(0),
            ResolverListV0::new(64, 1),
        );
        assert_eq!(c.wake_value_unsigned, 1);
        c.set_wake(WakeView::OnValueCross {
            address: [7; 32],
            offset: 8,
            len: 8,
            threshold: WatchValue::Signed(-3),
            cmp: 0,
        });
        assert_eq!(c.wake_value_unsigned, 0);
        assert_eq!(
            c.wake().unwrap(),
            WakeView::OnValueCross {
                address: [7; 32],
                offset: 8,
                len: 8,
                threshold: WatchValue::Signed(-3),
                cmp: 0,
            }
        );
    }

    #[test]
    fn resolver_list_is_always_indirect() {
        let c = ConditionV0::at_timestamp(0, spec(0), ResolverListV0::new(96, 7));
        assert_eq!(c.resolver_list_offset, 96);
        assert_eq!(c.num_resolver_accounts, 7);
        // Reserved space stays zero so a future field can claim it.
        assert_eq!(c._reserved, [0u8; 39]);
    }

    #[test]
    fn bad_magic_and_version_rejected() {
        let conditions = sample_conditions();
        let mut region = vec![0u8; block_space(conditions.len())];
        write_block(&mut region, &conditions).unwrap();

        let mut tampered = region.clone();
        tampered[0] ^= 0xFF;
        assert_eq!(
            read_conditions_unaligned(&tampered, 0),
            Err(SpecError::BadMagic)
        );

        let mut tampered = region;
        tampered[8] = SPEC_VERSION + 1;
        assert_eq!(
            read_conditions_unaligned(&tampered, 0),
            Err(SpecError::UnsupportedVersion)
        );
    }

    #[test]
    fn truncation_rejected() {
        let conditions = sample_conditions();
        let mut region = vec![0u8; block_space(conditions.len())];
        write_block(&mut region, &conditions).unwrap();
        (0..region.len()).for_each(|n| {
            assert!(
                read_conditions_unaligned(&region[..n], 0).is_err(),
                "cut {n} should fail"
            );
        });
    }

    #[test]
    fn pointer_round_trip() {
        let pointer = ResponsePointerV0::new(1, 640, 77);
        let parsed = ResponsePointerV0::read(&pointer.to_bytes()).unwrap();
        assert_eq!(parsed, pointer);
        assert!(parsed.has_work());
        assert_eq!(parsed.account_index, 1);
        assert_eq!(parsed.offset(), 640);
        assert_eq!(parsed.len(), 77);

        let none = ResponsePointerV0::no_work();
        assert!(!ResponsePointerV0::read(&none.to_bytes())
            .unwrap()
            .has_work());
    }

    #[test]
    fn pointer_tolerates_stripped_trailing_zeros() {
        // RPC return data drops trailing zero bytes; a no-work pointer is
        // all zeros and can arrive as a single byte (or the whole thing
        // stripped down to one).
        assert!(!ResponsePointerV0::read(&[0]).unwrap().has_work());
        let pointer = ResponsePointerV0::new(0, 16, 3);
        let full = pointer.to_bytes();
        let trimmed = &full[..full.iter().rposition(|b| *b != 0).unwrap() + 1];
        assert_eq!(ResponsePointerV0::read(trimmed).unwrap(), pointer);
        assert_eq!(ResponsePointerV0::read(&[]), Err(SpecError::Truncated));
    }

    #[test]
    fn staged_payload_round_trip() {
        let resolved = ResolvedCrankV0 {
            accounts: vec![
                AccountRefV0::writable(KEEPER_PLACEHOLDER),
                AccountRefV0::writable([5; 32]),
            ],
            data: vec![1, 2, 3, 4],
        };
        let bytes = resolved.to_bytes();
        assert_eq!(bytes.len(), resolved.encoded_len());
        assert_eq!(ResolvedCrankV0::read(&bytes).unwrap(), resolved);
    }

    #[test]
    fn staged_payload_write_into_region() {
        let resolved = ResolvedCrankV0 {
            accounts: vec![AccountRefV0::writable(KEEPER_PLACEHOLDER)],
            data: vec![7; 10],
        };
        // A region much larger than the payload: only the used prefix is
        // meaningful, and the pointer's `len` says how much.
        let mut region = [0xAAu8; 512];
        let n = resolved.write_into(&mut region).unwrap();
        assert_eq!(ResolvedCrankV0::read(&region[..n]).unwrap(), resolved);
        assert_eq!(region[n], 0xAA, "write must not clobber past len");

        let mut tiny = [0u8; 8];
        assert_eq!(resolved.write_into(&mut tiny), Err(SpecError::TooLarge));
    }

    /// The size argument for staging: a batch payload that would blow the
    /// 1024-byte return-data cap stages fine.
    #[test]
    fn staged_payload_exceeds_return_data_cap() {
        let resolved = ResolvedCrankV0 {
            accounts: (0..40u8).map(|i| AccountRefV0::writable([i; 32])).collect(),
            data: vec![9; 400],
        };
        assert!(resolved.encoded_len() > 1024);
        let mut region = vec![0u8; 4096];
        let n = resolved.write_into(&mut region).unwrap();
        assert_eq!(ResolvedCrankV0::read(&region[..n]).unwrap(), resolved);
        // ...while the thing that actually rides return data stays tiny.
        assert_eq!(
            ResponsePointerV0::new(0, 0, n as u32).to_bytes().len(),
            RESPONSE_POINTER_LEN
        );
    }

    #[test]
    fn watch_account_parse() {
        let data: Vec<u8> = WATCH_V0_DISCRIMINATOR
            .into_iter()
            .chain([6; 32]) // target_program
            .chain([8; 32]) // target
            .chain([7; 32]) // registrar
            .chain(123u32.to_le_bytes())
            .chain([0; 4])
            .collect();
        assert_eq!(data.len(), WATCH_V0_LEN);
        let w = WatchV0::read_from_account(&data).unwrap();
        assert_eq!(w.target_program, [6; 32]);
        assert_eq!(w.target, [8; 32]);
        assert_eq!(w.registrar, [7; 32]);
        assert_eq!(w.offset, 123);

        // The memcmp offsets turners filter on must address those fields.
        assert_eq!(&data[WATCH_TARGET_PROGRAM_OFFSET..][..32], &[6; 32]);
        assert_eq!(&data[WATCH_TARGET_OFFSET..][..32], &[8; 32]);
        assert_eq!(&data[WATCH_REGISTRAR_OFFSET..][..32], &[7; 32]);

        let mut bad = data;
        bad[0] ^= 1;
        assert_eq!(
            WatchV0::read_from_account(&bad),
            Err(SpecError::BadDiscriminator)
        );
    }

    #[test]
    fn guard_data_layout() {
        let begin = encode_begin_guard_v0_data(3);
        assert_eq!(&begin[..8], &BEGIN_GUARD_V0_DISCRIMINATOR);
        assert_eq!(begin[8], 3);

        let assert_paid = encode_assert_paid_v0_data(50_000, 3);
        assert_eq!(&assert_paid[..8], &ASSERT_PAID_V0_DISCRIMINATOR);
        assert_eq!(&assert_paid[8..16], &50_000u64.to_le_bytes());
        assert_eq!(assert_paid[16], 3);
    }
}

#[cfg(test)]
mod condition_block_tests {
    use super::*;

    /// A minimal host: header+conditions region, then staging.
    struct Host {
        block: [u8; BLOCK_HEADER_LEN + 3 * CONDITION_LEN],
        staging: [u8; 512],
    }

    const HOST_STAGING_OFFSET: u32 = (8 + BLOCK_HEADER_LEN + 3 * CONDITION_LEN) as u32;

    impl ConditionBlock for Host {
        const NUM_CONDITIONS: usize = 3;
        fn block(&self) -> &[u8] {
            &self.block
        }
        fn block_mut(&mut self) -> &mut [u8] {
            &mut self.block
        }
    }

    fn host() -> Host {
        Host {
            block: [0; BLOCK_HEADER_LEN + 3 * CONDITION_LEN],
            staging: [0; 512],
        }
    }

    fn spec() -> CrankSpecV0 {
        CrankSpecV0 {
            resolver_program: [1; 32],
            resolver_disc: [2; 8],
            executor_program: [1; 32],
            executor_disc: [3; 8],
            min_payment: 7,
        }
    }

    /// A block already at the current version reports no change.
    #[test]
    fn migrate_is_a_no_op_on_a_current_block() {
        let mut host = host();
        host.init_header().unwrap();
        assert_eq!(host.migrate(), Ok(false));
    }

    #[test]
    fn provided_methods_round_trip_through_the_readers() {
        let mut host = host();
        host.init_header().unwrap();
        host.write_condition(
            1,
            &ConditionV0::every_slots(42, spec(), ResolverListV0::new(0, 0)),
        )
        .unwrap();

        // The canonical reader accepts what the trait wrote.
        let (header, conditions) = read_block(host.block(), 0).unwrap();
        assert_eq!(header.num_conditions, 3);
        assert_eq!(conditions[1].wake_slot, 42);
        assert_eq!(conditions[0].active, 0);
        assert_eq!(host.read_condition(1).unwrap().wake_slot, 42);

        // In-place wake updates (the min-fold/repair pattern).
        host.update_condition(1, |c| c.wake_slot = 9).unwrap();
        assert_eq!(host.read_condition(1).unwrap().wake_slot, 9);

        // Deactivation is what makes a level-triggered wake go quiet.
        host.write_condition(
            2,
            &ConditionV0::on_value_cross(
                [5; 32],
                8,
                8,
                WatchValue::Signed(100),
                0,
                spec(),
                ResolverListV0::new(0, 0),
            ),
        )
        .unwrap();
        assert_eq!(host.read_condition(2).unwrap().active, 1);
        host.deactivate_condition(2).unwrap();
        assert_eq!(host.read_condition(2).unwrap().active, 0);

        // Out-of-range slots are rejected, not silently wrapped.
        assert!(host
            .write_condition(
                3,
                &ConditionV0::every_slots(1, spec(), ResolverListV0::new(0, 0))
            )
            .is_err());
        assert!(host.read_condition(3).is_err());
    }

    #[test]
    fn staging_returns_a_pointer_the_turner_can_follow() {
        let mut host = host();
        let resolved = ResolvedCrankV0 {
            accounts: vec![AccountRefV0::writable([9; 32])],
            data: vec![1, 2, 3],
        };
        let pointer_bytes =
            stage_into(&mut host.staging, 0, HOST_STAGING_OFFSET, &resolved).unwrap();
        let pointer = ResponsePointerV0::read(&pointer_bytes).unwrap();
        assert!(pointer.has_work());
        assert_eq!(pointer.offset(), HOST_STAGING_OFFSET);
        assert_eq!(
            ResolvedCrankV0::read(&host.staging[..pointer.len() as usize]).unwrap(),
            resolved
        );
    }
}
