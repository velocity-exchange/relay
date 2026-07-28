//! litesvm tests for the registry surface + spec-constant consistency.
//! Require the SBF build first: `./scripts/build-programs.sh` (or
//! `cargo-build-sbf --tools-version v1.52 --manifest-path relay/Cargo.toml`).
//!
//! The payment guards are exercised end-to-end in demo-book's test suite,
//! which loads both programs and brackets a real executor.

use anchor_v2_testing::{
    Keypair, LiteSVM, Message, Signer, VersionedMessage, VersionedTransaction,
};
use litesvm::types::{FailedTransactionMetadata, TransactionMetadata};
use relay::anchor_lang_v2::prelude::Address;
use relay::anchor_lang_v2::solana_program::instruction::Instruction;
use relay::anchor_lang_v2::{Discriminator, InstructionData};
use relay::state::{WatchV0, WATCH_ACCOUNT_LEN};
use relay::{accounts, instruction, RegisterWatchArgsV0};
use solana_pubkey::Pubkey;

const SO_PATH: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../target/deploy/relay.so");

fn program_id() -> Pubkey {
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
    registrar: Keypair,
    target: Pubkey,
    watch: Pubkey,
}

fn setup() -> Ctx {
    let mut svm = anchor_v2_testing::svm();
    svm.add_program_from_file(program_id(), SO_PATH)
        .expect("relay.so missing — run scripts/build-programs.sh first");

    let payer = Keypair::new();
    let registrar = Keypair::new();
    svm.airdrop(&payer.pubkey(), 100_000_000_000).unwrap();
    svm.airdrop(&registrar.pubkey(), 1_000_000_000).unwrap();

    let target = Pubkey::new_unique();
    svm.airdrop(&target, 1_000_000).unwrap();

    // Pre-created zeroed watch account, exactly WATCH_ACCOUNT_LEN bytes.
    let watch = Pubkey::new_unique();
    let rent = svm.minimum_balance_for_rent_exemption(WATCH_ACCOUNT_LEN);
    svm.set_account(
        watch,
        solana_account::Account {
            lamports: rent,
            data: vec![0u8; WATCH_ACCOUNT_LEN],
            owner: program_id(),
            executable: false,
            rent_epoch: 0,
        },
    )
    .unwrap();

    Ctx {
        svm,
        payer,
        registrar,
        target,
        watch,
    }
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
    let needs_registrar = ix
        .accounts
        .iter()
        .any(|m| m.is_signer && m.pubkey.to_bytes() == ctx.registrar.pubkey().to_bytes());
    if needs_registrar {
        signers.push(&ctx.registrar);
    }
    let tx = VersionedTransaction::try_new(VersionedMessage::Legacy(msg), &signers).unwrap();
    ctx.svm.send_transaction(tx)
}

fn register_ix(ctx: &Ctx, offset: u32) -> Instruction {
    instruction::RegisterWatchV0 {
        args: RegisterWatchArgsV0 { offset },
    }
    .to_instruction(accounts::RegisterWatchV0 {
        registrar: addr(ctx.registrar.pubkey()),
        target: addr(ctx.target),
        watch: addr(ctx.watch),
    })
}

fn custom_error_code(failed: &FailedTransactionMetadata) -> Option<u32> {
    // Avoid depending on solana-transaction-error enum paths across
    // versions; the Debug form pins the custom code unambiguously.
    let debug = format!("{:?}", failed.err);
    let needle = "Custom(";
    let start = debug.find(needle)? + needle.len();
    let end = debug[start..].find(')')? + start;
    debug[start..end].parse().ok()
}

// --- spec-constant consistency: the contract the turner relies on ---

