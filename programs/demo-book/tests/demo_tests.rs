//! Cross-program litesvm tests: demo-book conditions cranked the way a
//! turner would — resolver simulation → executor build → (optionally
//! crank_v0-wrapped) submission. Require the SBF build first:
//! `./scripts/build-programs.sh`.

use anchor_v2_testing::{
    Keypair, LiteSVM, Message, Signer, VersionedMessage, VersionedTransaction,
};
use demo_book::anchor_lang_v2::prelude::Address;
use demo_book::anchor_lang_v2::solana_program::instruction::{AccountMeta, Instruction};
use demo_book::anchor_lang_v2::Discriminator;
use demo_book::state::{
    BookV0, BOOK_ACCOUNT_LEN, CONDITIONS_OFFSET, CROSS_CONDITION, ENTRY_COUNT_OFFSET,
    EVICT_CONDITION, SIDE_ASK, SIDE_BID, STAGING_OFFSET, SWEEP_CONDITION,
};
use demo_book::{accounts, instruction, AddEntryArgsV0, InitializeBookArgsV0, SetPaymentArgsV0};
use litesvm::types::{FailedTransactionMetadata, TransactionMetadata};
use relay_spec as spec;
use solana_account::ReadableAccount;
use solana_clock::Clock;
use solana_pubkey::Pubkey;

const DEMO_SO: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../target/deploy/demo_book.so");
const RELAY_SO: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../target/deploy/relay.so");

const PAYMENT: u64 = 50_000;
const TREASURY: u64 = 1_000_000;
const EVICT_THRESHOLD: u32 = 3;

fn demo_id() -> Pubkey {
    "6PqZZeykcFwncPxs5LjjxzQshdRV29mpsFtmT3QS9jRZ"
        .parse()
        .unwrap()
}

fn relay_id() -> Pubkey {
    "4D5tPhw9sqkdkR5CpmP427TH6y9p9AMuKUukUEHn3Mpu"
        .parse()
        .unwrap()
}

fn addr(pk: Pubkey) -> Address {
    Address::new_from_array(pk.to_bytes())
}

struct Ctx {
    svm: LiteSVM,
    payer: Keypair,
    authority: Keypair,
    keeper: Pubkey,
    book: Pubkey,
}

fn setup() -> Ctx {
    setup_with_treasury(TREASURY)
}

fn setup_with_treasury(treasury: u64) -> Ctx {
    let mut svm = anchor_v2_testing::svm();
    svm.add_program_from_file(demo_id(), DEMO_SO)
        .expect("demo_book.so missing — run scripts/build-programs.sh first");
    svm.add_program_from_file(relay_id(), RELAY_SO)
        .expect("relay.so missing — run scripts/build-programs.sh first");

    let payer = Keypair::new();
    let authority = Keypair::new();
    svm.airdrop(&payer.pubkey(), 100_000_000_000).unwrap();
    svm.airdrop(&authority.pubkey(), 1_000_000_000).unwrap();
    let keeper = Pubkey::new_unique();
    svm.airdrop(&keeper, 1_000_000).unwrap();

    let book = Pubkey::new_unique();
    let rent = svm.minimum_balance_for_rent_exemption(BOOK_ACCOUNT_LEN);
    svm.set_account(
        book,
        solana_account::Account {
            lamports: rent + treasury,
            data: vec![0u8; BOOK_ACCOUNT_LEN],
            owner: demo_id(),
            executable: false,
            rent_epoch: 0,
        },
    )
    .unwrap();

    let mut ctx = Ctx {
        svm,
        payer,
        authority,
        keeper,
        book,
    };
    let ix = instruction::InitializeBookV0 {
        args: InitializeBookArgsV0 {
            payment_per_crank: PAYMENT,
            evict_threshold: EVICT_THRESHOLD,
        },
    }
    .to_instruction(accounts::InitializeBookV0 {
        authority: addr(ctx.authority.pubkey()),
        book: addr(ctx.book),
    });
    send(&mut ctx, ix).unwrap();
    ctx
}

#[allow(clippy::result_large_err)]
fn send(ctx: &mut Ctx, ix: Instruction) -> Result<TransactionMetadata, FailedTransactionMetadata> {
    send_all(ctx, &[ix])
}

