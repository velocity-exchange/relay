//! One crankable condition, and the wake that decides when it is due.

use bytemuck::{Pod, Zeroable};

use crate::resolver::ResolverListV0;
use crate::SpecError;

/// Wake kinds (the `wake_kind` byte on [`ConditionV0`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(feature = "idl-build-v2", derive(anchor_lang_v2::IdlType))]
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
    /// Due while the watched value sits at-or-beyond a threshold: the
    /// bytes at `wake_account.data[wake_offset..wake_offset + wake_len]`
    /// read as a little-endian integer (widths 1/2/4/8; signed and
    /// sign-extended by default, unsigned and zero-extended when
    /// `wake_value_unsigned` is set — declared by the [`WatchValue`] the
    /// condition was built with), compared against `wake_ts` reinterpreted
    /// as the threshold, in the direction `wake_cmp` selects (0 = due when
    /// value >= threshold, 1 = due when value <= threshold).
    ///
    /// This is the wake for price-like conditions — trigger orders,
    /// liquidation thresholds — where `OnAccountChange` would wake on
    /// every oracle tick and burn a resolver simulation per condition per
    /// tick even when the price is nowhere near. Level-triggered: the
    /// condition stays due while the value is beyond the threshold, so the
    /// program must deactivate or re-point the condition once the work is
    /// done (and the turner's no-work backoff bounds the cost of a due
    /// condition whose resolver finds nothing).
    OnValueCross = 4,
}

/// Copied, alloc-free view of a condition's wake for evaluation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(feature = "idl-build-v2", derive(anchor_lang_v2::IdlType))]
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
    OnValueCross {
        address: [u8; 32],
        offset: u32,
        len: u32,
        threshold: WatchValue,
        /// 0 = due when value >= threshold, 1 = due when value <= threshold.
        cmp: u8,
    },
}

/// The bytes a change- or value-cross wake watches: an account and a byte
/// range in its data.
///
/// The three travel together through every wake that reads account bytes, so
/// they are one argument rather than three. The account may be the one
/// holding the condition block or any other, an oracle for example.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(feature = "idl-build-v2", derive(anchor_lang_v2::IdlType))]
pub struct WatchedRegion {
    pub address: [u8; 32],
    pub offset: u32,
    pub len: u32,
}

impl WatchedRegion {
    pub fn new(address: [u8; 32], offset: u32, len: u32) -> Self {
        Self {
            address,
            offset,
            len,
        }
    }
}

/// A watched integer with its signedness. On-chain fields are unsigned at
/// least as often as they are signed (token amounts, counters, most u64
/// prices), and sign-extending an unsigned field with its top bit set
/// inverts every comparison — so the threshold carries which domain it
/// lives in, and the watched bytes are always read in that same domain.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(feature = "idl-build-v2", derive(anchor_lang_v2::IdlType))]
pub enum WatchValue {
    Signed(i64),
    Unsigned(u64),
}

impl WatchValue {
    pub fn is_unsigned(&self) -> bool {
        matches!(self, WatchValue::Unsigned(_))
    }

    /// Both domains fit in i128, so comparisons are uniform.
    pub fn widened(self) -> i128 {
        match self {
            WatchValue::Signed(v) => v as i128,
            WatchValue::Unsigned(v) => v as i128,
        }
    }
}

impl core::fmt::Display for WatchValue {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            WatchValue::Signed(v) => write!(f, "{v}"),
            WatchValue::Unsigned(v) => write!(f, "{v}u"),
        }
    }
}

/// Read a watched region as the little-endian integer
/// [`WakeKind::OnValueCross`] compares (widths 1/2/4/8; sign-extended when
/// signed, zero-extended when unsigned). `None` for any other width — an
/// unreadable value is never due.
pub fn read_watched_value(bytes: &[u8], unsigned: bool) -> Option<WatchValue> {
    if unsigned {
        let value = match bytes.len() {
            1 => u8::from_le_bytes(bytes.try_into().ok()?) as u64,
            2 => u16::from_le_bytes(bytes.try_into().ok()?) as u64,
            4 => u32::from_le_bytes(bytes.try_into().ok()?) as u64,
            8 => u64::from_le_bytes(bytes.try_into().ok()?),
            _ => return None,
        };
        return Some(WatchValue::Unsigned(value));
    }
    let value = match bytes.len() {
        1 => i8::from_le_bytes(bytes.try_into().ok()?) as i64,
        2 => i16::from_le_bytes(bytes.try_into().ok()?) as i64,
        4 => i32::from_le_bytes(bytes.try_into().ok()?) as i64,
        8 => i64::from_le_bytes(bytes.try_into().ok()?),
        _ => return None,
    };
    Some(WatchValue::Signed(value))
}

