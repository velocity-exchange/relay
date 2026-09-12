//! The host side of a condition block: the surface a program account
//! implements, and the one field that implements all of it.

use bytemuck::{Pod, Zeroable};

use crate::account_ref::{AccountRefV0, ACCOUNT_REF_LEN};
use crate::block::{ConditionBlockHeaderV0, BLOCK_HEADER_LEN};
use crate::condition::{ConditionV0, CONDITION_LEN};
use crate::resolver::{ResolverListV0, MAX_INDIRECT_RESOLVER_ACCOUNTS};
use crate::{SpecError, MAGIC, SPEC_VERSION};

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
/// [`crate::read_block`]).
///
/// Staging is deliberately not part of this. A host does not have to
/// carry scratch of its own — see [`crate::stage_into`] — so where a resolved
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

    /// Borrow one condition slot in place. Alignment 1 means this is a cast
    /// at any offset, so nothing is copied.
    fn condition(&self, index: usize) -> Result<&ConditionV0, SpecError> {
        let start = Self::slot_start(index)?;
        let block = self.block();
        let slot = block
            .get(start..start + CONDITION_LEN)
            .ok_or(SpecError::Truncated)?;
        bytemuck::try_from_bytes(slot).map_err(|_| SpecError::Misaligned)
    }

    /// Borrow one condition slot mutably, for wake updates in place.
    fn condition_mut(&mut self, index: usize) -> Result<&mut ConditionV0, SpecError> {
        let start = Self::slot_start(index)?;
        let block = self.block_mut();
        let slot = block
            .get_mut(start..start + CONDITION_LEN)
            .ok_or(SpecError::Truncated)?;
        bytemuck::try_from_bytes_mut(slot).map_err(|_| SpecError::Misaligned)
    }

    /// Read one condition slot back by value.
    fn read_condition(&self, index: usize) -> Result<ConditionV0, SpecError> {
        self.condition(index).copied()
    }

    /// Zero a slot — the way work that is done goes quiet. A
    /// level-triggered wake (see [`crate::WakeKind::OnValueCross`]) that is left
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
    /// pattern). No read-modify-write pass: the closure is handed the slot
    /// itself.
    fn update_condition(
        &mut self,
        index: usize,
        f: impl FnOnce(&mut ConditionV0),
    ) -> Result<(), SpecError> {
        f(self.condition_mut(index)?);
        Ok(())
    }

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
/// Anchor hosts wrap this in `relay_anchor::RelayBlock<C, R>`, which adds
/// the framework coupling (an `IdlBuild` impl) that this crate deliberately
/// cannot: the spec depends on bytemuck and nothing else.
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
/// maps) keep building `ResolverListV0`s from [`crate::block_offset!`] as before
/// — the built-in region is the common case, not a limit.
///
/// **Size the parameters with headroom.** Both are capacities: a zeroed
/// condition slot is inactive (turners skip it, `crank_v0` rejects it) and
/// unused resolver slots are just bytes, so adding a condition to an
/// account with spare slots is a plain `write_condition` — no resize, no
/// migration. Growing a parameter on a *deployed* account is a layout
/// change: the conditions array expands into the resolver region and
/// everything behind it (the region, this field's trailer, every host
/// field after the field) shifts. [`Self::grow_in_place`] does that
/// surgery when headroom runs out, but rent on a spare slot (192 bytes per
/// condition, 33 per resolver ref) is cheaper than ever needing it.
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

