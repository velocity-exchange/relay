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
//! post-execution account state, then submits the **executor** (usually
//! wrapped in relay's `crank_v0`, which asserts the keeper got paid
//! `min_payment`).
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

/// Fixed slots for a resolver's account list. A resolver reads the
/// condition account and stages its payload there (or in a sibling
/// scratch account) — its job is to *output* the executor's accounts, not
/// to take many itself.
pub const MAX_RESOLVER_ACCOUNTS: usize = 4;

/// Anchor instruction discriminator of relay's `crank_v0`
/// (`sha256("global:crank_v0")[..8]`). Pinned here so turners don't need a
/// hash dependency; the program's test suite asserts it matches the
/// generated constant.
pub const CRANK_V0_DISCRIMINATOR: [u8; 8] = [176, 209, 83, 95, 203, 163, 210, 99];

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
    pub min_payment: u64,
    /// [`WakeKind::AtTimestamp`] input.
    pub wake_ts: i64,
    /// [`WakeKind::EverySlots`] input (interval) or [`WakeKind::AtSlot`]
    /// input (absolute slot) — selected by `wake_kind`.
    pub wake_slot: u64,
    /// [`WakeKind::OnAccountChange`] inputs.
    pub wake_account: [u8; 32],
    pub wake_offset: u32,
    pub wake_len: u32,
    /// Instruction the turner simulates to discover work. Stages its
    /// payload in one of `resolver_accounts` and returns a
    /// [`ResponsePointerV0`].
    pub resolver_program: [u8; 32],
    pub resolver_disc: [u8; 8],
    /// Instruction that does the work and pays the keeper. Its account list
    /// and trailing args come from the resolver's staged payload.
    pub executor_program: [u8; 32],
    pub executor_disc: [u8; 8],
    /// The resolver's own (fixed) account list; first
    /// `num_resolver_accounts` entries are live. The staging account must
    /// be among them and marked writable.
    pub resolver_accounts: [AccountRefV0; MAX_RESOLVER_ACCOUNTS],
    pub num_resolver_accounts: u8,
    /// A [`WakeKind`] value.
    pub wake_kind: u8,
    /// 0 = inactive (skipped by turners, rejected by `crank_v0`).
    pub active: u8,
    pub _pad: [u8; 1],
}

pub const CONDITION_LEN: usize = core::mem::size_of::<ConditionV0>();
const _: () = assert!(CONDITION_LEN == 280);
const _: () = assert!(core::mem::align_of::<ConditionV0>() == 8);

impl ConditionV0 {
    fn base(spec: CrankSpecV0, resolver_accounts: &[AccountRefV0]) -> Self {
        let mut fixed = [AccountRefV0::zeroed(); MAX_RESOLVER_ACCOUNTS];
        fixed[..resolver_accounts.len()].copy_from_slice(resolver_accounts);
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
            resolver_accounts: fixed,
            num_resolver_accounts: resolver_accounts.len() as u8,
            wake_kind: 0,
            active: 1,
            _pad: [0],
        }
    }

    pub fn at_timestamp(
        unix_ts: i64,
        spec: CrankSpecV0,
        resolver_accounts: &[AccountRefV0],
    ) -> Self {
        let mut c = Self::base(spec, resolver_accounts);
        c.wake_kind = WakeKind::AtTimestamp as u8;
        c.wake_ts = unix_ts;
        c
    }

    pub fn on_account_change(
        watched: [u8; 32],
        offset: u32,
        len: u32,
        spec: CrankSpecV0,
        resolver_accounts: &[AccountRefV0],
    ) -> Self {
        let mut c = Self::base(spec, resolver_accounts);
        c.wake_kind = WakeKind::OnAccountChange as u8;
        c.wake_account = watched;
        c.wake_offset = offset;
        c.wake_len = len;
        c
    }

    pub fn every_slots(slots: u64, spec: CrankSpecV0, resolver_accounts: &[AccountRefV0]) -> Self {
        let mut c = Self::base(spec, resolver_accounts);
        c.wake_kind = WakeKind::EverySlots as u8;
        c.wake_slot = slots;
        c
    }

    pub fn at_slot(slot: u64, spec: CrankSpecV0, resolver_accounts: &[AccountRefV0]) -> Self {
        let mut c = Self::base(spec, resolver_accounts);
        c.wake_kind = WakeKind::AtSlot as u8;
        c.wake_slot = slot;
        c
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
            _ => Err(SpecError::BadWakeKind),
        }
    }

    pub fn resolver_accounts(&self) -> &[AccountRefV0] {
        &self.resolver_accounts[..(self.num_resolver_accounts as usize).min(MAX_RESOLVER_ACCOUNTS)]
    }
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

/// Encode `crank_v0`'s instruction data:
/// `[CRANK_V0_DISCRIMINATOR][offset: u32][condition_index: u8][keeper_index: u8][data: Vec<u8>]`
/// — the borsh wire of the program's `CrankArgsV0`. `keeper_index` is the
/// position of the keeper (payment recipient) within the executor account
/// list — i.e. where [`KEEPER_PLACEHOLDER`] sat in the staged payload.
pub fn encode_crank_v0_data(
    offset: u32,
    condition_index: u8,
    keeper_index: u8,
    data: &[u8],
) -> Vec<u8> {
    let mut out = Vec::with_capacity(8 + 4 + 2 + 4 + data.len());
    out.extend_from_slice(&CRANK_V0_DISCRIMINATOR);
    out.extend_from_slice(&offset.to_le_bytes());
    out.push(condition_index);
    out.push(keeper_index);
    out.extend_from_slice(&(data.len() as u32).to_le_bytes());
    out.extend_from_slice(data);
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
            ConditionV0::at_timestamp(i64::MAX, spec(5000), &[AccountRefV0::writable([2; 32])]),
            ConditionV0::on_account_change(
                [3; 32],
                48,
                4,
                spec(1),
                &[AccountRefV0::writable([2; 32])],
            ),
            ConditionV0::every_slots(300, spec(0), &[]),
        ]
    }

    #[test]
    fn layout_is_pinned() {
        assert_eq!(CONDITION_LEN, 280);
        assert_eq!(BLOCK_HEADER_LEN, 16);
        assert_eq!(ACCOUNT_REF_LEN, 33);
        assert_eq!(RESPONSE_POINTER_LEN, 10);
        assert_eq!(block_space(2), 576);
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
            ConditionV0::at_slot(500, spec(0), &[]).wake(),
            Ok(WakeView::AtSlot { slot: 500 })
        );
        let mut bad = conditions[0];
        bad.wake_kind = 9;
        assert_eq!(bad.wake(), Err(SpecError::BadWakeKind));
    }

    #[test]
    fn resolver_accounts_view() {
        let c = ConditionV0::at_timestamp(
            0,
            spec(0),
            &[
                AccountRefV0::writable([2; 32]),
                AccountRefV0::readonly([3; 32]),
            ],
        );
        assert_eq!(
            c.resolver_accounts(),
            &[
                AccountRefV0::writable([2; 32]),
                AccountRefV0::readonly([3; 32]),
            ]
        );
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
    fn crank_data_layout() {
        let data = encode_crank_v0_data(640, 2, 0, &[0xAB, 0xCD]);
        assert_eq!(&data[..8], &CRANK_V0_DISCRIMINATOR);
        assert_eq!(&data[8..12], &640u32.to_le_bytes());
        assert_eq!(data[12], 2);
        assert_eq!(data[13], 0);
        assert_eq!(&data[14..18], &2u32.to_le_bytes());
        assert_eq!(&data[18..], &[0xAB, 0xCD]);
    }
}