/// Send a whole instruction list — how a turner submits a guarded crank:
/// `begin_guard_v0`, the executor itself (no CPI wrapper), `assert_paid_v0`.
#[allow(clippy::result_large_err)] // litesvm's error type; tests match on it
fn send_all(
    ctx: &mut Ctx,
    ixs: &[Instruction],
) -> Result<TransactionMetadata, FailedTransactionMetadata> {
    ctx.svm.expire_blockhash();
    let blockhash = ctx.svm.latest_blockhash();
    let msg = Message::new_with_blockhash(ixs, Some(&ctx.payer.pubkey()), &blockhash);
    let mut signers: Vec<&dyn anchor_v2_testing::Signer> = vec![&ctx.payer];
    let needs_authority = ixs
        .iter()
        .flat_map(|ix| ix.accounts.iter())
        .any(|m| m.is_signer && m.pubkey.to_bytes() == ctx.authority.pubkey().to_bytes());
    if needs_authority {
        signers.push(&ctx.authority);
    }
    let tx = VersionedTransaction::try_new(VersionedMessage::Legacy(msg), &signers).unwrap();
    ctx.svm.send_transaction(tx)
}

fn custom_error_code(failed: &FailedTransactionMetadata) -> Option<u32> {
    let debug = format!("{:?}", failed.err);
    let needle = "Custom(";
    let start = debug.find(needle)? + needle.len();
    let end = debug[start..].find(')')? + start;
    debug[start..end].parse().ok()
}

fn warp_to(ctx: &mut Ctx, unix_ts: i64) {
    let mut clock: Clock = ctx.svm.get_sysvar();
    clock.unix_timestamp = unix_ts;
    ctx.svm.set_sysvar(&clock);
}

fn now(ctx: &Ctx) -> i64 {
    let clock: Clock = ctx.svm.get_sysvar();
    clock.unix_timestamp
}

fn add_entry(ctx: &mut Ctx, expiry_ts: i64) {
    add_quote(ctx, expiry_ts, 100, demo_book::state::SIDE_BID)
}

fn add_quote(ctx: &mut Ctx, expiry_ts: i64, price: u64, side: u8) {
    let ix = instruction::AddEntryV0 {
        args: AddEntryArgsV0 {
            expiry_ts,
            price,
            side,
        },
    }
    .to_instruction(accounts::AddEntryV0 {
        book: addr(ctx.book),
        authority: addr(ctx.authority.pubkey()),
    });
    send(ctx, ix).unwrap();
}

fn read_conditions(ctx: &Ctx) -> Vec<spec::ConditionV0> {
    let data = ctx.svm.get_account(&ctx.book).unwrap().data;
    let (_, conditions) = spec::read_block(&data, CONDITIONS_OFFSET).unwrap();
    conditions.to_vec()
}

fn sweep_wake_ts(conditions: &[spec::ConditionV0]) -> i64 {
    match conditions[SWEEP_CONDITION as usize].wake().unwrap() {
        spec::WakeView::AtTimestamp { unix_ts } => unix_ts,
        other => panic!("sweep wake should be AtTimestamp, got {other:?}"),
    }
}

/// A condition's resolver account list, followed from its indirect
/// pointer into the book's account bytes — exactly what a turner does.
fn resolver_refs(ctx: &Ctx, condition: &spec::ConditionV0) -> Vec<spec::AccountRefV0> {
    let list = condition.resolvers();
    let data = ctx.svm.get_account(&ctx.book).unwrap().data;
    (0..list.count as usize)
        .map(|i| {
            let start = list.offset as usize + i * spec::ACCOUNT_REF_LEN;
            bytemuck::pod_read_unaligned(&data[start..start + spec::ACCOUNT_REF_LEN])
        })
        .collect()
}

/// Simulate a resolver and read its staged payload back out of the
/// simulation post-execution account state — exactly what a turner does.
/// Returns `None` when the resolver reports no work.
fn resolve(ctx: &mut Ctx, condition_index: u8) -> Option<spec::ResolvedCrankV0> {
    let conditions = read_conditions(ctx);
    let condition = &conditions[condition_index as usize];
    assert_eq!(
        condition.crank_spec().resolver_program,
        demo_id().to_bytes()
    );
    let refs = resolver_refs(ctx, condition);
    let metas: Vec<AccountMeta> = refs
        .iter()
        .map(|a| AccountMeta {
            pubkey: Pubkey::new_from_array(a.address),
            is_signer: false,
            is_writable: a.is_writable(),
        })
        .collect();
    // The resolver is told which condition fired — one resolver serves all
    // three, so without this it cannot answer at all.
    let ix = Instruction {
        program_id: demo_id(),
        accounts: metas,
        data: spec::encode_resolver_data(
            condition.crank_spec().resolver_disc,
            spec::FiredConditionV0::new(
                ctx.book.to_bytes(),
                CONDITIONS_OFFSET as u32,
                condition_index,
            ),
        )
        .to_vec(),
    };
    ctx.svm.expire_blockhash();
    let blockhash = ctx.svm.latest_blockhash();
    let msg = Message::new_with_blockhash(
        std::slice::from_ref(&ix),
        Some(&ctx.payer.pubkey()),
        &blockhash,
    );
    let tx = VersionedTransaction::try_new(VersionedMessage::Legacy(msg), &[&ctx.payer]).unwrap();
    let info = ctx
        .svm
        .simulate_transaction(tx)
        .expect("resolver simulates");

    let pointer = spec::ResponsePointerV0::read(&info.meta.return_data.data).unwrap();
    if !pointer.has_work() {
        return None;
    }
    // The staged bytes live in post-simulation state, never on chain.
    let staged = info
        .post_accounts
        .iter()
        .find(|(address, _)| address.to_bytes() == refs[pointer.account_index as usize].address)
        .map(|(_, account)| account.data().to_vec())
        .expect("staging account returned by simulation");
    let start = pointer.offset() as usize;
    let end = start + pointer.len() as usize;
    Some(spec::ResolvedCrankV0::read(&staged[start..end]).unwrap())
}

