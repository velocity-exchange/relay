//! Unit tests for the parts of the loop that are pure functions of
//! their inputs. The behaviour that needs a cluster lives in
//! `crank-turner/tests`.

use super::crank::Prepared;
use super::decide::watched_accounts;
use super::pack::group_by_program;
use super::payment::required_payment;
use super::safety::is_own_guard;
use super::*;

use relay_spec as spec;
use solana_sdk::instruction::Instruction;

/// What a turner needs is the fee it is about to pay, not the number the
/// target program wrote down. Bidding nothing still costs the signature.
#[test]
fn required_payment_tracks_the_fee_being_bid() {
    // No priority bid: the base fee alone.
    assert_eq!(required_payment(0, 200_000, 0), 5_000);
    // A bid is priced per compute unit, in micro-lamports.
    assert_eq!(required_payment(50_000, 200_000, 0), 5_000 + 10_000);
    // Rounded up, so a fraction of a lamport is never eaten by the turner.
    assert_eq!(required_payment(1, 1, 0), 5_001);
    // Margin rides on top.
    assert_eq!(required_payment(0, 200_000, 2_500), 7_500);
    // An absurd bid saturates rather than wrapping into a small number,
    // which would silently accept work that pays nothing.
    assert_eq!(required_payment(u64::MAX, u32::MAX, u64::MAX), u64::MAX);
}

fn prepared(program: Pubkey, index: u8) -> Prepared {
    Prepared {
        key: (Pubkey::new_unique(), 0, index),
        program,
        min_payment: 1,
        units: 1,
        ixs: Vec::new(),
    }
}

/// One account watched by several conditions is fetched once.
///
/// This is the ordinary shape, not a corner: a book is watched for its
/// order counts and again for its best prices. A watched account is
/// whole-account sized, so a duplicate in the list is a second copy of
/// the whole book on every tick.
#[test]
fn an_account_watched_twice_is_fetched_once() {
    let book = Pubkey::new_unique();
    let elsewhere = Pubkey::new_unique();
    let watch = |address: Pubkey, offset: u32| {
        spec::ConditionV0::on_account_change(
            spec::WatchedRegion::new(address.to_bytes(), offset, 8),
            spec::CrankSpecV0 {
                resolver_program: Pubkey::new_unique().to_bytes(),
                resolver_disc: [0; 8],
                min_payment: 0,
            },
            spec::ResolverListV0::new(0, 0),
        )
    };
    // Two watches on the book at different offsets, plus one elsewhere.
    let conditions = [watch(book, 0), watch(book, 64), watch(elsewhere, 0)];

    let fetched = watched_accounts(conditions.iter(), &HashMap::new());
    assert_eq!(fetched.len(), 2, "the book was listed twice: {fetched:?}");
    assert!(fetched.contains(&book));
    assert!(fetched.contains(&elsewhere));

    // An account the tick already holds is not fetched again at all.
    let loaded = HashMap::from([(book, Account::default())]);
    assert_eq!(
        watched_accounts(conditions.iter(), &loaded),
        vec![elsewhere]
    );
}

/// A transaction has one outcome and the submitter books it against one
/// program, so cranks from two programs must never share one — however
/// they interleave coming out of the concurrent phase.
#[test]
fn cranks_are_grouped_by_program_before_packing() {
    let (a, b) = (Pubkey::new_unique(), Pubkey::new_unique());
    let interleaved = vec![
        prepared(a, 0),
        prepared(b, 1),
        prepared(a, 2),
        prepared(b, 3),
        prepared(a, 4),
    ];
    let groups = group_by_program(interleaved);
    assert_eq!(groups.len(), 2);
    assert!(groups
        .iter()
        .all(|group| group.iter().all(|p| p.program == group[0].program)));
    // Order within a program is the order they finished in.
    assert_eq!(
        groups[0].iter().map(|p| p.key.2).collect::<Vec<_>>(),
        vec![0, 2, 4]
    );
    assert_eq!(
        groups[1].iter().map(|p| p.key.2).collect::<Vec<_>>(),
        vec![1, 3]
    );
}

/// The guard exemption in `signer_leak` is keyed on the instruction, not
/// the program: a resolver may name relay as an executor like anything
/// else, and `begin_guard_v0` spends the fee payer's lamports.
#[test]
fn only_the_guard_instructions_are_exempt() {
    let relay = Pubkey::new_unique();
    let guard = |data: Vec<u8>| Instruction {
        program_id: relay,
        accounts: Vec::new(),
        data,
    };
    assert!(is_own_guard(
        &guard(spec::encode_begin_guard_v0_data(3).to_vec()),
        &relay
    ));
    assert!(is_own_guard(
        &guard(spec::encode_assert_paid_v0_data(5, 3).to_vec()),
        &relay
    ));
    assert!(!is_own_guard(&guard(vec![9; 8]), &relay));
    assert!(!is_own_guard(&guard(Vec::new()), &relay));
    assert!(!is_own_guard(
        &guard(spec::encode_begin_guard_v0_data(3).to_vec()),
        &Pubkey::new_unique()
    ));
}
