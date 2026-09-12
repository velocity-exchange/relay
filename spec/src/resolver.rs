//! Where a condition's resolver account list lives, and the striped
//! regions a host uses when one shared list will not serve.

//! Per-condition resolver lists, stored in a striped byte region the host owns.
//!
//! [`crate::RelayBlockV0::write_resolvers`] serves the common case: one shared list
//! every condition points at. Some hosts instead need a *distinct* list per
//! condition — a trigger slot, say, names its own market oracle and perp
//! market, so no single list serves them all. The block's one built-in region
//! cannot hold those, so such a host declares a byte region of
//! [`resolver_stripes_len`] bytes and points each condition at its own stripe
//! with [`write_resolver_stripe`]. That keeps the stride and offset arithmetic,
//! and the "rewrite the whole stripe" rule, in one tested place instead of
//! hand-rolled in every host. The region stays plain bytes, so it adds no wire
//! type and stays IDL-friendly.
//!
//! A stripe holds up to `refs_per_slot` refs. Its stride rounds that up to a
//! multiple of 8 so a region of stripes stays 8-divisible and never forces
//! padding into its host — the granularity [`crate::RelayBlockV0`] keeps for the same
//! reason.

use crate::account_ref::{AccountRefV0, ACCOUNT_REF_LEN};
use crate::SpecError;

/// Where a condition's resolver account list lives: `count`
/// [`AccountRefV0`]s at `offset` bytes into the account holding the block.
///
/// The list is always indirect — one copy per account, shared by every
/// condition that points at it, read fresh by the turner on every attempt.
/// A resolver needing many accounts (e.g. one that CPIs an external
/// quoter's registered `quote_v0` surface under simulation) costs those
/// bytes once instead of once per condition.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[cfg_attr(feature = "idl-build-v2", derive(anchor_lang_v2::IdlType))]
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

/// The bytes one stripe reserves: `refs_per_slot` refs rounded up to a
/// multiple of 8.
pub const fn resolver_stripe_stride(refs_per_slot: usize) -> usize {
    let raw = refs_per_slot * ACCOUNT_REF_LEN;
    raw.div_ceil(8) * 8
}

/// The bytes a striped resolver region of `slots` stripes occupies.
pub const fn resolver_stripes_len(slots: usize, refs_per_slot: usize) -> usize {
    slots * resolver_stripe_stride(refs_per_slot)
}

/// Write one condition's resolver list into `slot`'s stripe of a host-owned
/// striped region, and describe it the way a [`crate::ConditionV0`] wants it.
///
/// `region_offset` is where the region sits in its account
/// (`block_offset!(Host, field)`). The whole stripe is rewritten, so a
/// condition still pointing here with an older, larger count cannot read the
/// tail of a previous write as live refs.
pub fn write_resolver_stripe(
    region: &mut [u8],
    region_offset: u32,
    refs_per_slot: usize,
    slot: usize,
    refs: &[AccountRefV0],
) -> Result<ResolverListV0, SpecError> {
    if refs.len() > refs_per_slot || refs.len() > MAX_INDIRECT_RESOLVER_ACCOUNTS {
        return Err(SpecError::TooLarge);
    }
    let stride = resolver_stripe_stride(refs_per_slot);
    let base = slot.checked_mul(stride).ok_or(SpecError::Truncated)?;
    let end = base.checked_add(stride).ok_or(SpecError::Truncated)?;
    let stripe = region.get_mut(base..end).ok_or(SpecError::Truncated)?;
    stripe.fill(0);
    for (i, r) in refs.iter().enumerate() {
        let at = i * ACCOUNT_REF_LEN;
        stripe[at..at + 32].copy_from_slice(&r.address);
        stripe[at + 32] = r.writable;
    }
    Ok(ResolverListV0::new(
        region_offset + base as u32,
        refs.len() as u8,
    ))
}

/// Read a stripe's refs back — `count` of them, from the [`ResolverListV0`] the
/// write returned. Borrowed in place: [`AccountRefV0`] is alignment-1 pod and a
/// stripe is a packed array of them.
pub fn read_resolver_stripe(
    region: &[u8],
    refs_per_slot: usize,
    slot: usize,
    count: usize,
) -> Option<&[AccountRefV0]> {
    if count > refs_per_slot {
        return None;
    }
    let stride = resolver_stripe_stride(refs_per_slot);
    let base = slot.checked_mul(stride)?;
    let bytes = region.get(base..base.checked_add(count * ACCOUNT_REF_LEN)?)?;
    Some(bytemuck::cast_slice(bytes))
}

#[cfg(test)]
mod tests {
    use super::*;
    use bytemuck::Zeroable;

    #[test]
    fn resolver_stripes_write_and_read_back_by_slot() {
        const SLOTS: usize = 3;
        const REFS: usize = 5;
        // The stride matches the 168 a host hand-rolled for 5 refs before this
        // helper existed, so adopting it is not a layout change.
        assert_eq!(resolver_stripe_stride(REFS), 168);
        assert_eq!(resolver_stripes_len(SLOTS, REFS), 3 * 168);

        let mut region = vec![0u8; resolver_stripes_len(SLOTS, REFS)];
        let region_offset = 200u32;
        let refs = [
            AccountRefV0::writable([1; 32]),
            AccountRefV0::readonly([2; 32]),
        ];
        let list = write_resolver_stripe(&mut region, region_offset, REFS, 1, &refs).unwrap();
        // Slot 1 sits one stride in, and the list points there.
        assert_eq!(
            list.offset,
            region_offset + resolver_stripe_stride(REFS) as u32
        );
        assert_eq!(list.count, 2);
        assert_eq!(
            read_resolver_stripe(&region, REFS, 1, list.count as usize).unwrap(),
            &refs[..]
        );
        // A stripe is untouched by a write to another.
        assert!(read_resolver_stripe(&region, REFS, 0, REFS)
            .unwrap()
            .iter()
            .all(|r| *r == AccountRefV0::zeroed()));
        // More refs than a stripe holds is refused.
        let too_many = [AccountRefV0::readonly([0; 32]); REFS + 1];
        assert_eq!(
            write_resolver_stripe(&mut region, region_offset, REFS, 0, &too_many).err(),
            Some(SpecError::TooLarge)
        );
        // A slot past the region is refused.
        assert_eq!(
            write_resolver_stripe(&mut region, region_offset, REFS, SLOTS, &refs).err(),
            Some(SpecError::Truncated)
        );
    }

    /// A shorter overwrite of a stripe leaves none of the longer one behind —
    /// the same hazard [`crate::RelayBlockV0::write_resolvers`] guards, per stripe.
    #[test]
    fn a_shorter_stripe_erases_the_one_before_it() {
        const REFS: usize = 5;
        let mut region = vec![0u8; resolver_stripes_len(1, REFS)];
        let long = [
            AccountRefV0::writable([1; 32]),
            AccountRefV0::readonly([2; 32]),
            AccountRefV0::readonly([3; 32]),
        ];
        write_resolver_stripe(&mut region, 8, REFS, 0, &long).unwrap();
        let short = [AccountRefV0::writable([9; 32])];
        write_resolver_stripe(&mut region, 8, REFS, 0, &short).unwrap();
        // Read three the way a stale condition's count would: only the new
        // ref survives, the rest are zeroed.
        assert_eq!(
            read_resolver_stripe(&region, REFS, 0, 3).unwrap(),
            &[short[0], AccountRefV0::zeroed(), AccountRefV0::zeroed()][..]
        );
    }
}