/// First bytes of the staging region as committed on chain.
fn staging_bytes_on_chain(ctx: &Ctx) -> Vec<u8> {
    ctx.svm.get_account(&ctx.book).unwrap().data[STAGING_OFFSET..STAGING_OFFSET + 64].to_vec()
}

/// Build the executor account metas from a resolver's output, substituting
/// the keeper placeholder. Returns (metas, keeper_index).
fn executor_metas(resolved: &spec::ResolvedCrankV0, keeper: Pubkey) -> (Vec<AccountMeta>, u8) {
    let mut keeper_index = None;
    let metas = resolved
        .accounts
        .iter()
        .enumerate()
        .map(|(i, a)| {
            let pubkey = if a.address == spec::KEEPER_PLACEHOLDER {
                keeper_index.get_or_insert(i as u8);
                keeper
            } else {
                Pubkey::new_from_array(a.address)
            };
            AccountMeta {
                pubkey,
                is_signer: false,
                is_writable: a.is_writable(),
            }
        })
        .collect();
    (metas, keeper_index.expect("resolver output names a keeper"))
}

/// Unwrapped executor submission.
fn executor_ix(ctx: &Ctx, resolved: &spec::ResolvedCrankV0) -> Instruction {
    executor_ix_for(ctx, resolved, ctx.keeper)
}

/// The keeper's guard PDA, derived exactly as the turner derives it.
fn guard_address(keeper: Pubkey, nonce: u8) -> Pubkey {
    Pubkey::find_program_address(&[spec::GUARD_SEED, keeper.as_ref(), &[nonce]], &relay_id()).0
}

/// A guarded crank, built the way the turner builds it: the executor goes
/// in directly (no CPI), bracketed by relay's payment guards.
///
/// `payer` signs and funds the guard account; `payout` receives the
/// payment and never signs — the separation that keeps a hostile executor
/// away from the signing key.
fn guarded_crank(
    ctx: &Ctx,
    payer: Pubkey,
    payout: Pubkey,
    resolved: &spec::ResolvedCrankV0,
    min_payment: u64,
) -> Vec<Instruction> {
    let nonce = 0u8;
    let guard = guard_address(payout, nonce);
    vec![
        Instruction {
            program_id: relay_id(),
            accounts: vec![
                AccountMeta::new(payer, true),
                AccountMeta::new_readonly(payout, false),
                AccountMeta::new(guard, false),
                AccountMeta::new_readonly(Pubkey::new_from_array([0u8; 32]), false),
            ],
            data: spec::encode_begin_guard_v0_data(nonce).to_vec(),
        },
        executor_ix_for(ctx, resolved, payout),
        Instruction {
            program_id: relay_id(),
            accounts: vec![
                AccountMeta::new_readonly(payout, false),
                AccountMeta::new(guard, false),
            ],
            data: spec::encode_assert_paid_v0_data(min_payment, nonce).to_vec(),
        },
    ]
}

/// Executor instruction with an explicit keeper substituted in.
///
/// Which instruction to run comes entirely from the resolver's staged
/// payload — program, discriminator, accounts, args — so nothing here reads
/// the condition.
fn executor_ix_for(_ctx: &Ctx, resolved: &spec::ResolvedCrankV0, keeper: Pubkey) -> Instruction {
    let (metas, _) = executor_metas(resolved, keeper);
    let mut data = resolved.executor_disc.to_vec();
    data.extend_from_slice(&resolved.data);
    Instruction {
        program_id: Pubkey::new_from_array(resolved.executor_program),
        accounts: metas,
        data,
    }
}

fn keeper_balance(ctx: &Ctx) -> u64 {
    ctx.svm.get_balance(&ctx.keeper).unwrap()
}

// --- condition block contents ---