/// Whether an [`WakeKind::OnValueCross`] condition is due for `value`.
pub fn value_crossed(value: WatchValue, threshold: WatchValue, cmp: u8) -> bool {
    match cmp {
        0 => value.widened() >= threshold.widened(),
        _ => value.widened() <= threshold.widened(),
    }
}

/// Everything about a condition except its wake — the arguments shared by
/// all the constructors.
///
/// The executor is deliberately absent: a resolver names the instruction to
/// run in its [`crate::ResolvedCrankV0`], so a condition says *how to find out
/// what to do* and never *what to do*. If you trust the resolver you
/// already trust what it returns, and one resolver can then serve a family
/// of like-instructions from one condition slot.
#[derive(Debug, Clone, Copy)]
#[cfg_attr(feature = "idl-build-v2", derive(anchor_lang_v2::IdlType))]
pub struct CrankSpecV0 {
    pub resolver_program: [u8; 32],
    pub resolver_disc: [u8; 8],
    pub min_payment: u64,
}

/// One crankable condition. Fixed 192 bytes, **alignment 1**, no interior
/// padding: read it in place from any offset, update wake inputs with
/// single field stores. Wake variants are flattened into dedicated fields
/// (selected by `wake_kind`) rather than an enum, precisely so a program
/// can rewrite one wake input and touch nothing else.
///
/// Every multi-byte scalar is stored as a little-endian byte array with an
/// accessor of the same name ([`Self::min_payment`], [`Self::wake_ts`], …).
/// That is what buys alignment 1, and alignment 1 is what lets *every*
/// reader — on-chain host, off-chain turner, a test looking at raw account
/// bytes — cast a block in place instead of copying it condition by
/// condition.
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Pod, Zeroable)]
#[cfg_attr(feature = "idl-build-v2", derive(anchor_lang_v2::IdlType))]
pub struct ConditionV0 {
    /// Lamports the executor must pay the keeper. Turners use it to decide
    /// whether a crank is worth the fee, and pass it to `assert_paid_v0`.
    pub(crate) min_payment: [u8; 8],
    /// [`WakeKind::AtTimestamp`] input.
    pub(crate) wake_ts: [u8; 8],
    /// [`WakeKind::EverySlots`] input (interval) or [`WakeKind::AtSlot`]
    /// input (absolute slot) — selected by `wake_kind`.
    pub(crate) wake_slot: [u8; 8],
    /// [`WakeKind::OnAccountChange`] inputs.
    pub(crate) wake_account: [u8; 32],
    pub(crate) wake_offset: [u8; 4],
    pub(crate) wake_len: [u8; 4],
    /// Instruction the turner simulates to discover work. Stages a
    /// [`crate::ResolvedCrankV0`] in one of the resolver's accounts and returns a
    /// [`ResponsePointerV0`]; the turner appends a [`FiredConditionV0`] to
    /// its instruction data so it knows which condition it is answering
    /// for.
    pub(crate) resolver_program: [u8; 32],
    pub(crate) resolver_disc: [u8; 8],
    /// How many [`AccountRefV0`]s make up the resolver's account list. The
    /// list itself lives at `resolver_list_offset`; see there.
    pub(crate) num_resolver_accounts: u8,
    /// A [`WakeKind`] value.
    pub(crate) wake_kind: u8,
    /// 0 = inactive (skipped by turners).
    pub(crate) active: u8,
    /// [`WakeKind::OnValueCross`] comparator: 0 = due when value >=
    /// threshold (`wake_ts`), 1 = due when value <= threshold.
    pub(crate) wake_cmp: u8,
    /// Byte offset, within *this condition block's own account*, of the
    /// `num_resolver_accounts` [`AccountRefV0`]s that form the resolver's
    /// account list. The staging account must be among them and marked
    /// writable.
    ///
    /// The list is always indirect. Conditions used to carry four inline
    /// refs, which cost 132 of every condition's bytes whether or not they
    /// were used — and any resolver needing more than four had to point
    /// somewhere anyway. One list per account, shared by every condition
    /// that wants it, is both smaller and the only mechanism to reason
    /// about.
    pub(crate) resolver_list_offset: [u8; 4],
    /// [`WakeKind::OnValueCross`]: nonzero = the watched bytes and the
    /// threshold (`wake_ts`'s bits reinterpreted as `u64`) are unsigned.
    /// Zero keeps the signed reading.
    pub(crate) wake_value_unsigned: u8,
    /// Reserved. Zero on write, ignored on read — room to add fields
    /// without moving every existing one or resizing every account that
    /// holds a block. The executor program and discriminator used to live
    /// in 40 of these bytes; the resolver returns them now.
    pub(crate) _reserved: [u8; 79],
}