/// The CLOB account embeds this block, so the CLOB's IDL build must be able to
/// name it. It is const-generic, which the `IdlType` derive cannot express, and
/// its bytes are an opaque relay layout no IDL consumer decodes through this
/// type. So it is emitted as an empty struct: a valid type the reference
/// resolves against, carrying no fields to get wrong.
#[cfg(feature = "idl-build")]
impl<const C: usize, const R: usize> anchor_lang_v2::IdlAccountType for RelayBlockV0<C, R> {
    const __IDL_TYPE_DEF: Option<&'static str> =
        Some(r#"{"name":"RelayBlockV0","type":{"kind":"struct","fields":[]}}"#);
    fn __register_idl_deps(
        _accounts: &mut anchor_lang_v2::__alloc::vec::Vec<&'static str>,
        types: &mut anchor_lang_v2::__alloc::vec::Vec<&'static str>,
    ) {
        if let Some(t) = <Self as anchor_lang_v2::IdlAccountType>::__IDL_TYPE_DEF {
            types.push(t);
        }
    }
}

// Sound for any parameters: every field is a byte array, so the struct is
// alignment-1 and can contain no padding.
unsafe impl<const C: usize, const R: usize> Zeroable for RelayBlockV0<C, R> {}
unsafe impl<const C: usize, const R: usize> Pod for RelayBlockV0<C, R> {}

/// A summary, not the kilobytes of wire bytes — hosts derive `Debug` on
/// their account structs and this field would otherwise drown the output.
impl<const C: usize, const R: usize> core::fmt::Debug for RelayBlockV0<C, R> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("RelayBlockV0")
            .field("conditions", &C)
            .field("resolver_capacity", &R)
            .field("resolver_count", &self.resolver_count)
            .field("account_offset", &self.account_offset())
            .finish()
    }
}

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
        R.is_multiple_of(8),
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
        // The whole region is rewritten, not just the prefix: a condition
        // still pointing at this list with an older, larger count would
        // otherwise read the tail of the previous write as live refs.
        self.resolvers
            .iter_mut()
            .enumerate()
            .for_each(|(i, slot)| match refs.get(i) {
                Some(r) => {
                    slot[..32].copy_from_slice(&r.address);
                    slot[32] = r.writable;
                }
                None => slot.fill(0),
            });
        self.resolver_count = refs.len() as u8;
        let region = BLOCK_HEADER_LEN + C * CONDITION_LEN;
        Ok(ResolverListV0::new(
            self.account_offset() + region as u32,
            refs.len() as u8,
        ))
    }

    /// The stored resolver list, as last written — borrowed in place, not
    /// rebuilt. [`AccountRefV0`] is alignment-1 pod and the region is a
    /// packed array of them, so the list is already the wire format.
    pub fn resolver_refs(&self) -> &[AccountRefV0] {
        bytemuck::cast_slice(&self.resolvers[..(self.resolver_count as usize).min(R)])
    }

    /// Grow a smaller block into this instantiation, in place, inside an
    /// already-resized account.
    ///
    /// Growing `CONDITIONS` or `RESOLVER_CAPACITY` is a layout change: the
    /// conditions array expands into the resolver region, and the region,
    /// the trailer, and every host field behind this one all shift. This
    /// does the whole move in one pass over the raw account bytes —
    /// prefer sizing the parameters with headroom so it never runs.
    ///
    /// The caller reallocates the account by `Self::SIZE - old size`
    /// first (the extra bytes land at the end; on-chain `realloc` zeroes
    /// them) and redeclares the host struct with the new parameters; this
    /// then shifts the old bytes into their new homes, zeroes the new
    /// slots, restamps the header, and rewrites every condition's
    /// `resolver_list_offset`: pointers into the built-in region are
    /// rebased onto its new position, pointers past the old block end
    /// (host-side regions) shift with their bytes, pointers before the
    /// block are untouched.
    ///
    /// Two things stay the host's job, because the spec cannot know them:
    /// any *hardcoded* offsets in host code pointing behind the block, and
    /// any condition whose `wake_account` is this same account watching
    /// bytes behind the block (the spec does not know the account's own
    /// address). Registered watches keep working — they point at the block
    /// itself, whose offset does not move.
    pub fn grow_in_place(
        data: &mut [u8],
        block_offset: usize,
        old_conditions: usize,
        old_capacity: usize,
    ) -> Result<(), SpecError> {
        #[allow(clippy::let_unit_value)]
        let _ = Self::LAYOUT;
        if old_conditions > C || old_capacity > R || !old_capacity.is_multiple_of(8) {
            return Err(SpecError::TooLarge);
        }
        let old_size =
            BLOCK_HEADER_LEN + old_conditions * CONDITION_LEN + old_capacity * ACCOUNT_REF_LEN + 16;
        let delta = Self::SIZE - old_size;
        let old_end = block_offset
            .checked_add(old_size)
            .ok_or(SpecError::Truncated)?;
        // The caller must have grown the account by exactly `delta`; the
        // old bytes therefore end `delta` short of the new length.
        if data.len() < old_end + delta {
            return Err(SpecError::Truncated);
        }
        if delta == 0 {
            return Ok(());
        }

        let old_region = block_offset + BLOCK_HEADER_LEN + old_conditions * CONDITION_LEN;
        let new_region = block_offset + BLOCK_HEADER_LEN + C * CONDITION_LEN;
        let old_trailer = old_region + old_capacity * ACCOUNT_REF_LEN;
        let new_trailer = new_region + R * ACCOUNT_REF_LEN;

        // Right-to-left, so nothing is read after its bytes are overwritten:
        // the host's tail, then the trailer, then the resolver slots — each
        // target sits at or past the previous move's source.
        data.copy_within(old_end..data.len() - delta, old_end + delta);
        data.copy_within(old_trailer..old_trailer + 16, new_trailer);
        data.copy_within(
            old_region..old_region + old_capacity * ACCOUNT_REF_LEN,
            new_region,
        );
        // New capacity slots and new condition slots start zeroed (inactive).
        data[new_region + old_capacity * ACCOUNT_REF_LEN..new_trailer].fill(0);
        data[old_region..new_region].fill(0);

        // Restamp the header for the new slot count.
        data[block_offset + 9] = C as u8;

        // Re-point every condition's resolver list. Offsets into the
        // built-in region rebase onto its new position; offsets behind the
        // old block end moved with their bytes; anything before the block
        // (or the zero of an inactive slot) stands.
        for index in 0..old_conditions {
            let at = block_offset + BLOCK_HEADER_LEN + index * CONDITION_LEN;
            let condition: &mut ConditionV0 =
                bytemuck::from_bytes_mut(&mut data[at..at + CONDITION_LEN]);
            let offset = condition.resolvers().offset as usize;
            let rebased = if (old_region..old_trailer).contains(&offset) {
                new_region + (offset - old_region)
            } else if offset >= old_end {
                offset + delta
            } else {
                continue;
            };
            condition.resolver_list_offset = (rebased as u32).to_le_bytes();
        }
        Ok(())
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::block::read_block;
    use crate::condition::{CrankSpecV0, WakeView};
    use crate::test_support::spec;
    use alloc::vec;
    use alloc::vec::Vec;

    /// Growing a block in place: a `<2, 8>` host becomes `<4, 16>`, the
    /// host tail and built-in list survive the shift, pointers re-base,
    /// and the new slots read as inactive conditions.
    #[test]
    fn grow_in_place_shifts_regions_and_repoints_lists() {
        type Old = RelayBlockV0<2, 8>;
        type New = RelayBlockV0<4, 16>;
        const BLOCK_OFFSET: usize = 8;
        const HOST_TAIL: &[u8; 5] = b"tail!";

        let mut block = Old::zeroed();
        block.init(BLOCK_OFFSET as u32).unwrap();
        let refs = [
            AccountRefV0::writable([1; 32]),
            AccountRefV0::readonly([2; 32]),
        ];
        let list = block.write_resolvers(&refs).unwrap();
        let spec = CrankSpecV0 {
            resolver_program: [1; 32],
            resolver_disc: [2; 8],
            min_payment: 5,
        };
        // Condition 0 points at the built-in region, condition 1 at a
        // host-side list past the block (the per-slot pattern).
        let host_list = (BLOCK_OFFSET + Old::SIZE) as u32;
        block
            .write_condition(0, &ConditionV0::at_timestamp(42, spec, list))
            .unwrap();
        block
            .write_condition(
                1,
                &ConditionV0::at_slot(7, spec, ResolverListV0::new(host_list, 1)),
            )
            .unwrap();

        // The account: discriminator, block, a host tail — then realloc'd
        // by the size delta, new bytes zeroed at the end.
        let mut account = alloc::vec![0u8; BLOCK_OFFSET + Old::SIZE];
        account[BLOCK_OFFSET..].copy_from_slice(bytemuck::bytes_of(&block));
        account.extend_from_slice(HOST_TAIL);
        account.resize(account.len() + (New::SIZE - Old::SIZE), 0);

        New::grow_in_place(&mut account, BLOCK_OFFSET, 2, 8).unwrap();

        // The host tail moved wholesale.
        assert_eq!(
            &account[BLOCK_OFFSET + New::SIZE..BLOCK_OFFSET + New::SIZE + 5],
            HOST_TAIL
        );
        let grown: &New = bytemuck::from_bytes(&account[BLOCK_OFFSET..BLOCK_OFFSET + New::SIZE]);
        assert_eq!(grown.account_offset(), BLOCK_OFFSET as u32);
        assert_eq!(grown.resolver_refs(), &refs[..]);
        let (_, conditions) = read_block(&account, BLOCK_OFFSET).unwrap();
        assert_eq!(conditions.len(), 4);
        assert_eq!(
            conditions[0].wake(),
            Ok(WakeView::AtTimestamp { unix_ts: 42 })
        );
        // Built-in-region pointer re-based; the ref it points at reads back.
        let moved = conditions[0].resolvers();
        assert_eq!(
            moved.offset as usize,
            BLOCK_OFFSET + BLOCK_HEADER_LEN + 4 * CONDITION_LEN
        );
        let first: AccountRefV0 = bytemuck::pod_read_unaligned(
            &account[moved.offset as usize..moved.offset as usize + ACCOUNT_REF_LEN],
        );
        assert_eq!(first, refs[0]);
        // Host-side pointer shifted with its bytes.
        assert_eq!(
            conditions[1].resolvers().offset as usize,
            BLOCK_OFFSET + New::SIZE
        );
        // New slots are inactive, not garbage.
        assert!(!conditions[2].is_active());
        assert!(!conditions[3].is_active());
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
        assert_eq!(block.resolver_refs(), &refs[..]);

        block
            .write_condition(
                0,
                &ConditionV0::at_timestamp(
                    42,
                    CrankSpecV0 {
                        resolver_program: [1; 32],
                        resolver_disc: [2; 8],
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
        let (_, conditions) = read_block(&account, BLOCK_OFFSET).unwrap();
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

    #[test]
    fn resolver_list_is_always_indirect() {
        let c = ConditionV0::at_timestamp(0, spec(0), ResolverListV0::new(96, 7));
        assert_eq!(c.resolvers(), ResolverListV0::new(96, 7));
        // Reserved space stays zero so a future field can claim it — the
        // executor identity used to occupy 40 of these bytes.
        assert_eq!(c._reserved, [0u8; 79]);
    }

    /// Rewriting the built-in list with a shorter one leaves nothing of the
    /// longer one behind. A condition written against the old list still
    /// carries the old count, and would otherwise read whatever the
    /// previous write left in those slots as live account refs.
    #[test]
    fn a_shorter_resolver_list_erases_the_one_before_it() {
        let mut block = RelayBlockV0::<1, 8>::zeroed();
        block.init(8).unwrap();
        let long = [
            AccountRefV0::writable([1; 32]),
            AccountRefV0::readonly([2; 32]),
            AccountRefV0::readonly([3; 32]),
        ];
        let list = block.write_resolvers(&long).unwrap();
        assert_eq!(list.count, 3);

        let short = [AccountRefV0::writable([9; 32])];
        let list = block.write_resolvers(&short).unwrap();
        assert_eq!(list.count, 1);
        assert_eq!(block.resolver_refs(), &short[..]);

        // Read the region the way a turner does — by offset and count,
        // from the raw bytes — with the count a stale condition would use.
        let region = &bytemuck::bytes_of(&block)[BLOCK_HEADER_LEN + CONDITION_LEN..];
        let stale: Vec<AccountRefV0> = region[..3 * ACCOUNT_REF_LEN]
            .chunks_exact(ACCOUNT_REF_LEN)
            .map(bytemuck::pod_read_unaligned)
            .collect();
        assert_eq!(
            stale,
            vec![short[0], AccountRefV0::zeroed(), AccountRefV0::zeroed()]
        );
    }
}

#[cfg(test)]
mod condition_block_tests {
    use super::*;
    use crate::block::read_block;
    use crate::block::BLOCK_HEADER_LEN;
    use crate::condition::{
        ConditionV0, CrankSpecV0, WakeView, WatchValue, WatchedRegion, CONDITION_LEN,
    };
    use crate::resolved::{stage_into, ResolvedCrankV0, ResponsePointerV0};
    use crate::resolver::ResolverListV0;
    use alloc::vec;

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
        assert_eq!(conditions[1].wake_slot(), 42);
        assert_eq!(conditions[0].active, 0);
        assert_eq!(host.read_condition(1).unwrap().wake_slot(), 42);

        // In-place wake updates (the min-fold/repair pattern), written
        // straight into the block's own bytes.
        host.update_condition(1, |c| c.set_wake(WakeView::EverySlots { slots: 9 }))
            .unwrap();
        assert_eq!(host.read_condition(1).unwrap().wake_slot(), 9);
        assert_eq!(host.condition(1).unwrap().wake_slot(), 9);

        // Deactivation is what makes a level-triggered wake go quiet.
        host.write_condition(
            2,
            &ConditionV0::on_value_cross(
                WatchedRegion::new([5; 32], 8, 8),
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
        let resolved = ResolvedCrankV0::new(
            [1; 32],
            [2; 8],
            vec![AccountRefV0::writable([9; 32])],
            vec![1, 2, 3],
        );
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
