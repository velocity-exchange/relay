//! What makes a chain source *relay-aware*: the filters that select
//! `WatchV0` registry accounts.
//!
//! `relay-chain-source` is deliberately protocol-agnostic — it takes
//! transport-neutral [`AccountFilter`]s and translates them per transport.
//! The knowledge that a watch is `WATCH_V0_LEN` bytes, starts with
//! `WATCH_V0_DISCRIMINATOR`, and carries `target_program` at a known offset
//! lives here, and nowhere below.

use relay_chain_source::{AccountFilter, ProgramSubscription};
use solana_sdk::pubkey::Pubkey;

/// One filter set per allowed target program (a memcmp matches a single
/// value, so an allowlist of N is N provider-side queries — still far
/// cheaper than transmitting every other protocol's watches). Empty
/// `target_programs` yields one set matching every watch.
pub fn watch_filter_sets(target_programs: &[Pubkey]) -> Vec<Vec<AccountFilter>> {
    let base = || {
        vec![
            AccountFilter::DataSize(relay_spec::WATCH_V0_LEN as u64),
            AccountFilter::prefix(relay_spec::WATCH_V0_DISCRIMINATOR.to_vec()),
        ]
    };
    if target_programs.is_empty() {
        return vec![base()];
    }
    target_programs
        .iter()
        .map(|target_program| {
            let mut set = base();
            set.push(AccountFilter::Memcmp {
                offset: relay_spec::WATCH_TARGET_PROGRAM_OFFSET,
                bytes: target_program.to_bytes().to_vec(),
            });
            set
        })
        .collect()
}

/// The watch registry as a subscription: the relay program as owner, scoped
/// to `target_programs`.
pub fn watch_subscription(
    relay_program: Pubkey,
    target_programs: &[Pubkey],
) -> ProgramSubscription {
    ProgramSubscription {
        program: relay_program,
        filter_sets: watch_filter_sets(target_programs),
    }
}
