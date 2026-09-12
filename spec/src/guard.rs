//! Relay's payment guard instructions, as a turner encodes them.
//!
//! The discriminators are pinned copies of the values the relay program
//! generates, so a turner needs no hash dependency. The program's test suite
//! asserts that they still match.

/// Anchor instruction discriminators of relay's payment guards. Pinned
/// here so turners don't need a hash dependency; the program's test suite
/// asserts they match the generated constants.
pub const BEGIN_GUARD_V0_DISCRIMINATOR: [u8; 8] = [151, 249, 122, 144, 42, 38, 241, 176];
pub const ASSERT_PAID_V0_DISCRIMINATOR: [u8; 8] = [117, 202, 135, 43, 41, 100, 21, 141];

/// PDA seed prefix for a keeper's guard account: `["guard", keeper, nonce]`.
pub const GUARD_SEED: &[u8] = b"guard";

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