#[test]
fn init_writes_valid_condition_block() {
    let ctx = setup();
    let conditions = read_conditions(&ctx);
    assert_eq!(conditions.len(), 3);

    let sweep = &conditions[SWEEP_CONDITION as usize];
    assert!(sweep.is_active());
    assert_eq!(sweep_wake_ts(&conditions), i64::MAX);
    assert_eq!(sweep.min_payment(), PAYMENT);
    assert_eq!(sweep.crank_spec().resolver_program, demo_id().to_bytes());
    // Every condition names the same resolver; the executor is the
    // resolver's answer, not part of the block.
    assert_eq!(
        &sweep.crank_spec().resolver_disc[..],
        instruction::ResolveV0::DISCRIMINATOR
    );
    assert_eq!(resolver_refs(&ctx, sweep).len(), 1);
    assert_eq!(resolver_refs(&ctx, sweep)[0].address, ctx.book.to_bytes());

    let evict = &conditions[EVICT_CONDITION as usize];
    assert!(evict.is_active());
    match evict.wake().unwrap() {
        spec::WakeView::OnAccountChange {
            address,
            offset,
            len,
        } => {
            assert_eq!(address, ctx.book.to_bytes());
            assert_eq!(offset as usize, ENTRY_COUNT_OFFSET);
            assert_eq!(len, 4);
        }
        other => panic!("evict wake should be OnAccountChange, got {other:?}"),
    }
    assert_eq!(
        &evict.crank_spec().resolver_disc[..],
        instruction::ResolveV0::DISCRIMINATOR
    );
}

#[test]
fn add_entry_maintains_min_over_inserts_hint() {
    let mut ctx = setup();
    let t = now(&ctx);

    add_entry(&mut ctx, t + 1000);
    assert_eq!(sweep_wake_ts(&read_conditions(&ctx)), t + 1000);

    // Later expiry: hint unchanged.
    add_entry(&mut ctx, t + 5000);
    assert_eq!(sweep_wake_ts(&read_conditions(&ctx)), t + 1000);

    // Earlier expiry: hint lowered.
    add_entry(&mut ctx, t + 100);
    assert_eq!(sweep_wake_ts(&read_conditions(&ctx)), t + 100);
}

// --- sweep: resolver → executor round trip ---

#[test]
fn resolve_sweep_no_work_when_nothing_expired() {
    let mut ctx = setup();
    let t = now(&ctx);
    add_entry(&mut ctx, t + 1000);
    assert!(resolve(&mut ctx, SWEEP_CONDITION).is_none());
}

#[test]
fn sweep_via_resolver_roundtrip() {
    let mut ctx = setup();
    let t = now(&ctx);
    add_entry(&mut ctx, t + 100); // id 1 — will expire
    add_entry(&mut ctx, t + 200); // id 2 — will expire
    add_entry(&mut ctx, t + 9000); // id 3 — stays

    warp_to(&mut ctx, t + 300);
    let resolved = resolve(&mut ctx, SWEEP_CONDITION).expect("work");
    // The payload, not the condition, is what names the instruction to run.
    assert_eq!(resolved.executor_program, demo_id().to_bytes());
    assert_eq!(
        &resolved.executor_disc[..],
        instruction::SweepV0::DISCRIMINATOR
    );

    let before = keeper_balance(&ctx);
    let ix = executor_ix(&ctx, &resolved);
    send(&mut ctx, ix).unwrap();
    assert_eq!(keeper_balance(&ctx), before + PAYMENT);

    // Entries 1 and 2 gone; hint repaired to the true minimum (t + 9000 —
    // not stuck at the swept t + 100).
    assert_eq!(sweep_wake_ts(&read_conditions(&ctx)), t + 9000);
    assert!(
        resolve(&mut ctx, SWEEP_CONDITION).is_none(),
        "backlog should be drained"
    );
}

#[test]
fn sweep_rejects_bad_ids() {
    let mut ctx = setup();
    let t = now(&ctx);
    add_entry(&mut ctx, t + 100);

    // Unexpired.
    let ix = executor_ix(&ctx, &sweep_payload(&ctx, &[1]));
    let failed = send(&mut ctx, ix).unwrap_err();
    assert_eq!(custom_error_code(&failed), Some(6002)); // EntryNotExpired

    // Unknown id.
    warp_to(&mut ctx, t + 200);
    let ix = executor_ix(&ctx, &sweep_payload(&ctx, &[42]));
    let failed = send(&mut ctx, ix).unwrap_err();
    assert_eq!(custom_error_code(&failed), Some(6001)); // EntryNotFound

    // Empty ids.
    let ix = executor_ix(&ctx, &sweep_payload(&ctx, &[]));
    let failed = send(&mut ctx, ix).unwrap_err();
    assert_eq!(custom_error_code(&failed), Some(6003)); // NothingToSweep
}

