//! A resolver's answer: the pointer it returns, and the staged payload
//! that pointer locates.

use alloc::vec::Vec;

use bytemuck::{Pod, Zeroable};

use crate::account_ref::{AccountRefV0, ACCOUNT_REF_LEN};
use crate::SpecError;

/// A resolver's return data: whether there is work, and where the payload
/// was staged. Align-1 pod, 10 bytes — well inside the return-data cap no
/// matter how large the payload is.
///
/// `account_index` indexes the condition's `resolver_accounts`; `offset`
/// and `len` are a byte range in that account's **post-simulation** data.
/// A no-work result needs no staging write at all.
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Pod, Zeroable)]
#[cfg_attr(feature = "idl-build-v2", derive(anchor_lang_v2::IdlType))]
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

/// The staged payload a [`ResponsePointerV0`] points at: the whole
/// instruction to run. Align-1 wire:
/// `[executor_program: 32][executor_disc: 8][num_accounts: u8][data_len: u16 LE][AccountRefV0; num][data bytes]`
///
/// **The resolver names the executor.** A condition used to carry the
/// executor's program and discriminator as literals, which meant one
/// condition slot could only ever run one instruction — a program with a
/// family of like-instructions (settle *this* kind of position, sweep *that*
/// kind of order) needed a slot, and a wake, for each. Returning the
/// identity instead costs 40 staged bytes and adds no trust: the resolver
/// already decides the accounts and args, so a turner willing to run what a
/// resolver picked out is willing to run *which* instruction it picked. The
/// guards are what bound the damage either way.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
#[cfg_attr(feature = "idl-build-v2", derive(anchor_lang_v2::IdlType))]
pub struct ResolvedCrankV0 {
    /// Program owning the instruction to submit.
    pub executor_program: [u8; 32],
    /// Its 8-byte instruction discriminator; `data` follows it.
    pub executor_disc: [u8; 8],
    /// Full executor account list, in order. [`crate::KEEPER_PLACEHOLDER`] entries
    /// are replaced with the turner's keeper.
    pub accounts: Vec<AccountRefV0>,
    /// Executor args after the discriminator.
    pub data: Vec<u8>,
}

pub const RESOLVED_HEADER_LEN: usize = 43;

impl ResolvedCrankV0 {
    /// The common shape: an executor in the resolver's own program.
    pub fn new(
        executor_program: [u8; 32],
        executor_disc: [u8; 8],
        accounts: Vec<AccountRefV0>,
        data: Vec<u8>,
    ) -> Self {
        Self {
            executor_program,
            executor_disc,
            accounts,
            data,
        }
    }

    /// Bytes this payload needs when staged.
    pub fn encoded_len(&self) -> usize {
        RESOLVED_HEADER_LEN + self.accounts.len() * ACCOUNT_REF_LEN + self.data.len()
    }

    pub fn read(bytes: &[u8]) -> Result<Self, SpecError> {
        if bytes.len() < RESOLVED_HEADER_LEN {
            return Err(SpecError::Truncated);
        }
        let executor_program: [u8; 32] = bytes[..32].try_into().unwrap();
        let executor_disc: [u8; 8] = bytes[32..40].try_into().unwrap();
        let n = bytes[40] as usize;
        let data_len = u16::from_le_bytes([bytes[41], bytes[42]]) as usize;
        let accounts_end = RESOLVED_HEADER_LEN + n * ACCOUNT_REF_LEN;
        let end = accounts_end + data_len;
        if end > bytes.len() {
            return Err(SpecError::Truncated);
        }
        let accounts = bytes[RESOLVED_HEADER_LEN..accounts_end]
            .chunks_exact(ACCOUNT_REF_LEN)
            .map(bytemuck::pod_read_unaligned)
            .collect();
        Ok(Self {
            executor_program,
            executor_disc,
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
        region[..32].copy_from_slice(&self.executor_program);
        region[32..40].copy_from_slice(&self.executor_disc);
        region[40] = self.accounts.len() as u8;
        region[41..43].copy_from_slice(&(self.data.len() as u16).to_le_bytes());
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::KEEPER_PLACEHOLDER;
    use alloc::vec;

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
        let resolved = ResolvedCrankV0::new(
            [4; 32],
            [8; 8],
            vec![
                AccountRefV0::writable(KEEPER_PLACEHOLDER),
                AccountRefV0::writable([5; 32]),
            ],
            vec![1, 2, 3, 4],
        );
        let bytes = resolved.to_bytes();
        assert_eq!(bytes.len(), resolved.encoded_len());
        assert_eq!(ResolvedCrankV0::read(&bytes).unwrap(), resolved);
    }

    /// The executor's identity comes back from the resolver, in front of
    /// the account list, so one condition slot can run whichever
    /// instruction the resolver picked.
    #[test]
    fn staged_payload_names_the_executor() {
        let resolved = ResolvedCrankV0::new(
            [7; 32],
            [1, 2, 3, 4, 5, 6, 7, 8],
            vec![AccountRefV0::writable(KEEPER_PLACEHOLDER)],
            vec![9, 9],
        );
        let bytes = resolved.to_bytes();
        assert_eq!(&bytes[..32], &[7u8; 32]);
        assert_eq!(&bytes[32..40], &[1, 2, 3, 4, 5, 6, 7, 8]);
        assert_eq!(bytes[40], 1, "one account");
        assert_eq!(&bytes[41..43], &2u16.to_le_bytes());

        let parsed = ResolvedCrankV0::read(&bytes).unwrap();
        assert_eq!(parsed.executor_program, [7; 32]);
        assert_eq!(parsed.executor_disc, [1, 2, 3, 4, 5, 6, 7, 8]);
        assert_eq!(parsed, resolved);

        // A payload cut short of the identity is a truncation, not a
        // silently zeroed program id.
        (0..RESOLVED_HEADER_LEN).for_each(|n| {
            assert_eq!(
                ResolvedCrankV0::read(&bytes[..n]),
                Err(SpecError::Truncated),
                "cut {n}"
            );
        });
    }

    #[test]
    fn staged_payload_write_into_region() {
        let resolved = ResolvedCrankV0::new(
            [1; 32],
            [2; 8],
            vec![AccountRefV0::writable(KEEPER_PLACEHOLDER)],
            vec![7; 10],
        );
        // A region much larger than the payload: only the used prefix is
        // meaningful, and the pointer's `len` says how much.
        let mut region = [0xAAu8; 512];
        let n = resolved.write_into(&mut region).unwrap();
        assert_eq!(ResolvedCrankV0::read(&region[..n]).unwrap(), resolved);
        assert_eq!(region[n], 0xAA, "write must not clobber past len");

        let mut tiny = [0u8; RESOLVED_HEADER_LEN];
        assert_eq!(resolved.write_into(&mut tiny), Err(SpecError::TooLarge));
    }

    /// The size argument for staging: a batch payload that would blow the
    /// 1024-byte return-data cap stages fine.
    #[test]
    fn staged_payload_exceeds_return_data_cap() {
        let resolved = ResolvedCrankV0::new(
            [1; 32],
            [2; 8],
            (0..40u8).map(|i| AccountRefV0::writable([i; 32])).collect(),
            vec![9; 400],
        );
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
}