#[test]
fn spec_constants_match_generated() {
    assert_eq!(
        instruction::BeginGuardV0::DISCRIMINATOR,
        &relay_spec::BEGIN_GUARD_V0_DISCRIMINATOR,
        "spec BEGIN_GUARD_V0_DISCRIMINATOR drifted from the program"
    );
    assert_eq!(
        instruction::AssertPaidV0::DISCRIMINATOR,
        &relay_spec::ASSERT_PAID_V0_DISCRIMINATOR,
        "spec ASSERT_PAID_V0_DISCRIMINATOR drifted from the program"
    );
    assert_eq!(
        WatchV0::DISCRIMINATOR,
        &relay_spec::WATCH_V0_DISCRIMINATOR,
        "spec WATCH_V0_DISCRIMINATOR drifted from the program"
    );
    assert_eq!(WATCH_ACCOUNT_LEN, relay_spec::WATCH_V0_LEN);
    assert_eq!(relay::state::GUARD_SEED, relay_spec::GUARD_SEED);
}

#[test]
fn guard_args_wire_matches_spec_encoders() {
    let begin = instruction::BeginGuardV0 {
        args: relay::BeginGuardArgsV0 { nonce: 3 },
    }
    .data();
    assert_eq!(
        begin,
        relay_spec::encode_begin_guard_v0_data(3),
        "spec begin_guard_v0 encoder drifted from the program's wincode wire"
    );

    let assert_paid = instruction::AssertPaidV0 {
        args: relay::AssertPaidArgsV0 {
            min_payment: 50_000,
            nonce: 3,
        },
    }
    .data();
    assert_eq!(
        assert_paid,
        relay_spec::encode_assert_paid_v0_data(50_000, 3),
        "spec assert_paid_v0 encoder drifted from the program's wincode wire"
    );
}

// --- registry ---

#[test]
fn register_and_parse_watch() {
    let mut ctx = setup();
    let ix = register_ix(&ctx, 616);
    send(&mut ctx, ix).unwrap();

    let account = ctx.svm.get_account(&ctx.watch).unwrap();
    assert_eq!(account.data.len(), relay_spec::WATCH_V0_LEN);
    let parsed = relay_spec::WatchV0::read_from_account(&account.data).unwrap();
    assert_eq!(parsed.registrar, ctx.registrar.pubkey().to_bytes());
    assert_eq!(parsed.target, ctx.target.to_bytes());
    assert_eq!(parsed.offset, 616);
}

#[test]
fn register_requires_zeroed_account() {
    let mut ctx = setup();
    let ix = register_ix(&ctx, 616);
    send(&mut ctx, ix).unwrap();
    // Second registration into the same (now non-zero) account must fail.
    let ix = register_ix(&ctx, 617);
    assert!(send(&mut ctx, ix).is_err());
}

#[test]
fn close_watch_returns_rent_to_registrar() {
    let mut ctx = setup();
    let ix = register_ix(&ctx, 616);
    send(&mut ctx, ix).unwrap();

    let rent = ctx.svm.get_account(&ctx.watch).unwrap().lamports;
    let registrar_before = ctx.svm.get_balance(&ctx.registrar.pubkey()).unwrap();

    let ix = instruction::CloseWatchV0 {}.to_instruction(accounts::CloseWatchV0 {
        watch: addr(ctx.watch),
        registrar: addr(ctx.registrar.pubkey()),
    });
    send(&mut ctx, ix).unwrap();

    let registrar_after = ctx.svm.get_balance(&ctx.registrar.pubkey()).unwrap();
    assert_eq!(registrar_after, registrar_before + rent);
    // Closed: gone or drained.
    let closed = ctx.svm.get_account(&ctx.watch);
    assert!(closed.is_none() || closed.unwrap().lamports == 0);
}

#[test]
fn close_watch_rejects_non_registrar() {
    let mut ctx = setup();
    let ix = register_ix(&ctx, 616);
    send(&mut ctx, ix).unwrap();

    // Payer signs, but is not the registrar recorded on the watch.
    let ix = instruction::CloseWatchV0 {}.to_instruction(accounts::CloseWatchV0 {
        watch: addr(ctx.watch),
        registrar: addr(ctx.payer.pubkey()),
    });
    let failed = send(&mut ctx, ix).unwrap_err();
    assert_eq!(
        custom_error_code(&failed),
        Some(6003),
        "expected InvalidRegistrar, got {:?}",
        failed.err
    );
}