/// One resolver, three conditions: which executor comes back depends only
/// on which condition the turner says fired.
#[test]
fn one_resolver_answers_for_each_condition() {
    let mut ctx = setup();
    let t = now(&ctx);
    // An expired bid that also crosses a live ask, so sweep and cross both
    // have work at the same time.
    add_quote(&mut ctx, t + 100, 100, SIDE_BID);
    add_quote(&mut ctx, t + 9000, 90, SIDE_ASK);
    add_quote(&mut ctx, t + 9000, 80, SIDE_ASK); // third entry hits the evict threshold
    warp_to(&mut ctx, t + 300);

    let expected = [
        (SWEEP_CONDITION, instruction::SweepV0::DISCRIMINATOR),
        (EVICT_CONDITION, instruction::EvictV0::DISCRIMINATOR),
        (CROSS_CONDITION, instruction::CrossV0::DISCRIMINATOR),
    ];
    expected.iter().for_each(|(index, disc)| {
        let resolved = resolve(&mut ctx, *index).expect("work for condition {index}");
        assert_eq!(
            &resolved.executor_disc[..],
            *disc,
            "condition {index} resolved to the wrong executor"
        );
        assert_eq!(resolved.executor_program, demo_id().to_bytes());
    });
}

/// The fired identity is an argument, so the resolver checks it rather than
/// trusting it: a condition this book does not serve is refused outright
/// instead of staging an answer about something else.
#[test]
fn resolver_refuses_a_condition_it_does_not_serve() {
    let mut ctx = setup();
    let t = now(&ctx);
    add_entry(&mut ctx, t + 100);
    warp_to(&mut ctx, t + 300);

    let book = ctx.book;
    // A slot index past the block.
    let ix = resolve_ix(book, book, CONDITIONS_OFFSET as u32, 7);
    let failed = send(&mut ctx, ix).expect_err("index 7 is not a condition this book has");
    assert_eq!(custom_error_code(&failed), Some(6010)); // UnknownCondition

    // The right index, but pointed at another account's block.
    let ix = resolve_ix(
        book,
        Pubkey::new_unique(),
        CONDITIONS_OFFSET as u32,
        SWEEP_CONDITION,
    );
    let failed = send(&mut ctx, ix).expect_err("the target must be the book actually held");
    assert_eq!(custom_error_code(&failed), Some(6010));

    // And the right index at the wrong offset.
    let ix = resolve_ix(book, book, 0, SWEEP_CONDITION);
    let failed = send(&mut ctx, ix).expect_err("offset 0 is not where this book's block lives");
    assert_eq!(custom_error_code(&failed), Some(6010));

    // The honest coordinates still resolve.
    let ix = resolve_ix(book, book, CONDITIONS_OFFSET as u32, SWEEP_CONDITION);
    send(&mut ctx, ix).expect("the real condition resolves");
}

/// A `resolve_v0` instruction with arbitrary fired-condition coordinates.
fn resolve_ix(book: Pubkey, target: Pubkey, block_offset: u32, index: u8) -> Instruction {
    Instruction {
        program_id: demo_id(),
        accounts: vec![AccountMeta::new(book, false)],
        data: spec::encode_resolver_data(
            instruction::ResolveV0::DISCRIMINATOR
                .try_into()
                .expect("8-byte discriminator"),
            spec::FiredConditionV0::new(target.to_bytes(), block_offset, index),
        )
        .to_vec(),
    }
}

/// A resolver payload built by hand, for the cases a resolver would never
/// produce: the executor identity is part of the payload now, so a test
/// naming bad args names the instruction too.
fn sweep_payload(ctx: &Ctx, ids: &[u64]) -> spec::ResolvedCrankV0 {
    BookV0::resolved(
        &addr(ctx.book),
        instruction::SweepV0::DISCRIMINATOR,
        sweep_args_wire(ids),
    )
}

fn sweep_args_wire(ids: &[u64]) -> Vec<u8> {
    (ids.len() as u32)
        .to_le_bytes()
        .into_iter()
        .chain(ids.iter().flat_map(|id| id.to_le_bytes()))
        .collect()
}

#[test]
fn cancel_leaves_hint_early_and_sweep_noops() {
    let mut ctx = setup();
    let t = now(&ctx);
    add_entry(&mut ctx, t + 100); // id 1
    add_entry(&mut ctx, t + 9000); // id 2

    let ix = instruction::CancelEntryV0 {
        args: demo_book::CancelEntryArgsV0 { id: 1 },
    }
    .to_instruction(accounts::CancelEntryV0 {
        book: addr(ctx.book),
        authority: addr(ctx.authority.pubkey()),
    });
    send(&mut ctx, ix).unwrap();

    // Hint deliberately not repaired: still t + 100 (stale-early).
    assert_eq!(sweep_wake_ts(&read_conditions(&ctx)), t + 100);

    // Past the stale hint the resolver says no-work — the turner's
    // simulation filters the wake, no transaction lands.
    warp_to(&mut ctx, t + 200);
    assert!(resolve(&mut ctx, SWEEP_CONDITION).is_none());
}