pub const CONDITION_LEN: usize = core::mem::size_of::<ConditionV0>();
const _: () = assert!(CONDITION_LEN == 192);
const _: () = assert!(core::mem::align_of::<ConditionV0>() == 1);

impl ConditionV0 {
    fn base(spec: CrankSpecV0, resolvers: ResolverListV0) -> Self {
        Self {
            min_payment: spec.min_payment.to_le_bytes(),
            wake_ts: [0; 8],
            wake_slot: [0; 8],
            wake_account: [0; 32],
            wake_offset: [0; 4],
            wake_len: [0; 4],
            resolver_program: spec.resolver_program,
            resolver_disc: spec.resolver_disc,
            num_resolver_accounts: resolvers.count,
            wake_kind: 0,
            active: 1,
            wake_cmp: 0,
            resolver_list_offset: resolvers.offset.to_le_bytes(),
            wake_value_unsigned: 0,
            _reserved: [0; 79],
        }
    }

    pub fn at_timestamp(unix_ts: i64, spec: CrankSpecV0, resolvers: ResolverListV0) -> Self {
        let mut c = Self::base(spec, resolvers);
        c.set_wake(WakeView::AtTimestamp { unix_ts });
        c
    }

    pub fn on_account_change(
        watched: WatchedRegion,
        spec: CrankSpecV0,
        resolvers: ResolverListV0,
    ) -> Self {
        let mut c = Self::base(spec, resolvers);
        c.set_wake(WakeView::OnAccountChange {
            address: watched.address,
            offset: watched.offset,
            len: watched.len,
        });
        c
    }

    /// Level-triggered value threshold (see [`WakeKind::OnValueCross`]):
    /// due while the watched value is at-or-beyond `threshold` in the
    /// direction `cmp` selects (0 = >=, 1 = <=). The threshold's
    /// [`WatchValue`] variant declares the watched field's signedness —
    /// the caller knows the layout it is watching, and reading an unsigned
    /// field sign-extended inverts the comparison once the top bit is set.
    pub fn on_value_cross(
        watched: WatchedRegion,
        threshold: WatchValue,
        cmp: u8,
        spec: CrankSpecV0,
        resolvers: ResolverListV0,
    ) -> Self {
        let mut c = Self::base(spec, resolvers);
        c.set_wake(WakeView::OnValueCross {
            address: watched.address,
            offset: watched.offset,
            len: watched.len,
            threshold,
            cmp,
        });
        c
    }

    pub fn every_slots(slots: u64, spec: CrankSpecV0, resolvers: ResolverListV0) -> Self {
        let mut c = Self::base(spec, resolvers);
        c.set_wake(WakeView::EverySlots { slots });
        c
    }

    pub fn at_slot(slot: u64, spec: CrankSpecV0, resolvers: ResolverListV0) -> Self {
        let mut c = Self::base(spec, resolvers);
        c.set_wake(WakeView::AtSlot { slot });
        c
    }

    /// Lamports the executor must pay the keeper.
    pub fn min_payment(&self) -> u64 {
        u64::from_le_bytes(self.min_payment)
    }

    /// [`WakeKind::AtTimestamp`] input (and [`WakeKind::OnValueCross`]'s
    /// threshold bits — see [`Self::wake`] for the reading that applies).
    pub fn wake_ts(&self) -> i64 {
        i64::from_le_bytes(self.wake_ts)
    }

    /// [`WakeKind::AtSlot`] / [`WakeKind::EverySlots`] input.
    pub fn wake_slot(&self) -> u64 {
        u64::from_le_bytes(self.wake_slot)
    }

    /// Watched account of a change- or value-cross wake.
    pub fn wake_account(&self) -> [u8; 32] {
        self.wake_account
    }

    /// Watched byte range of a change- or value-cross wake.
    pub fn wake_range(&self) -> (u32, u32) {
        (
            u32::from_le_bytes(self.wake_offset),
            u32::from_le_bytes(self.wake_len),
        )
    }

