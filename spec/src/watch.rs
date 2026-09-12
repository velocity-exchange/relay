//! The relay watch registry account, as a turner reads it.

use crate::SpecError;

/// Anchor account discriminator of relay's `WatchV0`
/// (`sha256("account:WatchV0")[..8]`).
pub const WATCH_V0_DISCRIMINATOR: [u8; 8] = [177, 10, 201, 159, 2, 232, 62, 244];

/// Serialized length of a `WatchV0` account:
/// `[disc: 8][target_program: 32][target: 32][creator: 32][offset: u32][_pad: 4]`.
pub const WATCH_V0_LEN: usize = 112;

/// Byte offset of `target_program` in a `WatchV0` account. `target_program`
/// leads the struct so turners can memcmp-filter the registry server-side
/// (`getProgramAccounts` filters, geyser account filters) and never receive
/// the watches of programs they don't crank — the cheapest possible way to
/// ignore another protocol's expensive conditions.
pub const WATCH_TARGET_PROGRAM_OFFSET: usize = 8;

/// Byte offset of `target` in a `WatchV0` account.
pub const WATCH_TARGET_OFFSET: usize = 40;

/// Byte offset of `creator` in a `WatchV0` account — memcmp-filterable
/// too, for turners that key off who registered rather than what program
/// owns the target.
pub const WATCH_CREATOR_OFFSET: usize = 72;

/// A parsed relay `WatchV0` registry account.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(feature = "idl-build-v2", derive(anchor_lang_v2::IdlType))]
pub struct WatchV0 {
    /// Owner program of `target`, recorded by the relay program from the
    /// account itself at registration — a creator cannot forge it.
    pub target_program: [u8; 32],
    pub target: [u8; 32],
    /// Who registered the watch, and the only key that may close it.
    pub creator: [u8; 32],
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
            creator: field(WATCH_CREATOR_OFFSET),
            offset: u32::from_le_bytes(data[104..108].try_into().unwrap()),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn watch_account_parse() {
        let data: Vec<u8> = WATCH_V0_DISCRIMINATOR
            .into_iter()
            .chain([6; 32]) // target_program
            .chain([8; 32]) // target
            .chain([7; 32]) // creator
            .chain(123u32.to_le_bytes())
            .chain([0; 4])
            .collect();
        assert_eq!(data.len(), WATCH_V0_LEN);
        let w = WatchV0::read_from_account(&data).unwrap();
        assert_eq!(w.target_program, [6; 32]);
        assert_eq!(w.target, [8; 32]);
        assert_eq!(w.creator, [7; 32]);
        assert_eq!(w.offset, 123);

        // The memcmp offsets turners filter on must address those fields.
        assert_eq!(&data[WATCH_TARGET_PROGRAM_OFFSET..][..32], &[6; 32]);
        assert_eq!(&data[WATCH_TARGET_OFFSET..][..32], &[8; 32]);
        assert_eq!(&data[WATCH_CREATOR_OFFSET..][..32], &[7; 32]);

        let mut bad = data;
        bad[0] ^= 1;
        assert_eq!(
            WatchV0::read_from_account(&bad),
            Err(SpecError::BadDiscriminator)
        );
    }
}
