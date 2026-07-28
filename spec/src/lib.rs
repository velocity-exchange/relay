//! Wire format for relay condition blocks.
//!
//! A target program embeds a condition block — [`ConditionBlockHeaderV0`]
//! followed by a fixed array of [`ConditionV0`] — at a fixed, **8-aligned**
//! offset in one of its accounts, and registers `(account, offset)` with the
//! relay program as a `WatchV0`. A crank turner finds watches, reads the
//! conditions, and for each due condition simulates the **resolver**
//! instruction; the resolver returns a resolved-crank payload
//! ([`ResolvedCrankV0`]) in return data naming the **executor**'s account
//! list and args. The turner then submits the executor (usually wrapped in
//! relay's `crank_v0`, which asserts the keeper got paid `min_payment`).
//!
//! Everything on the evaluation path is **zero-copy pod**: fixed-size
//! `#[repr(C)]` structs with natural alignment and no interior padding, so
//! programs read conditions in place (`bytemuck::from_bytes`) and update a
//! wake with a single field store — no serialization pass, no heap. The
//! resolver return payload is align-1 (parses from any buffer).
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

/// Sentinel address in a resolver's returned account list that the turner
/// replaces with its keeper (payment recipient) before submitting.
pub const KEEPER_PLACEHOLDER: [u8; 32] = *b"relay/keeper/placeholder\0\0\0\0\0\0\0\0";

/// Fixed slots for a resolver's account list. Resolvers read the condition
/// account (and little else) — their job is to *output* the executor's
/// accounts, not to take many themselves.
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
/// `[disc: 8][registrar: 32][target: 32][offset: u32][_pad: 4]`.
pub const WATCH_V0_LEN: usize = 80;

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
    /// A count field exceeds its fixed capacity.
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
    OnAccountDirty = 1,
    /// Due every `wake_slots` slots — the fallback / pure-poll hint.
    EverySlots = 2,
}

/// Copied, alloc-free view of a condition's wake for evaluation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WakeView {
    AtTimestamp {
        unix_ts: i64,
    },
    OnAccountDirty {
        address: [u8; 32],
        offset: u32,
        len: u32,
    },
    EverySlots {
        slots: u64,
    },
}

/// Everything about a condition except its wake — the arguments shared by
/// all three constructors.
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
    /// [`WakeKind::EverySlots`] input.
    pub wake_slots: u64,
    /// [`WakeKind::OnAccountDirty`] inputs.
    pub wake_account: [u8; 32],
    pub wake_offset: u32,
    pub wake_len: u32,
    /// Read-only instruction the turner simulates to discover work. Must
    /// write a resolved-crank payload to return data.
    pub resolver_program: [u8; 32],
    pub resolver_disc: [u8; 8],
    /// Instruction that does the work and pays the keeper. Its account list
    /// and trailing args come from the resolver's output.
    pub executor_program: [u8; 32],
    pub executor_disc: [u8; 8],
    /// The resolver's own (fixed) account list; first
    /// `num_resolver_accounts` entries are live.
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
            wake_slots: 0,
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

    pub fn on_account_dirty(
        watched: [u8; 32],
        offset: u32,
        len: u32,
        spec: CrankSpecV0,
        resolver_accounts: &[AccountRefV0],
    ) -> Self {
        let mut c = Self::base(spec, resolver_accounts);
        c.wake_kind = WakeKind::OnAccountDirty as u8;
        c.wake_account = watched;
        c.wake_offset = offset;
        c.wake_len = len;
        c
    }

    pub fn every_slots(slots: u64, spec: CrankSpecV0, resolver_accounts: &[AccountRefV0]) -> Self {
        let mut c = Self::base(spec, resolver_accounts);
        c.wake_kind = WakeKind::EverySlots as u8;
        c.wake_slots = slots;
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
            1 => Ok(WakeView::OnAccountDirty {
                address: self.wake_account,
                offset: self.wake_offset,
                len: self.wake_len,
            }),
            2 => Ok(WakeView::EverySlots {
                slots: self.wake_slots,
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

/// What a resolver writes to return data, decoded. The wire is align-1
/// (parses from any buffer):
/// `[work: u8][num_accounts: u8][data_len: u16 LE][AccountRefV0; num][data bytes]`
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ResolvedCrankV0 {
    /// False = nothing to do right now; the turner records the wake as
    /// evaluated and moves on. No accounts/data expected.
    pub work: bool,
    /// Full executor account list, in order. [`KEEPER_PLACEHOLDER`] entries
    /// are replaced with the turner's keeper.
    pub accounts: Vec<AccountRefV0>,
    /// Executor args after the 8-byte discriminator.
    pub data: Vec<u8>,
}

pub const RESOLVED_HEADER_LEN: usize = 4;

impl ResolvedCrankV0 {
    pub fn no_work() -> Self {
        Self::default()
    }

    pub fn read(bytes: &[u8]) -> Result<Self, SpecError> {
        if bytes.len() < RESOLVED_HEADER_LEN {
            return Err(SpecError::Truncated);
        }
        let work = bytes[0] != 0;
        let n = bytes[1] as usize;
        let data_len = u16::from_le_bytes([bytes[2], bytes[3]]) as usize;
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
            work,
            accounts,
            data: bytes[accounts_end..end].to_vec(),
        })
    }

    /// Serialize into a caller-provided buffer (no alloc — resolvers write
    /// into a stack array and `set_return_data` the used prefix). Returns
    /// the used length.
    pub fn write_into(&self, buf: &mut [u8]) -> Result<usize, SpecError> {
        if self.accounts.len() > u8::MAX as usize || self.data.len() > u16::MAX as usize {
            return Err(SpecError::TooLarge);
        }
        let accounts_end = RESOLVED_HEADER_LEN + self.accounts.len() * ACCOUNT_REF_LEN;
        let end = accounts_end + self.data.len();
        if end > buf.len() {
            return Err(SpecError::TooLarge);
        }
        buf[0] = self.work as u8;
        buf[1] = self.accounts.len() as u8;
        buf[2..4].copy_from_slice(&(self.data.len() as u16).to_le_bytes());
        self.accounts.iter().enumerate().for_each(|(i, a)| {
            let start = RESOLVED_HEADER_LEN + i * ACCOUNT_REF_LEN;
            buf[start..start + ACCOUNT_REF_LEN].copy_from_slice(bytemuck::bytes_of(a));
        });
        buf[accounts_end..end].copy_from_slice(&self.data);
        Ok(end)
    }

    pub fn to_bytes(&self) -> Vec<u8> {
        let mut buf = alloc::vec![
            0u8;
            RESOLVED_HEADER_LEN + self.accounts.len() * ACCOUNT_REF_LEN + self.data.len()
        ];
        let n = self.write_into(&mut buf).expect("sized to fit");
        buf.truncate(n);
        buf
    }
}

// --- registry ---

/// A parsed relay `WatchV0` registry account.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WatchV0 {
    pub registrar: [u8; 32],
    pub target: [u8; 32],
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
        Ok(Self {
            registrar: data[8..40].try_into().unwrap(),
            target: data[40..72].try_into().unwrap(),
            offset: u32::from_le_bytes(data[72..76].try_into().unwrap()),
        })
    }
}

