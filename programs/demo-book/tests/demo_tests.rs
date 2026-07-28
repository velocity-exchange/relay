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
    BookV0, BOOK_ACCOUNT_LEN, CONDITIONS_OFFSET, ENTRY_COUNT_OFFSET, EVICT_CONDITION,
    STAGING_OFFSET, SWEEP_CONDITION,
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

#[allow(clippy::result_large_err)] // litesvm's error type; tests match on it
fn send(ctx: &mut Ctx, ix: Instruction) -> Result<TransactionMetadata, FailedTransactionMetadata> {
    ctx.svm.expire_blockhash();
    let blockhash = ctx.svm.latest_blockhash();
    let msg = Message::new_with_blockhash(
        std::slice::from_ref(&ix),
        Some(&ctx.payer.pubkey()),
        &blockhash,
    );
    let mut signers: Vec<&dyn anchor_v2_testing::Signer> = vec![&ctx.payer];
    let needs_authority = ix
        .accounts
        .iter()
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
    let ix = instruction::AddEntryV0 {
        args: AddEntryArgsV0 { expiry_ts },
    }
    .to_instruction(accounts::AddEntryV0 {
        book: addr(ctx.book),
        authority: addr(ctx.authority.pubkey()),
    });
    send(ctx, ix).unwrap();
}

fn read_conditions(ctx: &Ctx) -> Vec<spec::ConditionV0> {
    let data = ctx.svm.get_account(&ctx.book).unwrap().data;
    spec::read_conditions_unaligned(&data, CONDITIONS_OFFSET).unwrap()
}

fn write_conditions(ctx: &mut Ctx, conditions: &[spec::ConditionV0]) {
    let mut account = ctx.svm.get_account(&ctx.book).unwrap();
    spec::write_block(&mut account.data[CONDITIONS_OFFSET..], conditions).unwrap();
    ctx.svm.set_account(ctx.book, account).unwrap();
}

fn sweep_wake_ts(conditions: &[spec::ConditionV0]) -> i64 {
    match conditions[SWEEP_CONDITION as usize].wake().unwrap() {
        spec::WakeView::AtTimestamp { unix_ts } => unix_ts,
        other => panic!("sweep wake should be AtTimestamp, got {other:?}"),
    }
}