    /// The raw [`WakeKind`] byte. [`Self::wake`] is the checked reading.
    pub fn wake_kind(&self) -> u8 {
        self.wake_kind
    }

    /// Where this condition's resolver account list lives.
    pub fn resolvers(&self) -> ResolverListV0 {
        ResolverListV0::new(
            u32::from_le_bytes(self.resolver_list_offset),
            self.num_resolver_accounts,
        )
    }

    /// The resolver to simulate, and what the crank must pay.
    pub fn crank_spec(&self) -> CrankSpecV0 {
        CrankSpecV0 {
            resolver_program: self.resolver_program,
            resolver_disc: self.resolver_disc,
            min_payment: self.min_payment(),
        }
    }

    /// Point this condition at a different resolver account list.
    pub fn set_resolvers(&mut self, resolvers: ResolverListV0) {
        self.resolver_list_offset = resolvers.offset.to_le_bytes();
        self.num_resolver_accounts = resolvers.count;
    }

    /// Re-price the keeper fee (hosts re-price in place on reconfigure).
    pub fn set_min_payment(&mut self, lamports: u64) {
        self.min_payment = lamports.to_le_bytes();
    }

    /// Go quiet. A level-triggered wake fires until this is called.
    pub fn deactivate(&mut self) {
        self.active = 0;
    }

    /// Rewrite the wake, by variant. The wire format overloads its fields
    /// — `wake_ts` is a timestamp for one kind and a threshold for another
    /// — so this is the only sanctioned way to change one: it always
    /// writes the discriminant and the fields together, and clears what
    /// the new variant does not use.
    pub fn set_wake(&mut self, wake: WakeView) {
        self.wake_ts = [0; 8];
        self.wake_slot = [0; 8];
        self.wake_account = [0; 32];
        self.wake_offset = [0; 4];
        self.wake_len = [0; 4];
        self.wake_cmp = 0;
        self.wake_value_unsigned = 0;
        match wake {
            WakeView::AtTimestamp { unix_ts } => {
                self.wake_kind = WakeKind::AtTimestamp as u8;
                self.wake_ts = unix_ts.to_le_bytes();
            }
            WakeView::OnAccountChange {
                address,
                offset,
                len,
            } => {
                self.wake_kind = WakeKind::OnAccountChange as u8;
                self.set_watched(address, offset, len);
            }
            WakeView::EverySlots { slots } => {
                self.wake_kind = WakeKind::EverySlots as u8;
                self.wake_slot = slots.to_le_bytes();
            }
            WakeView::AtSlot { slot } => {
                self.wake_kind = WakeKind::AtSlot as u8;
                self.wake_slot = slot.to_le_bytes();
            }
            WakeView::OnValueCross {
                address,
                offset,
                len,
                threshold,
                cmp,
            } => {
                self.wake_kind = WakeKind::OnValueCross as u8;
                self.set_watched(address, offset, len);
                self.wake_cmp = cmp;
                match threshold {
                    WatchValue::Signed(value) => self.wake_ts = value.to_le_bytes(),
                    WatchValue::Unsigned(value) => {
                        self.wake_ts = value.to_le_bytes();
                        self.wake_value_unsigned = 1;
                    }
                }
            }
        }
    }

    fn set_watched(&mut self, address: [u8; 32], offset: u32, len: u32) {
        self.wake_account = address;
        self.wake_offset = offset.to_le_bytes();
        self.wake_len = len.to_le_bytes();
    }

    pub fn is_active(&self) -> bool {
        self.active != 0
    }

    pub fn wake(&self) -> Result<WakeView, SpecError> {
        let (offset, len) = self.wake_range();
        match self.wake_kind {
            0 => Ok(WakeView::AtTimestamp {
                unix_ts: self.wake_ts(),
            }),
            1 => Ok(WakeView::OnAccountChange {
                address: self.wake_account,
                offset,
                len,
            }),
            2 => Ok(WakeView::EverySlots {
                slots: self.wake_slot(),
            }),
            3 => Ok(WakeView::AtSlot {
                slot: self.wake_slot(),
            }),
            4 => Ok(WakeView::OnValueCross {
                address: self.wake_account,
                offset,
                len,
                threshold: if self.wake_value_unsigned != 0 {
                    WatchValue::Unsigned(u64::from_le_bytes(self.wake_ts))
                } else {
                    WatchValue::Signed(self.wake_ts())
                },
                cmp: self.wake_cmp,
            }),
            _ => Err(SpecError::BadWakeKind),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::sample_conditions;
    use crate::test_support::spec;

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
            ConditionV0::at_slot(500, spec(0), ResolverListV0::new(0, 0)).wake(),
            Ok(WakeView::AtSlot { slot: 500 })
        );
        let mut bad = conditions[0];
        bad.wake_kind = 9;
        assert_eq!(bad.wake(), Err(SpecError::BadWakeKind));
    }

