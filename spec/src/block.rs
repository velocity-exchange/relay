//! The condition block: a header followed by a fixed array of conditions,
//! and the readers and writers that work on it in place.

use bytemuck::{Pod, Zeroable};

use crate::condition::{ConditionV0, CONDITION_LEN};
use crate::{SpecError, MAGIC, SPEC_VERSION};

/// Block header. The block is `[header][ConditionV0; num_conditions]`,
/// contiguous, at an 8-aligned offset.
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Pod, Zeroable)]
#[cfg_attr(feature = "idl-build-v2", derive(anchor_lang_v2::IdlType))]
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

/// Account-data offset of a field: past anchor's 8-byte discriminator.
/// Every region a condition points at — resolver lists, staging — must be
/// described this way rather than by summing field lengths.
#[macro_export]
macro_rules! block_offset {
    ($ty:ty, $field:ident) => {
        8 + core::mem::offset_of!($ty, $field)
    };
}

/// Bytes a block of `n` conditions occupies.
pub const fn block_space(n: usize) -> usize {
    BLOCK_HEADER_LEN + n * CONDITION_LEN
}

/// Zero-copy view of a block at `data[offset..]`.
///
/// Every type in the block is alignment 1, so this works at **any** offset
/// in any buffer — an account's data on chain, an RPC response off it — and
/// costs a bounds check and a cast. There is deliberately no copying
/// sibling: one reader means one behaviour to reason about, and a turner
/// reading a large registry pays nothing per condition.
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::condition::WakeView;
    use crate::test_support::sample_conditions;

    /// The read path with no alignment left to get wrong: the same block
    /// written at every offset in a byte buffer reads back identically.
    #[test]
    fn blocks_read_in_place_at_any_offset() {
        let conditions = sample_conditions();
        (0..8).for_each(|offset| {
            let mut region = vec![0u8; offset + block_space(conditions.len())];
            write_block(&mut region[offset..], &conditions).unwrap();
            let (header, parsed) = read_block(&region, offset).unwrap();
            assert_eq!(header.num_conditions, 3);
            assert_eq!(parsed, &conditions[..], "offset {offset}");
        });
    }

    #[test]
    fn block_round_trip() {
        let conditions = sample_conditions();
        let mut bytes = vec![0u8; block_space(conditions.len())];
        let n = write_block(&mut bytes, &conditions).unwrap();
        assert_eq!(n, block_space(3));

        let (header, parsed) = read_block(&bytes, 0).unwrap();
        assert_eq!(header.num_conditions, 3);
        assert_eq!(parsed, &conditions[..]);

        // In-place wake update through the mut view.
        let parsed_mut = read_block_mut(&mut bytes, 0).unwrap();
        parsed_mut[0].set_wake(WakeView::AtTimestamp { unix_ts: 42 });
        let (_, parsed) = read_block(&bytes, 0).unwrap();
        assert_eq!(parsed[0].wake(), Ok(WakeView::AtTimestamp { unix_ts: 42 }));
    }

    #[test]
    fn bad_magic_and_version_rejected() {
        let conditions = sample_conditions();
        let mut region = vec![0u8; block_space(conditions.len())];
        write_block(&mut region, &conditions).unwrap();

        let mut tampered = region.clone();
        tampered[0] ^= 0xFF;
        assert_eq!(read_block(&tampered, 0).err(), Some(SpecError::BadMagic));

        let mut tampered = region;
        tampered[8] = SPEC_VERSION + 1;
        assert_eq!(
            read_block(&tampered, 0).err(),
            Some(SpecError::UnsupportedVersion)
        );
    }

    #[test]
    fn truncation_rejected() {
        let conditions = sample_conditions();
        let mut region = vec![0u8; block_space(conditions.len())];
        write_block(&mut region, &conditions).unwrap();
        (0..region.len()).for_each(|n| {
            assert!(read_block(&region[..n], 0).is_err(), "cut {n} should fail");
        });
    }
}