/// Encode `crank_v0`'s instruction data:
/// `[CRANK_V0_DISCRIMINATOR][offset: u32][condition_index: u8][keeper_index: u8][data: Vec<u8>]`
/// — the borsh wire of the program's `CrankArgsV0`. `keeper_index` is the
/// position of the keeper (payment recipient) within the executor account
/// list — i.e. where [`KEEPER_PLACEHOLDER`] sat in the resolver's output.
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
            ConditionV0::at_timestamp(i64::MAX, spec(5000), &[AccountRefV0::readonly([2; 32])]),
            ConditionV0::on_account_dirty(
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
            Ok(WakeView::OnAccountDirty {
                address: [3; 32],
                offset: 48,
                len: 4
            })
        );
        assert_eq!(
            conditions[2].wake(),
            Ok(WakeView::EverySlots { slots: 300 })
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
                AccountRefV0::readonly([2; 32]),
                AccountRefV0::writable([3; 32]),
            ],
        );
        assert_eq!(
            c.resolver_accounts(),
            &[
                AccountRefV0::readonly([2; 32]),
                AccountRefV0::writable([3; 32]),
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
    fn resolved_round_trip() {
        let resolved = ResolvedCrankV0 {
            work: true,
            accounts: vec![
                AccountRefV0::writable(KEEPER_PLACEHOLDER),
                AccountRefV0::writable([5; 32]),
            ],
            data: vec![1, 2, 3, 4],
        };
        assert_eq!(
            ResolvedCrankV0::read(&resolved.to_bytes()).unwrap(),
            resolved
        );

        let none = ResolvedCrankV0::no_work();
        let bytes = none.to_bytes();
        assert_eq!(bytes.len(), RESOLVED_HEADER_LEN);
        assert_eq!(ResolvedCrankV0::read(&bytes).unwrap(), none);
    }

    #[test]
    fn resolved_write_into_stack_buffer() {
        let resolved = ResolvedCrankV0 {
            work: true,
            accounts: vec![AccountRefV0::writable(KEEPER_PLACEHOLDER)],
            data: vec![7; 10],
        };
        let mut buf = [0u8; 128];
        let n = resolved.write_into(&mut buf).unwrap();
        assert_eq!(ResolvedCrankV0::read(&buf[..n]).unwrap(), resolved);

        let mut tiny = [0u8; 8];
        assert_eq!(resolved.write_into(&mut tiny), Err(SpecError::TooLarge));
    }

    #[test]
    fn watch_account_parse() {
        let data: Vec<u8> = WATCH_V0_DISCRIMINATOR
            .into_iter()
            .chain([7; 32])
            .chain([8; 32])
            .chain(123u32.to_le_bytes())
            .chain([0; 4])
            .collect();
        assert_eq!(data.len(), WATCH_V0_LEN);
        let w = WatchV0::read_from_account(&data).unwrap();
        assert_eq!(w.registrar, [7; 32]);
        assert_eq!(w.target, [8; 32]);
        assert_eq!(w.offset, 123);

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