    /// The wire fields are sealed; a wake is written by variant, and
    /// writing one clears what the previous variant used. `wake_ts` is a
    /// timestamp for one kind and a threshold for another — that overload
    /// is exactly what a caller must not have to remember.
    #[test]
    fn set_wake_replaces_the_variant_wholesale() {
        let mut c = ConditionV0::on_value_cross(
            WatchedRegion::new([7; 32], 8, 8),
            WatchValue::Signed(1234),
            1,
            spec(0),
            ResolverListV0::new(64, 1),
        );
        assert_eq!(
            c.wake(),
            Ok(WakeView::OnValueCross {
                address: [7; 32],
                offset: 8,
                len: 8,
                threshold: WatchValue::Signed(1234),
                cmp: 1,
            })
        );
        c.set_wake(WakeView::EverySlots { slots: 300 });
        assert_eq!(c.wake(), Ok(WakeView::EverySlots { slots: 300 }));
        // The threshold did not survive as a stale timestamp.
        c.set_wake(WakeView::AtTimestamp { unix_ts: 5 });
        assert_eq!(c.wake(), Ok(WakeView::AtTimestamp { unix_ts: 5 }));
    }

    /// An unsigned watched field with its top bit set must not read as
    /// negative. A u64 amount past `i64::MAX` sign-extended flips every
    /// comparison: `value >= threshold` reports not-due exactly when the
    /// value is at its largest.
    #[test]
    fn unsigned_watches_compare_in_the_unsigned_domain() {
        let big: u64 = i64::MAX as u64 + 5; // top bit set
        let bytes = big.to_le_bytes();

        // Sign-extended (the old reading) this is negative and >= fails.
        let signed = read_watched_value(&bytes, false).unwrap();
        assert!(!value_crossed(signed, WatchValue::Signed(100), 0));

        // Declared unsigned it compares correctly, in both directions.
        let unsigned = read_watched_value(&bytes, true).unwrap();
        assert_eq!(unsigned, WatchValue::Unsigned(big));
        assert!(value_crossed(unsigned, WatchValue::Unsigned(100), 0));
        assert!(!value_crossed(unsigned, WatchValue::Unsigned(u64::MAX), 0));
        assert!(value_crossed(unsigned, WatchValue::Unsigned(u64::MAX), 1));

        // And the condition round-trips the declaration through the wire.
        let c = ConditionV0::on_value_cross(
            WatchedRegion::new([7; 32], 8, 8),
            WatchValue::Unsigned(big),
            0,
            spec(0),
            ResolverListV0::new(64, 1),
        );
        let Ok(WakeView::OnValueCross { threshold, .. }) = c.wake() else {
            panic!("wrong wake kind");
        };
        assert_eq!(threshold, WatchValue::Unsigned(big));

        // Narrow unsigned widths zero-extend.
        assert_eq!(
            read_watched_value(&0xFFu8.to_le_bytes(), true),
            Some(WatchValue::Unsigned(255))
        );
        assert_eq!(
            read_watched_value(&0xFFFFu16.to_le_bytes(), true),
            Some(WatchValue::Unsigned(65_535))
        );
    }

    /// Rewriting an unsigned value-cross wake to any other variant clears
    /// the signedness flag with the rest of the wake fields.
    #[test]
    fn set_wake_clears_the_signedness_flag() {
        let mut c = ConditionV0::on_value_cross(
            WatchedRegion::new([7; 32], 8, 8),
            WatchValue::Unsigned(9),
            0,
            spec(0),
            ResolverListV0::new(64, 1),
        );
        assert_eq!(c.wake_value_unsigned, 1);
        c.set_wake(WakeView::OnValueCross {
            address: [7; 32],
            offset: 8,
            len: 8,
            threshold: WatchValue::Signed(-3),
            cmp: 0,
        });
        assert_eq!(c.wake_value_unsigned, 0);
        assert_eq!(
            c.wake().unwrap(),
            WakeView::OnValueCross {
                address: [7; 32],
                offset: 8,
                len: 8,
                threshold: WatchValue::Signed(-3),
                cmp: 0,
            }
        );
    }
}