/// Simulate a resolver and read its staged payload back out of the
/// simulation post-execution account state — exactly what a turner does.
/// Returns `None` when the resolver reports no work.
fn resolve(ctx: &mut Ctx, condition_index: u8) -> Option<spec::ResolvedCrankV0> {
    let conditions = read_conditions(ctx);
    let condition = &conditions[condition_index as usize];
    assert_eq!(condition.resolver_program, demo_id().to_bytes());
    let metas: Vec<AccountMeta> = condition
        .resolver_accounts()
        .iter()
        .map(|a| AccountMeta {
            pubkey: Pubkey::new_from_array(a.address),
            is_signer: false,
            is_writable: a.is_writable(),
        })
        .collect();
    let ix = Instruction {
        program_id: demo_id(),
        accounts: metas,
        data: condition.resolver_disc.to_vec(),
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
        .find(|(address, _)| {
            address.to_bytes()
                == condition.resolver_accounts()[pointer.account_index as usize].address
        })
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
fn executor_ix(ctx: &Ctx, condition_index: u8, resolved: &spec::ResolvedCrankV0) -> Instruction {
    let conditions = read_conditions(ctx);
    let condition = &conditions[condition_index as usize];
    let (metas, _) = executor_metas(resolved, ctx.keeper);
    let mut data = condition.executor_disc.to_vec();
    data.extend_from_slice(&resolved.data);
    Instruction {
        program_id: Pubkey::new_from_array(condition.executor_program),
        accounts: metas,
        data,
    }
}

/// crank_v0-wrapped executor submission, built exactly as the turner builds
/// it (spec encoder, not the program's generated client).
fn crank_ix(ctx: &Ctx, condition_index: u8, resolved: &spec::ResolvedCrankV0) -> Instruction {
    let (exec_metas, keeper_index) = executor_metas(resolved, ctx.keeper);
    let mut accounts = vec![
        AccountMeta {
            pubkey: ctx.book,
            is_signer: false,
            is_writable: false,
        },
        AccountMeta {
            pubkey: demo_id(),
            is_signer: false,
            is_writable: false,
        },
    ];
    accounts.extend(exec_metas);
    Instruction {
        program_id: relay_id(),
        accounts,
        data: spec::encode_crank_v0_data(
            CONDITIONS_OFFSET as u32,
            condition_index,
            keeper_index,
            &resolved.data,
        ),
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
    assert_eq!(conditions.len(), 2);

    let sweep = &conditions[SWEEP_CONDITION as usize];
    assert!(sweep.is_active());
    assert_eq!(sweep_wake_ts(&conditions), i64::MAX);
    assert_eq!(sweep.min_payment, PAYMENT);
    assert_eq!(sweep.resolver_program, demo_id().to_bytes());
    assert_eq!(
        &sweep.resolver_disc[..],
        instruction::ResolveSweepV0::DISCRIMINATOR
    );
    assert_eq!(
        &sweep.executor_disc[..],
        instruction::SweepV0::DISCRIMINATOR
    );
    assert_eq!(sweep.resolver_accounts().len(), 1);
    assert_eq!(sweep.resolver_accounts()[0].address, ctx.book.to_bytes());

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
        &evict.executor_disc[..],
        instruction::EvictV0::DISCRIMINATOR
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

    let before = keeper_balance(&ctx);
    let ix = executor_ix(&ctx, SWEEP_CONDITION, &resolved);
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
    let ix = executor_ix(
        &ctx,
        SWEEP_CONDITION,
        &spec::ResolvedCrankV0 {
            accounts: demo_book::state::BookV0::executor_accounts(&addr(ctx.book)),
            data: sweep_args_wire(&[1]),
        },
    );
    let failed = send(&mut ctx, ix).unwrap_err();
    assert_eq!(custom_error_code(&failed), Some(6002)); // EntryNotExpired

    // Unknown id.
    warp_to(&mut ctx, t + 200);
    let ix = executor_ix(
        &ctx,
        SWEEP_CONDITION,
        &spec::ResolvedCrankV0 {
            accounts: BookV0::executor_accounts(&addr(ctx.book)),
            data: sweep_args_wire(&[42]),
        },
    );
    let failed = send(&mut ctx, ix).unwrap_err();
    assert_eq!(custom_error_code(&failed), Some(6001)); // EntryNotFound

    // Empty ids.
    let ix = executor_ix(
        &ctx,
        SWEEP_CONDITION,
        &spec::ResolvedCrankV0 {
            accounts: BookV0::executor_accounts(&addr(ctx.book)),
            data: sweep_args_wire(&[]),
        },
    );
    let failed = send(&mut ctx, ix).unwrap_err();
    assert_eq!(custom_error_code(&failed), Some(6003)); // NothingToSweep
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

// --- crank_v0 wrapper ---

#[test]
fn crank_v0_happy_path() {
    let mut ctx = setup();
    let t = now(&ctx);
    add_entry(&mut ctx, t + 100);
    warp_to(&mut ctx, t + 200);

    let resolved = resolve(&mut ctx, SWEEP_CONDITION).expect("work");

    let before = keeper_balance(&ctx);
    let ix = crank_ix(&ctx, SWEEP_CONDITION, &resolved);
    send(&mut ctx, ix).unwrap();
    assert_eq!(keeper_balance(&ctx), before + PAYMENT);
}

#[test]
fn crank_v0_rejects_underpayment() {
    let mut ctx = setup();
    let t = now(&ctx);
    add_entry(&mut ctx, t + 100);
    warp_to(&mut ctx, t + 200);

    // Divergence: executors now pay less than the block's advertised
    // min_payment.
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
    let ix = crank_ix(&ctx, SWEEP_CONDITION, &resolved);
    let failed = send(&mut ctx, ix).unwrap_err();
    assert_eq!(
        custom_error_code(&failed),
        Some(6006), // InsufficientKeeperPayment
        "expected InsufficientKeeperPayment, got {:?}",
        failed.err
    );

    // Unwrapped, the executor itself is fine — the wrapper is what catches
    // the divergence.
    let before = keeper_balance(&ctx);
    let ix = executor_ix(&ctx, SWEEP_CONDITION, &resolved);
    send(&mut ctx, ix).unwrap();
    assert_eq!(keeper_balance(&ctx), before + PAYMENT / 2);
}

#[test]
fn crank_v0_rejects_inactive_condition() {
    let mut ctx = setup();
    let t = now(&ctx);
    add_entry(&mut ctx, t + 100);
    warp_to(&mut ctx, t + 200);
    let resolved = resolve(&mut ctx, SWEEP_CONDITION).expect("work");

    let mut conditions = read_conditions(&ctx);
    conditions[SWEEP_CONDITION as usize].active = 0;
    write_conditions(&mut ctx, &conditions);

    let ix = crank_ix(&ctx, SWEEP_CONDITION, &resolved);
    let failed = send(&mut ctx, ix).unwrap_err();
    assert_eq!(custom_error_code(&failed), Some(6002)); // ConditionInactive
}

#[test]
fn crank_v0_rejects_executor_program_mismatch() {
    let mut ctx = setup();
    let t = now(&ctx);
    add_entry(&mut ctx, t + 100);
    warp_to(&mut ctx, t + 200);
    let resolved = resolve(&mut ctx, SWEEP_CONDITION).expect("work");

    // Pass relay itself as executor_program; the block says demo-book.
    let mut ix = crank_ix(&ctx, SWEEP_CONDITION, &resolved);
    ix.accounts[1].pubkey = relay_id();
    let failed = send(&mut ctx, ix).unwrap_err();
    assert_eq!(custom_error_code(&failed), Some(6003)); // ExecutorProgramMismatch
}

#[test]
fn crank_v0_rejects_self_reentry() {
    let mut ctx = setup();
    let t = now(&ctx);
    add_entry(&mut ctx, t + 100);
    warp_to(&mut ctx, t + 200);
    let resolved = resolve(&mut ctx, SWEEP_CONDITION).expect("work");

    // Craft a block whose executor is relay itself.
    let mut conditions = read_conditions(&ctx);
    conditions[SWEEP_CONDITION as usize].executor_program = relay_id().to_bytes();
    write_conditions(&mut ctx, &conditions);

    let mut ix = crank_ix(&ctx, SWEEP_CONDITION, &resolved);
    ix.accounts[1].pubkey = relay_id(); // match the crafted block
    let failed = send(&mut ctx, ix).unwrap_err();
    assert_eq!(custom_error_code(&failed), Some(6004)); // SelfReentry
}

#[test]
fn crank_v0_rejects_garbage_offset_and_bad_indices() {
    let mut ctx = setup();
    let t = now(&ctx);
    add_entry(&mut ctx, t + 100);
    warp_to(&mut ctx, t + 200);
    let resolved = resolve(&mut ctx, SWEEP_CONDITION).expect("work");

    // Offset pointing at entry data, not the block.
    let mut ix = crank_ix(&ctx, SWEEP_CONDITION, &resolved);
    ix.data = spec::encode_crank_v0_data(8, SWEEP_CONDITION, 0, &resolved.data);
    let failed = send(&mut ctx, ix).unwrap_err();
    assert_eq!(custom_error_code(&failed), Some(6000)); // InvalidConditionBlock

    // Condition index out of bounds.
    let mut ix = crank_ix(&ctx, SWEEP_CONDITION, &resolved);
    ix.data = spec::encode_crank_v0_data(CONDITIONS_OFFSET as u32, 9, 0, &resolved.data);
    let failed = send(&mut ctx, ix).unwrap_err();
    assert_eq!(custom_error_code(&failed), Some(6001)); // ConditionIndexOutOfBounds

    // Keeper index past the remaining accounts.
    let mut ix = crank_ix(&ctx, SWEEP_CONDITION, &resolved);
    ix.data =
        spec::encode_crank_v0_data(CONDITIONS_OFFSET as u32, SWEEP_CONDITION, 9, &resolved.data);
    let failed = send(&mut ctx, ix).unwrap_err();
    assert_eq!(custom_error_code(&failed), Some(6005)); // KeeperIndexOutOfBounds
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
        EVICT_CONDITION,
        &spec::ResolvedCrankV0 {
            accounts: BookV0::executor_accounts(&addr(ctx.book)),
            data: 1u64.to_le_bytes().to_vec(),
        },
    );
    let failed = send(&mut ctx, ix).unwrap_err();
    assert_eq!(custom_error_code(&failed), Some(6004)); // BelowEvictThreshold

    // At threshold: resolver picks the oldest entry; crank it wrapped.
    add_entry(&mut ctx, t + 300); // id 3 — entry_count hits EVICT_THRESHOLD
    let resolved = resolve(&mut ctx, EVICT_CONDITION).expect("work");
    assert_eq!(&resolved.data, &1u64.to_le_bytes()); // victim = oldest id

    let before = keeper_balance(&ctx);
    let ix = crank_ix(&ctx, EVICT_CONDITION, &resolved);
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
    let ix = executor_ix(&ctx, SWEEP_CONDITION, &resolved);
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
    let ix = crank_ix(&ctx, SWEEP_CONDITION, &resolved);
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
