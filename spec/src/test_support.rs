//! Fixtures several modules' tests build on.

use alloc::vec;
use alloc::vec::Vec;

use crate::condition::{ConditionV0, CrankSpecV0};
use crate::resolver::ResolverListV0;

pub fn spec(min_payment: u64) -> CrankSpecV0 {
    CrankSpecV0 {
        resolver_program: [1; 32],
        resolver_disc: [9; 8],
        min_payment,
    }
}

pub fn sample_conditions() -> Vec<ConditionV0> {
    vec![
        ConditionV0::at_timestamp(i64::MAX, spec(5000), ResolverListV0::new(64, 1)),
        ConditionV0::on_account_change(
            crate::condition::WatchedRegion::new([3; 32], 48, 4),
            spec(1),
            ResolverListV0::new(64, 1),
        ),
        ConditionV0::every_slots(300, spec(0), ResolverListV0::new(0, 0)),
    ]
}