// --- payment guards ---

#[test]
fn guarded_crank_happy_path() {
    let mut ctx = setup();
    let t = now(&ctx);
    add_entry(&mut ctx, t + 100);
    warp_to(&mut ctx, t + 200);

    // Payer signs; payout is a separate account that never does.
    let payer = ctx.payer.pubkey();
    let keeper = ctx.keeper;

    // First guarded crank also creates the guard account, so the keeper
    // pays its (one-time) rent on top of the fee.
    let resolved = resolve(&mut ctx, SWEEP_CONDITION).expect("work");
    let ixs = guarded_crank(&ctx, payer, keeper, &resolved, PAYMENT);
    send_all(&mut ctx, &ixs).unwrap();
    assert_eq!(entry_count(&ctx), 0);

    // Steady state: the guard account already exists, so a crank nets the
    // payment minus the transaction fee.
    add_entry(&mut ctx, t + 300);
    warp_to(&mut ctx, t + 400);
    let resolved = resolve(&mut ctx, SWEEP_CONDITION).expect("work");
    let before = ctx.svm.get_balance(&keeper).unwrap();
    let ixs = guarded_crank(&ctx, payer, keeper, &resolved, PAYMENT);
    send_all(&mut ctx, &ixs).unwrap();
    let after = ctx.svm.get_balance(&keeper).unwrap();
    assert!(
        after > before,
        "keeper should net the payment minus the fee: {before} -> {after}"
    );
    assert!(after - before <= PAYMENT);
    assert_eq!(entry_count(&ctx), 0);
}

#[test]
fn guard_reverts_underpayment() {
    let mut ctx = setup();
    let t = now(&ctx);
    add_entry(&mut ctx, t + 100);
    warp_to(&mut ctx, t + 200);
    let payer = ctx.payer.pubkey();
    let keeper = ctx.keeper;

    // Arm the guard account once so its rent is not part of this test.
    let resolved = resolve(&mut ctx, SWEEP_CONDITION).expect("work");
    let ixs = guarded_crank(&ctx, payer, keeper, &resolved, PAYMENT);
    send_all(&mut ctx, &ixs).unwrap();

    // Now the book pays less than the turner asserts.
    add_entry(&mut ctx, t + 300);
    warp_to(&mut ctx, t + 400);
    let ix = instruction::SetPaymentV0 {
        args: SetPaymentArgsV0 {
            payment_per_crank: PAYMENT / 2,
        },
    }
    .to_instruction(accounts::SetPaymentV0 {
        book: addr(ctx.book),
        authority: addr(ctx.authority.pubkey()),
    });
    send(&mut ctx, ix).unwrap();

    let resolved = resolve(&mut ctx, SWEEP_CONDITION).expect("work");
    let entries_before = entry_count(&ctx);
    let ixs = guarded_crank(&ctx, payer, keeper, &resolved, PAYMENT);
    let failed = send_all(&mut ctx, &ixs).unwrap_err();
    assert_eq!(
        custom_error_code(&failed),
        Some(6000), // InsufficientKeeperPayment
        "expected InsufficientKeeperPayment, got {:?}",
        failed.err
    );
    // The whole transaction reverted, executor work included.
    assert_eq!(entry_count(&ctx), entries_before);

    // Unguarded, the same executor succeeds — the guard is what refuses.
    let ix = executor_ix_for(&ctx, &resolved, keeper);
    send(&mut ctx, ix).unwrap();
    assert!(entry_count(&ctx) < entries_before);
}

#[test]
fn assert_paid_requires_an_armed_guard() {
    let mut ctx = setup();
    let t = now(&ctx);
    add_entry(&mut ctx, t + 100);
    warp_to(&mut ctx, t + 200);
    let payer = ctx.payer.pubkey();
    let keeper = ctx.keeper;

    // Arm and consume a guard so the account exists but is disarmed.
    let resolved = resolve(&mut ctx, SWEEP_CONDITION).expect("work");
    let ixs = guarded_crank(&ctx, payer, keeper, &resolved, PAYMENT);
    send_all(&mut ctx, &ixs).unwrap();

    // A trailing guard with no matching arm must fail rather than measure
    // against stale state.
    let ixs = guarded_crank(&ctx, payer, keeper, &resolved, PAYMENT);
    let trailing_only = vec![ixs[2].clone()];
    let failed = send_all(&mut ctx, &trailing_only).unwrap_err();
    assert_eq!(custom_error_code(&failed), Some(6001)); // GuardNotArmed
}

