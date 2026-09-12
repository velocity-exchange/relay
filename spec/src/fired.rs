//! The resolver's input: which condition fired, and the instruction data
//! that carries it.

use bytemuck::{Pod, Zeroable};

use crate::SpecError;

/// Which condition fired, handed to the resolver.
///
/// A resolver that serves several conditions — the point of letting it name
/// the executor — has to know which one it is answering for. The turner
/// therefore appends this to the resolver's instruction data, after the
/// 8-byte discriminator: `[resolver_disc: 8][target: 32][block_offset: u32
/// LE][index: u8]`, 45 bytes total, built by
/// [`encode_resolver_data`].
///
/// Instruction data is the cheapest faithful channel for it. It costs 37
/// transaction bytes and no accounts, no compute, and no extra reads: the
/// alternatives are an extra account (a whole 32-byte key plus a load, to
/// carry 5 bytes of context the target account already holds), or a
/// per-condition resolver discriminator (which is the very duplication this
/// change removes). The identity is exactly a turner's [`crate::WatchV0`]
/// coordinates plus the slot index, so nothing new has to be tracked to
/// produce it.
///
/// It is **not** authenticated, and does not need to be: it names the
/// resolver's *own* program state, which the resolver re-reads from the
/// accounts it was given. A resolver must therefore validate it the way it
/// validates any argument — check the target is the account it holds and
/// the index is one it serves — rather than trust it. A wrong identity can
/// only make a resolver answer about a condition that is not due, which
/// costs a simulation.
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Pod, Zeroable)]
#[cfg_attr(feature = "idl-build-v2", derive(anchor_lang_v2::IdlType))]
pub struct FiredConditionV0 {
    /// Account holding the condition block.
    pub target: [u8; 32],
    /// Byte offset of the block in that account (the `WatchV0` offset).
    pub block_offset: [u8; 4],
    /// Slot index of the condition within the block.
    pub index: u8,
}

pub const FIRED_CONDITION_LEN: usize = core::mem::size_of::<FiredConditionV0>();
const _: () = assert!(FIRED_CONDITION_LEN == 37);
const _: () = assert!(core::mem::align_of::<FiredConditionV0>() == 1);

/// Bytes of a resolver's instruction data: discriminator, then the fired
/// condition.
pub const RESOLVER_DATA_LEN: usize = 8 + FIRED_CONDITION_LEN;

impl FiredConditionV0 {
    pub fn new(target: [u8; 32], block_offset: u32, index: u8) -> Self {
        Self {
            target,
            block_offset: block_offset.to_le_bytes(),
            index,
        }
    }

    pub fn block_offset(&self) -> u32 {
        u32::from_le_bytes(self.block_offset)
    }

    pub fn to_bytes(&self) -> [u8; FIRED_CONDITION_LEN] {
        let mut out = [0u8; FIRED_CONDITION_LEN];
        out.copy_from_slice(bytemuck::bytes_of(self));
        out
    }

    /// Read from the tail of a resolver's instruction data — i.e. from the
    /// bytes after the discriminator. Trailing bytes are ignored.
    pub fn read(bytes: &[u8]) -> Result<Self, SpecError> {
        if bytes.len() < FIRED_CONDITION_LEN {
            return Err(SpecError::Truncated);
        }
        Ok(bytemuck::pod_read_unaligned(&bytes[..FIRED_CONDITION_LEN]))
    }
}

/// Encode a resolver's whole instruction data: its discriminator followed
/// by the condition that fired.
pub fn encode_resolver_data(
    resolver_disc: [u8; 8],
    fired: FiredConditionV0,
) -> [u8; RESOLVER_DATA_LEN] {
    let mut out = [0u8; RESOLVER_DATA_LEN];
    out[..8].copy_from_slice(&resolver_disc);
    out[8..].copy_from_slice(&fired.to_bytes());
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The resolver's input: which condition fired, appended to its
    /// instruction data after the discriminator.
    #[test]
    fn fired_condition_round_trips_through_instruction_data() {
        let fired = FiredConditionV0::new([3; 32], 912, 2);
        let data = encode_resolver_data([9; 8], fired);
        assert_eq!(data.len(), RESOLVER_DATA_LEN);
        assert_eq!(&data[..8], &[9u8; 8]);

        let parsed = FiredConditionV0::read(&data[8..]).unwrap();
        assert_eq!(parsed, fired);
        assert_eq!(parsed.target, [3; 32]);
        assert_eq!(parsed.block_offset(), 912);
        assert_eq!(parsed.index, 2);

        // Trailing bytes are ignored; a short tail is a truncation.
        let mut padded = data.to_vec();
        padded.push(0xAA);
        assert_eq!(FiredConditionV0::read(&padded[8..]).unwrap(), fired);
        assert_eq!(
            FiredConditionV0::read(&data[9..]),
            Err(SpecError::Truncated)
        );
    }
}
