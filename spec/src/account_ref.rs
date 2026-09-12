//! One account in an instruction's account list.

use bytemuck::{Pod, Zeroable};

/// One account in an instruction's account list. Never a signer: crank
/// executors are permissionless by contract, and relay's `crank_v0`
/// forwards every CPI account with `is_signer: false`. Align-1 (33 bytes)
/// so fixed arrays of it pack without padding.
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Pod, Zeroable)]
#[cfg_attr(feature = "idl-build-v2", derive(anchor_lang_v2::IdlType))]
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