#[test]
fn guard_measures_only_this_transaction() {
    let mut ctx = setup();
    let t = now(&ctx);
    add_entry(&mut ctx, t + 100);
    warp_to(&mut ctx, t + 200);
    let payer = ctx.payer.pubkey();
    let keeper = ctx.keeper;

    // Asserting more than the book pays fails even though the keeper's
    // absolute balance is enormous — the guard measures the delta, not the
    // balance.
    let resolved = resolve(&mut ctx, SWEEP_CONDITION).expect("work");
    // Make the payout rich, so only a delta measurement can fail.
    ctx.svm.airdrop(&keeper, 100_000_000_000).unwrap();
    assert!(ctx.svm.get_balance(&keeper).unwrap() > PAYMENT * 1000);
    let ixs = guarded_crank(&ctx, payer, keeper, &resolved, PAYMENT + 1);
    let failed = send_all(&mut ctx, &ixs).unwrap_err();
    assert_eq!(custom_error_code(&failed), Some(6000));
}

// --- evict: change condition ---

#[test]
fn evict_flow() {
    let mut ctx = setup();
    let t = now(&ctx);

    // Below threshold: resolver reports no work, direct evict fails.
    add_entry(&mut ctx, t + 100); // id 1 (oldest)
    add_entry(&mut ctx, t + 200); // id 2
    assert!(resolve(&mut ctx, EVICT_CONDITION).is_none());

    let ix = executor_ix(
        &ctx,
        &BookV0::resolved(
            &addr(ctx.book),
            instruction::EvictV0::DISCRIMINATOR,
            1u64.to_le_bytes().to_vec(),
        ),
    );
    let failed = send(&mut ctx, ix).unwrap_err();
    assert_eq!(custom_error_code(&failed), Some(6004)); // BelowEvictThreshold

    // At threshold: resolver picks the oldest entry; crank it wrapped.
    add_entry(&mut ctx, t + 300); // id 3 — entry_count hits EVICT_THRESHOLD
    let resolved = resolve(&mut ctx, EVICT_CONDITION).expect("work");
    assert_eq!(&resolved.data, &1u64.to_le_bytes()); // victim = oldest id

    let before = keeper_balance(&ctx);
    let ix = executor_ix(&ctx, &resolved);
    send(&mut ctx, ix).unwrap();
    assert_eq!(keeper_balance(&ctx), before + PAYMENT);

    // Back below threshold.
    assert!(resolve(&mut ctx, EVICT_CONDITION).is_none());
}

// --- treasury floor ---

#[test]
fn sweep_fails_when_book_cannot_pay() {
    let mut ctx = setup_with_treasury(0);
    let t = now(&ctx);
    add_entry(&mut ctx, t + 100);
    warp_to(&mut ctx, t + 200);

    let resolved = resolve(&mut ctx, SWEEP_CONDITION).expect("work");
    let ix = executor_ix(&ctx, &resolved);
    let failed = send(&mut ctx, ix).unwrap_err();
    assert_eq!(custom_error_code(&failed), Some(6005)); // InsufficientTreasury
}

// --- staging ---

/// The whole point of staging over return data: the payload can exceed the
/// 1024-byte return-data cap, while what actually rides return data is a
/// 10-byte pointer.
#[test]
fn staged_payload_can_exceed_return_data_cap() {
    let mut ctx = setup();
    let t = now(&ctx);
    // A full sweep batch: 8 ids plus the executor account list.
    (0..demo_book::state::MAX_SWEEP_IDS).for_each(|i| add_entry(&mut ctx, t + i as i64));
    warp_to(&mut ctx, t + 1000);

    let resolved = resolve(&mut ctx, SWEEP_CONDITION).expect("work");
    assert_eq!(resolved.data.len(), 4 + 8 * demo_book::state::MAX_SWEEP_IDS);
    assert_eq!(
        spec::ResponsePointerV0::new(0, 0, 0).to_bytes().len(),
        spec::RESPONSE_POINTER_LEN,
        "the pointer stays tiny no matter how large the payload is"
    );

    // And the batch actually cranks.
    let before = keeper_balance(&ctx);
    let ix = executor_ix(&ctx, &resolved);
    send(&mut ctx, ix).unwrap();
    assert_eq!(keeper_balance(&ctx), before + PAYMENT);
    assert_eq!(entry_count(&ctx), 0);
}

/// Resolvers are only ever simulated, so their staging writes must never
/// reach chain state.
#[test]
fn staging_write_never_lands_on_chain() {
    let mut ctx = setup();
    let t = now(&ctx);
    add_entry(&mut ctx, t + 100);
    warp_to(&mut ctx, t + 200);

    let before = staging_bytes_on_chain(&ctx);
    assert!(before.iter().all(|b| *b == 0), "staging starts zeroed");
    let resolved = resolve(&mut ctx, SWEEP_CONDITION).expect("work");
    assert!(!resolved.accounts.is_empty());
    assert_eq!(
        staging_bytes_on_chain(&ctx),
        before,
        "simulation must not mutate committed state"
    );
}

fn entry_count(ctx: &Ctx) -> u32 {
    let data = ctx.svm.get_account(&ctx.book).unwrap().data;
    u32::from_le_bytes(
        data[ENTRY_COUNT_OFFSET..ENTRY_COUNT_OFFSET + 4]
            .try_into()
            .unwrap(),
    )
}

// --- cross: any change to the book ---

#[test]
fn cross_condition_watches_the_version_counter() {
    let ctx = setup();
    let conditions = read_conditions(&ctx);
    let cross = &conditions[CROSS_CONDITION as usize];
    match cross.wake().unwrap() {
        spec::WakeView::OnAccountChange {
            address,
            offset,
            len,
        } => {
            assert_eq!(address, ctx.book.to_bytes());
            assert_eq!(offset as usize, demo_book::state::VERSION_OFFSET);
            assert_eq!(len, 8, "eight bytes stand in for the whole book changing");
        }
        other => panic!("cross wake should be OnAccountChange, got {other:?}"),
    }
}

#[test]
fn every_mutation_bumps_the_version() {
    let mut ctx = setup();
    let t = now(&ctx);
    let version = |ctx: &Ctx| {
        let data = ctx.svm.get_account(&ctx.book).unwrap().data;
        u64::from_le_bytes(
            data[demo_book::state::VERSION_OFFSET..demo_book::state::VERSION_OFFSET + 8]
                .try_into()
                .unwrap(),
        )
    };
    let start = version(&ctx);
    add_quote(&mut ctx, t + 1000, 100, SIDE_BID);
    let after_insert = version(&ctx);
    assert!(after_insert > start, "insert must bump the version");

    let ix = instruction::CancelEntryV0 {
        args: demo_book::CancelEntryArgsV0 { id: 1 },
    }
    .to_instruction(accounts::CancelEntryV0 {
        book: addr(ctx.book),
        authority: addr(ctx.authority.pubkey()),
    });
    send(&mut ctx, ix).unwrap();
    assert!(version(&ctx) > after_insert, "cancel must bump the version");
}

#[test]
fn uncrossed_book_resolves_to_no_work() {
    let mut ctx = setup();
    let t = now(&ctx);
    add_quote(&mut ctx, t + 1000, 100, SIDE_BID);
    add_quote(&mut ctx, t + 1000, 110, SIDE_ASK); // bid < ask
    assert!(resolve(&mut ctx, CROSS_CONDITION).is_none());
}

#[test]
fn crossed_book_is_matched() {
    let mut ctx = setup();
    let t = now(&ctx);
    add_quote(&mut ctx, t + 1000, 90, SIDE_BID); // id 1, worse bid
    add_quote(&mut ctx, t + 1000, 105, SIDE_BID); // id 2, best bid
    add_quote(&mut ctx, t + 1000, 100, SIDE_ASK); // id 3, best ask
    add_quote(&mut ctx, t + 1000, 120, SIDE_ASK); // id 4, worse ask

    let resolved = resolve(&mut ctx, CROSS_CONDITION).expect("book is crossed");
    // The resolver picks the best pair: bid 105 against ask 100.
    assert_eq!(&resolved.data[..8], &2u64.to_le_bytes());
    assert_eq!(&resolved.data[8..], &3u64.to_le_bytes());

    let before = keeper_balance(&ctx);
    let ix = executor_ix(&ctx, &resolved);
    send(&mut ctx, ix).unwrap();
    assert_eq!(keeper_balance(&ctx), before + PAYMENT);
    assert_eq!(entry_count(&ctx), 2, "the matched pair is gone");

    // What is left no longer crosses, so the re-fired wake finds nothing.
    assert!(resolve(&mut ctx, CROSS_CONDITION).is_none());
}

#[test]
fn cross_rejects_a_pair_that_does_not_cross() {
    let mut ctx = setup();
    let t = now(&ctx);
    add_quote(&mut ctx, t + 1000, 90, SIDE_BID); // id 1
    add_quote(&mut ctx, t + 1000, 100, SIDE_ASK); // id 2

    // A stale turner naming an uncrossed pair must be refused on chain.
    let ix = executor_ix(
        &ctx,
        &BookV0::resolved(
            &addr(ctx.book),
            instruction::CrossV0::DISCRIMINATOR,
            1u64.to_le_bytes()
                .into_iter()
                .chain(2u64.to_le_bytes())
                .collect(),
        ),
    );
    let failed = send(&mut ctx, ix).unwrap_err();
    assert_eq!(custom_error_code(&failed), Some(6009)); // NotCrossing
    assert_eq!(entry_count(&ctx), 2);
}
