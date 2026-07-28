//! End-to-end turner tests against litesvm: the full loop — registry
//! refresh, wake evaluation, resolver simulation, executor build, payment
//! assertion, send — driven deterministically through `Turner::tick()`.
//!
//! Everything client-side is hand-rolled (discriminators via sha2, borsh
//! wires by hand) rather than imported from the program crates: the root
//! workspace must not depend on the anchor-v2 git tree, and doing it this
//! way doubles as an ABI-stability check on the demo program.
//!
//! Requires the SBF build first: `./scripts/build-programs.sh`.

use std::sync::{Arc, Mutex};

use anyhow::{anyhow, Result};
use async_trait::async_trait;
use litesvm::LiteSVM;
use relay_crank_turner::{
    feed_channel, AccountUpdate, BlockhashInfo, CachedSource, CachedSourceConfig, ChainSource,
    ClockSnapshot, Outcome, RejectReason, SignatureOutcome, SimOutcome, SkipReason, Stage, Turner,
    TurnerConfig, WatchFilter,
};
use relay_spec as spec;
use sha2::{Digest, Sha256};
use solana_account::ReadableAccount;
use solana_sdk::account::Account;
use solana_sdk::clock::Clock;
use solana_sdk::instruction::{AccountMeta, Instruction};
use solana_sdk::pubkey::Pubkey;
use solana_sdk::signature::{Keypair, Signature};
use solana_sdk::signer::Signer;
use solana_sdk::transaction::Transaction;

const RELAY_SO: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../programs/target/deploy/relay.so"
);
const DEMO_SO: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../programs/target/deploy/demo_book.so"
);

// Mirrors of demo-book layout constants, hand-pinned (see module docs).
const BOOK_ACCOUNT_LEN: usize = 2216;
const CONDITIONS_OFFSET: u32 = 616;
const ENTRY_COUNT_OFFSET: usize = 64;
const NEXT_EXPIRY_OFFSET: usize = 56;
const STAGING_OFFSET: usize = 1192;

const PAYMENT: u64 = 50_000;
const TREASURY: u64 = 1_000_000;

fn relay_id() -> Pubkey {
    "4D5tPhw9sqkdkR5CpmP427TH6y9p9AMuKUukUEHn3Mpu"
        .parse()
        .unwrap()
}

fn demo_id() -> Pubkey {
    "6PqZZeykcFwncPxs5LjjxzQshdRV29mpsFtmT3QS9jRZ"
        .parse()
        .unwrap()
}

fn disc(name: &str) -> Vec<u8> {
    Sha256::digest(format!("global:{name}").as_bytes())[..8].to_vec()
}

// --- litesvm-backed ChainSource ---

#[derive(Clone)]
struct LiteSvmSource {
    svm: Arc<Mutex<LiteSVM>>,
    /// litesvm has no getProgramAccounts; the harness registers watch
    /// pubkeys here as it creates them.
    watch_keys: Arc<Mutex<Vec<Pubkey>>>,
    /// Signatures this source has sent, so `signature_statuses` can answer.
    sent: Arc<Mutex<std::collections::HashSet<Signature>>>,
}

impl LiteSvmSource {
    fn new(svm: &Arc<Mutex<LiteSVM>>, watch_keys: &Arc<Mutex<Vec<Pubkey>>>) -> Self {
        Self {
            svm: Arc::clone(svm),
            watch_keys: Arc::clone(watch_keys),
            sent: Arc::new(Mutex::new(std::collections::HashSet::new())),
        }
    }
}

#[async_trait]
impl ChainSource for LiteSvmSource {
    async fn get_multiple_accounts(&self, pubkeys: &[Pubkey]) -> Result<Vec<Option<Account>>> {
        let svm = self.svm.lock().unwrap();
        Ok(pubkeys.iter().map(|pk| svm.get_account(pk)).collect())
    }

    async fn get_watch_accounts(
        &self,
        program: &Pubkey,
        target_programs: &[Pubkey],
    ) -> Result<Vec<(Pubkey, Account)>> {
        let svm = self.svm.lock().unwrap();
        let keys = self.watch_keys.lock().unwrap();
        // Stands in for the provider-side memcmp on `target_program`.
        let allowed: Vec<[u8; 32]> = target_programs.iter().map(|pk| pk.to_bytes()).collect();
        Ok(keys
            .iter()
            .filter_map(|pk| svm.get_account(pk).map(|acc| (*pk, acc)))
            .filter(|(_, acc)| acc.owner == *program && acc.data.len() == spec::WATCH_V0_LEN)
            .filter(|(_, acc)| {
                allowed.is_empty()
                    || spec::WatchV0::read_from_account(&acc.data)
                        .is_ok_and(|w| allowed.contains(&w.target_program))
            })
            .collect())
    }

    async fn clock(&self) -> Result<ClockSnapshot> {
        let clock: Clock = self.svm.lock().unwrap().get_sysvar();
        Ok(ClockSnapshot {
            slot: clock.slot,
            unix_timestamp: clock.unix_timestamp,
        })
    }

    async fn latest_blockhash(&self) -> Result<BlockhashInfo> {
        let svm = self.svm.lock().unwrap();
        Ok(BlockhashInfo {
            hash: svm.latest_blockhash(),
            // litesvm does not expire by height; nothing here tests the
            // resend path, which submit.rs owns.
            last_valid_block_height: u64::MAX,
        })
    }

    async fn block_height(&self) -> Result<u64> {
        let clock: Clock = self.svm.lock().unwrap().get_sysvar();
        Ok(clock.slot)
    }

    async fn signature_statuses(
        &self,
        signatures: &[Signature],
    ) -> Result<Vec<Option<SignatureOutcome>>> {
        // litesvm applies transactions synchronously, so anything the
        // harness sent has already landed.
        let sent = self.sent.lock().unwrap();
        Ok(signatures
            .iter()
            .map(|signature| sent.contains(signature).then_some(SignatureOutcome::Landed))
            .collect())
    }

    async fn simulate_transaction(
        &self,
        tx: &Transaction,
        return_accounts: &[Pubkey],
    ) -> Result<SimOutcome> {
        let svm = self.svm.lock().unwrap();
        Ok(match svm.simulate_transaction(tx.clone()) {
            Ok(info) => {
                // Post-execution state, in the order asked for — the RPC
                // `accounts` config equivalent.
                let accounts = return_accounts
                    .iter()
                    .map(|wanted| {
                        info.post_accounts
                            .iter()
                            .find(|(address, _)| address.to_bytes() == wanted.to_bytes())
                            .map(|(_, account)| Account {
                                lamports: account.lamports(),
                                data: account.data().to_vec(),
                                owner: Pubkey::new_from_array(account.owner().to_bytes()),
                                executable: account.executable(),
                                rent_epoch: account.rent_epoch(),
                            })
                    })
                    .collect();
                SimOutcome {
                    err: None,
                    logs: info.meta.logs,
                    return_data: Some(info.meta.return_data.data),
                    units_consumed: info.meta.compute_units_consumed,
                    accounts,
                }
            }
            Err(failed) => SimOutcome {
                err: Some(format!("{:?}", failed.err)),
                logs: failed.meta.logs,
                return_data: Some(failed.meta.return_data.data),
                units_consumed: failed.meta.compute_units_consumed,
                accounts: Vec::new(),
            },
        })
    }

    async fn send_transaction(&self, tx: &Transaction) -> Result<Signature> {
        let signature = {
            let mut svm = self.svm.lock().unwrap();
            svm.send_transaction(tx.clone())
                .map(|meta| meta.signature)
                .map_err(|failed| anyhow!("send failed: {:?}", failed.err))?
        };
        self.sent.lock().unwrap().insert(signature);
        Ok(signature)
    }

    async fn recent_priority_fee(&self, _accounts: &[Pubkey]) -> Result<u64> {
        Ok(1_234)
    }
}

/// Wraps a source and refuses to simulate, so a test can prove the local
/// simulator never falls through to the provider.
#[derive(Clone)]
struct NoRemoteSimSource {
    inner: LiteSvmSource,
    fetches: Arc<Mutex<usize>>,
}

#[async_trait]
impl ChainSource for NoRemoteSimSource {
    async fn get_multiple_accounts(&self, pubkeys: &[Pubkey]) -> Result<Vec<Option<Account>>> {
        *self.fetches.lock().unwrap() += 1;
        self.inner.get_multiple_accounts(pubkeys).await
    }
    async fn get_watch_accounts(
        &self,
        program: &Pubkey,
        target_programs: &[Pubkey],
    ) -> Result<Vec<(Pubkey, Account)>> {
        self.inner
            .get_watch_accounts(program, target_programs)
            .await
    }
    async fn clock(&self) -> Result<ClockSnapshot> {
        self.inner.clock().await
    }
    async fn latest_blockhash(&self) -> Result<BlockhashInfo> {
        self.inner.latest_blockhash().await
    }
    async fn block_height(&self) -> Result<u64> {
        self.inner.block_height().await
    }
    async fn signature_statuses(
        &self,
        signatures: &[Signature],
    ) -> Result<Vec<Option<SignatureOutcome>>> {
        self.inner.signature_statuses(signatures).await
    }
    async fn recent_priority_fee(&self, accounts: &[Pubkey]) -> Result<u64> {
        self.inner.recent_priority_fee(accounts).await
    }
    async fn simulate_transaction(
        &self,
        _tx: &Transaction,
        _return_accounts: &[Pubkey],
    ) -> Result<SimOutcome> {
        panic!("simulation must not reach the provider");
    }
    async fn send_transaction(&self, tx: &Transaction) -> Result<Signature> {
        self.inner.send_transaction(tx).await
    }
}

// --- harness ---

struct Harness {
    turner: Turner<LiteSvmSource>,
    svm: Arc<Mutex<LiteSVM>>,
    watch_keys: Arc<Mutex<Vec<Pubkey>>>,
    authority: Keypair,
    book: Pubkey,
    t0: i64,
}

fn setup(payment: u64, evict_threshold: u32, config: TurnerConfig) -> Harness {
    setup_with_treasury(payment, evict_threshold, TREASURY, config)
}

fn setup_with_treasury(
    payment: u64,
    evict_threshold: u32,
    treasury: u64,
    config: TurnerConfig,
) -> Harness {
    let mut svm = LiteSVM::new();
    svm.add_program_from_file(relay_id(), RELAY_SO)
        .expect("relay.so missing — run scripts/build-programs.sh first");
    svm.add_program_from_file(demo_id(), DEMO_SO)
        .expect("demo_book.so missing — run scripts/build-programs.sh first");

    let authority = Keypair::new();
    let keeper = Keypair::new();
    svm.airdrop(&authority.pubkey(), 100_000_000_000).unwrap();
    svm.airdrop(&keeper.pubkey(), 1_000_000_000).unwrap();

    // Book account, pre-created zeroed with a treasury surplus.
    let book = Pubkey::new_unique();
    let rent = svm.minimum_balance_for_rent_exemption(BOOK_ACCOUNT_LEN);
    svm.set_account(
        book,
        Account {
            lamports: rent + treasury,
            data: vec![0u8; BOOK_ACCOUNT_LEN],
            owner: demo_id(),
            executable: false,
            rent_epoch: 0,
        },
    )
    .unwrap();

    let clock: Clock = svm.get_sysvar();
    let t0 = clock.unix_timestamp;

    let svm = Arc::new(Mutex::new(svm));
    let watch_keys = Arc::new(Mutex::new(Vec::new()));
    let source = LiteSvmSource::new(&svm, &watch_keys);
    let mut harness = Harness {
        turner: Turner::new(source, keeper, config),
        svm,
        watch_keys,
        authority,
        book,
        t0,
    };

    // initialize_book_v0(payment_per_crank: u64, evict_threshold: u32)
    let args: Vec<u8> = payment
        .to_le_bytes()
        .into_iter()
        .chain(evict_threshold.to_le_bytes())
        .collect();
    harness.send_admin(demo_ix(
        "initialize_book_v0",
        vec![
            AccountMeta::new_readonly(harness.authority.pubkey(), true),
            AccountMeta::new(harness.book, false),
        ],
        &args,
    ));
    harness.register_watch(harness.book, CONDITIONS_OFFSET);
    harness
}

fn demo_ix(name: &str, accounts: Vec<AccountMeta>, args: &[u8]) -> Instruction {
    Instruction {
        program_id: demo_id(),
        accounts,
        data: disc(name).into_iter().chain(args.iter().copied()).collect(),
    }
}

impl Harness {
    fn send_admin(&mut self, ix: Instruction) {
        let mut svm = self.svm.lock().unwrap();
        svm.expire_blockhash();
        let blockhash = svm.latest_blockhash();
        let tx = Transaction::new_signed_with_payer(
            &[ix],
            Some(&self.authority.pubkey()),
            &[&self.authority],
            blockhash,
        );
        svm.send_transaction(tx)
            .unwrap_or_else(|failed| panic!("admin tx failed: {:?}", failed.err));
    }

    fn register_watch(&mut self, target: Pubkey, offset: u32) -> Pubkey {
        let watch = Pubkey::new_unique();
        {
            let mut svm = self.svm.lock().unwrap();
            let rent = svm.minimum_balance_for_rent_exemption(spec::WATCH_V0_LEN);
            svm.set_account(
                watch,
                Account {
                    lamports: rent,
                    data: vec![0u8; spec::WATCH_V0_LEN],
                    owner: relay_id(),
                    executable: false,
                    rent_epoch: 0,
                },
            )
            .unwrap();
        }
        self.send_admin(Instruction {
            program_id: relay_id(),
            accounts: vec![
                AccountMeta::new_readonly(self.authority.pubkey(), true),
                AccountMeta::new_readonly(target, false),
                AccountMeta::new(watch, false),
            ],
            data: disc("register_watch_v0")
                .into_iter()
                .chain(offset.to_le_bytes())
                .collect(),
        });
        self.watch_keys.lock().unwrap().push(watch);
        watch
    }

    fn add_entry(&mut self, expiry_ts: i64) {
        let ix = demo_ix(
            "add_entry_v0",
            vec![
                AccountMeta::new(self.book, false),
                AccountMeta::new_readonly(self.authority.pubkey(), true),
            ],
            &expiry_ts.to_le_bytes(),
        );
        self.send_admin(ix);
    }

    fn cancel_entry(&mut self, id: u64) {
        let ix = demo_ix(
            "cancel_entry_v0",
            vec![
                AccountMeta::new(self.book, false),
                AccountMeta::new_readonly(self.authority.pubkey(), true),
            ],
            &id.to_le_bytes(),
        );
        self.send_admin(ix);
    }

    fn set_payment(&mut self, payment: u64) {
        let ix = demo_ix(
            "set_payment_v0",
            vec![
                AccountMeta::new(self.book, false),
                AccountMeta::new_readonly(self.authority.pubkey(), true),
            ],
            &payment.to_le_bytes(),
        );
        self.send_admin(ix);
    }

    /// Advance chain time; `slots` also moves the slot so backoff windows
    /// (slot-denominated) progress.
    fn warp(&mut self, unix_ts: i64, slots: u64) {
        let mut svm = self.svm.lock().unwrap();
        let mut clock: Clock = svm.get_sysvar();
        clock.unix_timestamp = unix_ts;
        clock.slot += slots;
        svm.set_sysvar(&clock);
    }

    fn keeper_balance(&self) -> u64 {
        let keeper = self.turner.keeper_pubkey();
        self.svm.lock().unwrap().get_balance(&keeper).unwrap()
    }

    /// Create and register another book, so a tick has several
    /// independent targets (watches dedupe by target+offset).
    fn add_book(&mut self, payment: u64, evict_threshold: u32) -> Pubkey {
        let book = Pubkey::new_unique();
        {
            let mut svm = self.svm.lock().unwrap();
            let rent = svm.minimum_balance_for_rent_exemption(BOOK_ACCOUNT_LEN);
            svm.set_account(
                book,
                Account {
                    lamports: rent + TREASURY,
                    data: vec![0u8; BOOK_ACCOUNT_LEN],
                    owner: demo_id(),
                    executable: false,
                    rent_epoch: 0,
                },
            )
            .unwrap();
        }
        let args: Vec<u8> = payment
            .to_le_bytes()
            .into_iter()
            .chain(evict_threshold.to_le_bytes())
            .collect();
        self.send_admin(demo_ix(
            "initialize_book_v0",
            vec![
                AccountMeta::new_readonly(self.authority.pubkey(), true),
                AccountMeta::new(book, false),
            ],
            &args,
        ));
        self.register_watch(book, CONDITIONS_OFFSET);
        book
    }

    /// Add an entry to a specific book.
    fn add_entry_to(&mut self, book: Pubkey, expiry_ts: i64) {
        let ix = demo_ix(
            "add_entry_v0",
            vec![
                AccountMeta::new(book, false),
                AccountMeta::new_readonly(self.authority.pubkey(), true),
            ],
            &expiry_ts.to_le_bytes(),
        );
        self.send_admin(ix);
    }

    fn conditions(&self) -> Vec<spec::ConditionV0> {
        spec::read_conditions_unaligned(&self.book_data(), CONDITIONS_OFFSET as usize).unwrap()
    }

    fn write_conditions(&mut self, conditions: &[spec::ConditionV0]) {
        let mut svm = self.svm.lock().unwrap();
        let mut account = svm.get_account(&self.book).unwrap();
        spec::write_block(&mut account.data[CONDITIONS_OFFSET as usize..], conditions).unwrap();
        svm.set_account(self.book, account).unwrap();
    }

    fn slot(&self) -> u64 {
        let clock: Clock = self.svm.lock().unwrap().get_sysvar();
        clock.slot
    }

    fn guard_exists(&self) -> bool {
        let guard = self.turner.guard_address(0);
        self.svm
            .lock()
            .unwrap()
            .get_account(&guard)
            .is_some_and(|a| !a.data.is_empty())
    }

    fn staging_bytes(&self) -> Vec<u8> {
        self.book_data()[STAGING_OFFSET..STAGING_OFFSET + 64].to_vec()
    }

    fn book_data(&self) -> Vec<u8> {
        self.svm
            .lock()
            .unwrap()
            .get_account(&self.book)
            .unwrap()
            .data
    }

    fn entry_count(&self) -> u32 {
        u32::from_le_bytes(
            self.book_data()[ENTRY_COUNT_OFFSET..ENTRY_COUNT_OFFSET + 4]
                .try_into()
                .unwrap(),
        )
    }

    fn next_expiry(&self) -> i64 {
        i64::from_le_bytes(
            self.book_data()[NEXT_EXPIRY_OFFSET..NEXT_EXPIRY_OFFSET + 8]
                .try_into()
                .unwrap(),
        )
    }

    async fn refresh(&mut self) -> usize {
        self.turner.refresh_watches().await.unwrap().admitted
    }

    async fn tick(&mut self) -> Vec<Outcome> {
        self.turner.tick().await.unwrap()
    }
}

fn sent(outcomes: &[Outcome]) -> Vec<&Outcome> {
    outcomes
        .iter()
        .filter(|o| matches!(o, Outcome::Sent { .. }))
        .collect()
}

fn failed(outcomes: &[Outcome]) -> Vec<&Outcome> {
    outcomes
        .iter()
        .filter(|o| matches!(o, Outcome::Failed { .. }))
        .collect()
}

// --- tests ---

#[tokio::test]
async fn sweep_end_to_end() {
    let mut h = setup(PAYMENT, 100, TurnerConfig::default());
    assert_eq!(h.refresh().await, 1);
    let t0 = h.t0;
    h.add_entry(t0 + 100); // id 1
    h.add_entry(t0 + 9000); // id 2

    // Nothing due: sweep wake is t0+100; evict change-wake first-evaluates to
    // no-work. No transaction may land.
    let outcomes = h.tick().await;
    assert!(sent(&outcomes).is_empty(), "{outcomes:?}");

    // Past the first expiry: exactly one crank lands, the expired entry is
    // gone, and the hint self-repairs to the survivor. (Economics are
    // pinned separately — the first guarded crank also pays guard rent.)
    h.warp(t0 + 150, 2);
    let outcomes = h.tick().await;
    assert_eq!(sent(&outcomes).len(), 1, "{outcomes:?}");
    assert_eq!(h.entry_count(), 1);
    assert_eq!(h.next_expiry(), t0 + 9000);

    // Wake repaired to t0+9000: nothing further to do.
    h.warp(t0 + 200, 2);
    let outcomes = h.tick().await;
    assert!(sent(&outcomes).is_empty(), "{outcomes:?}");
}

#[tokio::test]
async fn stale_early_hint_resolves_to_no_work() {
    let mut h = setup(PAYMENT, 100, TurnerConfig::default());
    h.refresh().await;
    let t0 = h.t0;
    h.add_entry(t0 + 100); // id 1
    h.cancel_entry(1); // hint now stale-early (still t0+100)

    let balance_before = h.keeper_balance();
    h.warp(t0 + 200, 2);
    let outcomes = h.tick().await;
    assert!(sent(&outcomes).is_empty(), "{outcomes:?}");
    assert!(
        outcomes
            .iter()
            .any(|o| matches!(o, Outcome::NoWork((_, _, 0)))),
        "sweep should resolve to no-work: {outcomes:?}"
    );
    // Nothing landed: simulation is free, keeper balance untouched.
    assert_eq!(h.keeper_balance(), balance_before);
}

#[tokio::test]
async fn evict_fires_on_changed_count() {
    let mut h = setup(PAYMENT, 2, TurnerConfig::default());
    h.refresh().await;
    let t0 = h.t0;
    h.add_entry(t0 + 5000); // id 1 (oldest)
    h.add_entry(t0 + 6000); // id 2 — count hits the threshold

    // First evaluation of the change wake sees work immediately.
    let outcomes = h.tick().await;
    assert_eq!(sent(&outcomes).len(), 1, "{outcomes:?}");
    assert_eq!(h.entry_count(), 1);

    // Count changed again (2 → 1): change wake re-fires, resolver reports
    // below-threshold, nothing lands.
    h.warp(t0, 2);
    let outcomes = h.tick().await;
    assert!(sent(&outcomes).is_empty(), "{outcomes:?}");
    assert!(
        outcomes
            .iter()
            .any(|o| matches!(o, Outcome::NoWork((_, _, 1)))),
        "evict should resolve to no-work: {outcomes:?}"
    );
}

#[tokio::test]
async fn min_payment_filter_skips_cheap_conditions() {
    let config = TurnerConfig {
        min_crank_payment: PAYMENT + 1,
        ..TurnerConfig::default()
    };
    let mut h = setup(PAYMENT, 100, config);
    h.refresh().await;
    let t0 = h.t0;
    h.add_entry(t0 + 100);

    h.warp(t0 + 200, 2);
    let outcomes = h.tick().await;
    assert!(sent(&outcomes).is_empty());
    assert!(
        outcomes
            .iter()
            .all(|o| matches!(o, Outcome::Skipped(_, SkipReason::BelowMinPayment))),
        "{outcomes:?}"
    );
}

#[tokio::test]
async fn underpaying_executor_is_blocked_by_crank_wrapper() {
    let mut h = setup(PAYMENT, 100, TurnerConfig::default());
    h.refresh().await;
    let t0 = h.t0;
    h.add_entry(t0 + 100);
    // Divergence: the block still advertises PAYMENT, executors now pay half.
    h.set_payment(PAYMENT / 2);

    let balance_before = h.keeper_balance();
    h.warp(t0 + 200, 2);
    let outcomes = h.tick().await;
    assert!(sent(&outcomes).is_empty(), "{outcomes:?}");
    assert!(
        failed(&outcomes).iter().any(|o| matches!(
            o,
            Outcome::Failed {
                stage: Stage::ExecuteSim,
                ..
            }
        )),
        "wrapper simulation should catch the underpayment: {outcomes:?}"
    );
    // The bad crank never left simulation.
    assert_eq!(h.keeper_balance(), balance_before);
    assert_eq!(h.entry_count(), 1);

    // Failure backoff: the condition is suppressed on the next tick.
    h.warp(t0 + 201, 1);
    let outcomes = h.tick().await;
    assert!(
        outcomes
            .iter()
            .any(|o| matches!(o, Outcome::Skipped((_, _, 0), SkipReason::Backoff))),
        "{outcomes:?}"
    );
}

#[tokio::test]
async fn garbage_watch_is_ignored() {
    let mut h = setup(PAYMENT, 100, TurnerConfig::default());
    let junk = Pubkey::new_unique();
    h.svm.lock().unwrap().airdrop(&junk, 1_000_000).unwrap();
    h.register_watch(junk, 0);

    // A watch pointing at data that is not a condition block is dropped at
    // refresh, so it never costs a fetch or a simulation afterwards.
    let summary = h.turner.refresh_watches().await.unwrap();
    assert_eq!(summary.admitted, 1);
    assert_eq!(summary.rejected_for(RejectReason::Unparseable), 1);

    // The real watch still works alongside the junk one.
    let t0 = h.t0;
    h.add_entry(t0 + 100);
    h.warp(t0 + 200, 2);
    let outcomes = h.tick().await;
    assert_eq!(sent(&outcomes).len(), 1, "{outcomes:?}");
}

#[tokio::test]
async fn turner_resolves_without_committing_staging() {
    let mut h = setup(PAYMENT, 100, TurnerConfig::default());
    h.refresh().await;
    let t0 = h.t0;
    h.add_entry(t0 + 100);
    h.cancel_entry(1); // stale-early hint: resolver will report no work

    let staging_before = h.staging_bytes();
    h.warp(t0 + 200, 2);
    let outcomes = h.tick().await;
    assert!(sent(&outcomes).is_empty(), "{outcomes:?}");
    assert_eq!(
        h.staging_bytes(),
        staging_before,
        "resolver simulation must not commit staging bytes"
    );
}

/// A full sweep batch stages more than the executor could have carried in
/// return data, and still cranks end to end.
#[tokio::test]
async fn turner_cranks_large_staged_batch() {
    let mut h = setup(PAYMENT, 100, TurnerConfig::default());
    h.refresh().await;
    let t0 = h.t0;
    (0..8).for_each(|i| h.add_entry(t0 + 10 + i));

    h.warp(t0 + 1000, 2);
    let outcomes = h.tick().await;
    assert_eq!(sent(&outcomes).len(), 1, "{outcomes:?}");
    assert_eq!(h.entry_count(), 0, "whole batch swept in one crank");
}

// --- AtSlot wake ---

/// The slot-denominated sibling of AtTimestamp: not due before the target
/// slot, due at/after it. Crafted directly into the block (demo-book uses
/// timestamps), which also exercises reading a wake kind the target program
/// never writes itself.
#[tokio::test]
async fn at_slot_wake_fires_at_target_slot() {
    let mut h = setup(PAYMENT, 100, TurnerConfig::default());
    h.refresh().await;
    let t0 = h.t0;
    h.add_entry(t0 - 10); // already expired: work exists whenever the wake fires

    let target_slot = h.slot() + 50;
    let mut conditions = h.conditions();
    conditions[0].wake_kind = spec::WakeKind::AtSlot as u8;
    conditions[0].wake_slot = target_slot;
    conditions[0].wake_ts = 0;
    h.write_conditions(&conditions);
    assert_eq!(
        h.conditions()[0].wake(),
        Ok(spec::WakeView::AtSlot { slot: target_slot })
    );

    // Before the slot: not due, despite work being available.
    let outcomes = h.tick().await;
    assert!(sent(&outcomes).is_empty(), "{outcomes:?}");
    assert!(
        outcomes
            .iter()
            .any(|o| matches!(o, Outcome::Skipped((_, _, 0), SkipReason::NotDue))),
        "{outcomes:?}"
    );

    // At the slot: fires.
    h.warp(t0, 60);
    let outcomes = h.tick().await;
    assert_eq!(sent(&outcomes).len(), 1, "{outcomes:?}");
    assert_eq!(h.entry_count(), 0);
}

// --- subscription cache (shared by the ws and gRPC transports) ---

/// `CachedSource` serves reads from feed updates instead of the inner
/// source, and publishes the interest set backends subscribe to.
#[tokio::test]
async fn cached_source_serves_feed_updates_and_publishes_interest() {
    let h = setup(PAYMENT, 100, TurnerConfig::default());
    let inner = LiteSvmSource::new(&h.svm, &h.watch_keys);
    let (sender, receiver) = feed_channel();
    let cached = CachedSource::new(
        inner,
        receiver,
        CachedSourceConfig {
            relay_program: relay_id(),
            repoll_every: 0, // never fall back: prove the cache is doing the work
            watch_programs: Vec::new(),
        },
    );

    // Cold read falls through to the inner source and seeds the cache; the
    // pubkey shows up in the interest set a backend would subscribe to.
    let first = cached.get_multiple_accounts(&[h.book]).await.unwrap();
    assert!(first[0].is_some());
    let interest = sender.interest.borrow().clone();
    assert!(interest.contains(&h.book), "book should be watched");
    assert!(
        interest.contains(&solana_sdk::sysvar::clock::id()),
        "clock rides the feed too"
    );

    // A pushed update wins over the cached snapshot.
    let mut mutated = h.svm.lock().unwrap().get_account(&h.book).unwrap();
    mutated.data[STAGING_OFFSET] = 0xEE;
    sender
        .updates
        .send(AccountUpdate {
            pubkey: h.book,
            account: Some(mutated),
            slot: 42,
        })
        .unwrap();
    let served = cached.get_multiple_accounts(&[h.book]).await.unwrap();
    assert_eq!(served[0].as_ref().unwrap().data[STAGING_OFFSET], 0xEE);

    // Older-slot updates are dropped rather than rewinding the cache.
    let mut stale = served[0].clone().unwrap();
    stale.data[STAGING_OFFSET] = 0x11;
    sender
        .updates
        .send(AccountUpdate {
            pubkey: h.book,
            account: Some(stale),
            slot: 41,
        })
        .unwrap();
    let served = cached.get_multiple_accounts(&[h.book]).await.unwrap();
    assert_eq!(
        served[0].as_ref().unwrap().data[STAGING_OFFSET],
        0xEE,
        "stale-slot update must not overwrite newer state"
    );
}

/// The turner drives identically through a subscription-fed cache — the
/// transport swap is invisible above `ChainSource`.
#[tokio::test]
async fn turner_cranks_through_cached_source() {
    let mut h = setup(PAYMENT, 100, TurnerConfig::default());
    let t0 = h.t0;
    h.add_entry(t0 + 100);

    let inner = LiteSvmSource::new(&h.svm, &h.watch_keys);
    let (_sender, receiver) = feed_channel();
    let cached = CachedSource::new(
        inner,
        receiver,
        CachedSourceConfig {
            relay_program: relay_id(),
            // No live backend in this test, so repoll every read keeps the
            // cache honest against the inner source.
            repoll_every: 1,
            watch_programs: Vec::new(),
        },
    );
    let mut turner = Turner::new(cached, Keypair::new(), TurnerConfig::default());
    {
        let mut svm = h.svm.lock().unwrap();
        svm.airdrop(&turner.keeper_pubkey(), 1_000_000_000).unwrap();
    }
    assert_eq!(turner.refresh_watches().await.unwrap().admitted, 1);

    h.warp(t0 + 200, 2);
    let outcomes = turner.tick().await.unwrap();
    assert_eq!(sent(&outcomes).len(), 1, "{outcomes:?}");
    assert_eq!(h.entry_count(), 0);
}

// --- watch filtering ---

/// A turner scoped to its own program ignores another protocol's watches
/// outright: they are never admitted, so their targets are never fetched,
/// subscribed, parsed, or simulated.
#[tokio::test]
async fn filter_scopes_turner_to_its_own_programs() {
    let mut h = setup(PAYMENT, 100, TurnerConfig::default());
    // A foreign protocol registers a watch on an account it owns.
    let foreign_program = Pubkey::new_unique();
    let foreign_target = Pubkey::new_unique();
    {
        let mut svm = h.svm.lock().unwrap();
        svm.set_account(
            foreign_target,
            Account {
                lamports: 1_000_000,
                data: vec![0u8; 4096],
                owner: foreign_program,
                executable: false,
                rent_epoch: 0,
            },
        )
        .unwrap();
    }
    h.register_watch(foreign_target, 0);

    // Unfiltered: both watches are candidates (the foreign one is dropped
    // only later, for being unparseable — after we paid to fetch it).
    let summary = h.turner.refresh_watches().await.unwrap();
    assert_eq!(summary.admitted, 1);
    assert_eq!(summary.rejected_for(RejectReason::Unparseable), 1);

    // Scoped to demo-book: the foreign watch is rejected from the registry
    // alone, before its 4KB target is ever read.
    h.turner = Turner::new(
        LiteSvmSource::new(&h.svm, &h.watch_keys),
        Keypair::new(),
        TurnerConfig {
            filter: WatchFilter::for_programs([demo_id()]),
            ..TurnerConfig::default()
        },
    );
    let summary = h.turner.refresh_watches().await.unwrap();
    assert_eq!(summary.admitted, 1);
    assert!(
        summary.rejected.is_empty(),
        "the provider filtered the foreign watch out server-side, so the turner never saw it: \
         {summary:?}"
    );

    // The same rule is enforced locally too, for providers that ignore
    // filters (and for the subscription cache).
    let scoped = WatchFilter::for_programs([demo_id()]);
    let foreign_watch = relay_crank_turner::Watch {
        target_program: foreign_program,
        target: foreign_target,
        registrar: Pubkey::new_unique(),
        offset: 0,
    };
    assert_eq!(
        scoped.check_registry(&foreign_watch),
        Err(RejectReason::ProgramNotAllowed)
    );

    // Denylisting our own program empties the working set entirely.
    h.turner = Turner::new(
        LiteSvmSource::new(&h.svm, &h.watch_keys),
        Keypair::new(),
        TurnerConfig {
            filter: WatchFilter {
                blocked_target_programs: [demo_id()].into_iter().collect(),
                ..WatchFilter::default()
            },
            ..TurnerConfig::default()
        },
    );
    let summary = h.turner.refresh_watches().await.unwrap();
    assert_eq!(summary.admitted, 0);
    assert_eq!(summary.rejected_for(RejectReason::ProgramBlocked), 1);
    assert!(h.tick().await.is_empty(), "nothing tracked, nothing ticked");
}

/// The fee bar drops a whole watch, not just its conditions — a book that
/// pays too little stops costing the turner anything.
#[tokio::test]
async fn filter_drops_watches_that_pay_too_little() {
    let cheap = 10;
    let mut h = setup(cheap, 100, TurnerConfig::default());
    let t0 = h.t0;
    h.add_entry(t0 + 100);

    h.turner = Turner::new(
        LiteSvmSource::new(&h.svm, &h.watch_keys),
        Keypair::new(),
        TurnerConfig {
            min_crank_payment: cheap + 1,
            ..TurnerConfig::default()
        },
    );
    let summary = h.turner.refresh_watches().await.unwrap();
    assert_eq!(summary.admitted, 0);
    assert_eq!(summary.rejected_for(RejectReason::PaysTooLittle), 1);

    // Even with work waiting, an unadmitted watch is never even looked at.
    h.warp(t0 + 200, 2);
    assert!(h.tick().await.is_empty());
    assert_eq!(h.entry_count(), 1, "nothing was cranked");
}

/// Size and count ceilings, for targets that are expensive to stream.
#[tokio::test]
async fn filter_enforces_size_and_count_ceilings() {
    let mut h = setup(PAYMENT, 100, TurnerConfig::default());

    h.turner = Turner::new(
        LiteSvmSource::new(&h.svm, &h.watch_keys),
        Keypair::new(),
        TurnerConfig {
            filter: WatchFilter {
                max_target_bytes: Some(BOOK_ACCOUNT_LEN - 1),
                ..WatchFilter::default()
            },
            ..TurnerConfig::default()
        },
    );
    let summary = h.turner.refresh_watches().await.unwrap();
    assert_eq!(summary.admitted, 0);
    assert_eq!(summary.rejected_for(RejectReason::TargetTooLarge), 1);

    h.turner = Turner::new(
        LiteSvmSource::new(&h.svm, &h.watch_keys),
        Keypair::new(),
        TurnerConfig {
            filter: WatchFilter {
                max_watches: Some(0),
                ..WatchFilter::default()
            },
            ..TurnerConfig::default()
        },
    );
    let summary = h.turner.refresh_watches().await.unwrap();
    assert_eq!(summary.admitted, 0);
    assert_eq!(summary.rejected_for(RejectReason::OverCapacity), 1);
}

/// The program allowlist is pushed down to the provider, so a filtered
/// turner never even receives the other watches.
#[tokio::test]
async fn program_allowlist_is_pushed_to_the_provider() {
    let mut h = setup(PAYMENT, 100, TurnerConfig::default());
    let foreign_program = Pubkey::new_unique();
    let foreign_target = Pubkey::new_unique();
    {
        let mut svm = h.svm.lock().unwrap();
        svm.set_account(
            foreign_target,
            Account {
                lamports: 1_000_000,
                data: vec![0u8; 128],
                owner: foreign_program,
                executable: false,
                rent_epoch: 0,
            },
        )
        .unwrap();
    }
    h.register_watch(foreign_target, 0);

    let source = LiteSvmSource::new(&h.svm, &h.watch_keys);
    let all = source
        .get_watch_accounts(&relay_id(), &[])
        .await
        .unwrap()
        .len();
    let scoped = source
        .get_watch_accounts(&relay_id(), &[demo_id()])
        .await
        .unwrap();
    assert_eq!(all, 2, "both watches exist in the registry");
    assert_eq!(scoped.len(), 1, "provider filtered by target_program");
    let parsed = spec::WatchV0::read_from_account(&scoped[0].1.data).unwrap();
    assert_eq!(parsed.target_program, demo_id().to_bytes());
    assert_eq!(parsed.target, h.book.to_bytes());
}

// --- guarded execution ---

/// Steady-state economics: the executor goes in directly (no CPI) and the
/// keeper nets the payment minus the fee. The first guarded crank also
/// creates the keeper's guard account, so its rent shows up once.
#[tokio::test]
async fn guarded_cranks_pay_the_keeper() {
    let mut h = setup(PAYMENT, 100, TurnerConfig::default());
    h.refresh().await;
    let t0 = h.t0;

    // First crank: pays for the guard account as well.
    h.add_entry(t0 + 100);
    h.warp(t0 + 200, 2);
    let before = h.keeper_balance();
    assert_eq!(sent(&h.tick().await).len(), 1);
    assert!(
        h.keeper_balance() < before,
        "one-time guard rent exceeds one payment"
    );
    assert!(h.guard_exists(), "guard account created on first use");

    // Every crank after that is pure profit minus the fee.
    h.add_entry(t0 + 300);
    h.warp(t0 + 400, 2);
    let before = h.keeper_balance();
    assert_eq!(sent(&h.tick().await).len(), 1);
    let delta = h.keeper_balance() as i64 - before as i64;
    assert!(
        delta > 0 && delta <= PAYMENT as i64,
        "keeper delta {delta} should be payment minus fee"
    );
}

/// With guards off the turner submits the bare executor — one instruction,
/// no guard account, no relay involvement at execution time at all.
#[tokio::test]
async fn unguarded_cranks_skip_relay_entirely() {
    let mut h = setup(
        PAYMENT,
        100,
        TurnerConfig {
            guard_payments: false,
            ..TurnerConfig::default()
        },
    );
    h.refresh().await;
    let t0 = h.t0;
    h.add_entry(t0 + 100);

    h.warp(t0 + 200, 2);
    let before = h.keeper_balance();
    assert_eq!(sent(&h.tick().await).len(), 1);
    assert!(h.keeper_balance() > before, "paid, minus the fee");
    assert_eq!(h.entry_count(), 0);
    assert!(!h.guard_exists(), "no guard account is ever created");
}

/// The guard reverts the whole transaction — executor work included — when
/// the payment falls short of what the condition advertised between
/// simulation and landing.
#[tokio::test]
async fn guard_reverts_a_crank_that_underpays() {
    let mut h = setup(PAYMENT, 100, TurnerConfig::default());
    h.refresh().await;
    let t0 = h.t0;
    h.add_entry(t0 + 100);
    // The book now pays half of what its condition block still advertises,
    // which is the number the turner asserts.
    h.set_payment(PAYMENT / 2);

    h.warp(t0 + 200, 2);
    let outcomes = h.tick().await;
    assert!(sent(&outcomes).is_empty(), "{outcomes:?}");
    assert!(
        failed(&outcomes).iter().any(|o| matches!(
            o,
            Outcome::Failed {
                stage: Stage::ExecuteSim,
                ..
            }
        )),
        "{outcomes:?}"
    );
    assert_eq!(h.entry_count(), 1, "executor work reverted with the guard");
}

// --- operational behavior ---

/// A source that stalls every simulation, so a sequential turner would
/// take `n * delay` and a concurrent one about `delay`.
#[derive(Clone)]
struct SlowSource {
    inner: LiteSvmSource,
    delay: std::time::Duration,
    peak_concurrent: Arc<Mutex<usize>>,
    live: Arc<Mutex<usize>>,
}

#[async_trait]
impl ChainSource for SlowSource {
    async fn get_multiple_accounts(&self, pubkeys: &[Pubkey]) -> Result<Vec<Option<Account>>> {
        self.inner.get_multiple_accounts(pubkeys).await
    }
    async fn get_watch_accounts(
        &self,
        program: &Pubkey,
        target_programs: &[Pubkey],
    ) -> Result<Vec<(Pubkey, Account)>> {
        self.inner
            .get_watch_accounts(program, target_programs)
            .await
    }
    async fn clock(&self) -> Result<ClockSnapshot> {
        self.inner.clock().await
    }
    async fn latest_blockhash(&self) -> Result<BlockhashInfo> {
        self.inner.latest_blockhash().await
    }
    async fn block_height(&self) -> Result<u64> {
        self.inner.block_height().await
    }
    async fn signature_statuses(
        &self,
        signatures: &[Signature],
    ) -> Result<Vec<Option<SignatureOutcome>>> {
        self.inner.signature_statuses(signatures).await
    }
    async fn simulate_transaction(
        &self,
        tx: &Transaction,
        return_accounts: &[Pubkey],
    ) -> Result<SimOutcome> {
        {
            let mut live = self.live.lock().unwrap();
            *live += 1;
            let mut peak = self.peak_concurrent.lock().unwrap();
            *peak = (*peak).max(*live);
        }
        tokio::time::sleep(self.delay).await;
        *self.live.lock().unwrap() -= 1;
        self.inner.simulate_transaction(tx, return_accounts).await
    }
    async fn send_transaction(&self, tx: &Transaction) -> Result<Signature> {
        self.inner.send_transaction(tx).await
    }
    async fn recent_priority_fee(&self, accounts: &[Pubkey]) -> Result<u64> {
        self.inner.recent_priority_fee(accounts).await
    }
}

/// Conditions are cranked concurrently, not one after another. Every crank
/// is several RPC round trips, so this is the difference between a turner
/// that keeps up and one that falls behind as the registry grows.
#[tokio::test]
async fn conditions_are_cranked_concurrently() {
    let mut h = setup(PAYMENT, 100, TurnerConfig::default());
    let t0 = h.t0;
    h.add_entry(t0 + 100);

    // Independent books: watches dedupe by (target, offset), so repeating
    // one target would collapse to a single condition set.
    (0..4).for_each(|_| {
        let book = h.add_book(PAYMENT, 100);
        h.add_entry_to(book, t0 + 100);
    });

    let peak = Arc::new(Mutex::new(0usize));
    let slow = SlowSource {
        inner: LiteSvmSource::new(&h.svm, &h.watch_keys),
        delay: std::time::Duration::from_millis(60),
        peak_concurrent: Arc::clone(&peak),
        live: Arc::new(Mutex::new(0)),
    };
    let mut turner = Turner::new(slow, Keypair::new(), TurnerConfig::default());
    {
        let mut svm = h.svm.lock().unwrap();
        svm.airdrop(&turner.keeper_pubkey(), 1_000_000_000).unwrap();
    }
    turner.refresh_watches().await.unwrap();

    h.warp(t0 + 200, 2);
    let started = std::time::Instant::now();
    let outcomes = turner.tick().await.unwrap();
    let elapsed = started.elapsed();

    // Five books × two conditions each.
    assert_eq!(outcomes.len(), 10, "{outcomes:?}");
    assert_eq!(sent(&outcomes).len(), 5, "every book swept: {outcomes:?}");
    assert!(
        *peak.lock().unwrap() > 1,
        "simulations should overlap, peak was {}",
        peak.lock().unwrap()
    );
    // Sequential would be at least (conditions × 60ms); concurrent is far
    // less. Loose bound so a slow machine does not flake it.
    assert!(
        elapsed < std::time::Duration::from_millis(60 * 6),
        "tick took {elapsed:?}, suspiciously close to sequential"
    );
}

/// The submitter owns send/confirm: the turner hands off a signed
/// transaction and keeps going, and the transaction still lands.
#[tokio::test]
async fn submitter_lands_transactions_off_the_decision_loop() {
    let mut h = setup(PAYMENT, 100, TurnerConfig::default());
    let t0 = h.t0;
    h.add_entry(t0 + 100);

    let source = Arc::new(LiteSvmSource::new(&h.svm, &h.watch_keys));
    let submitter = relay_crank_turner::spawn_submitter(
        Arc::clone(&source),
        relay_crank_turner::SubmitterConfig {
            blockhash_refresh: std::time::Duration::from_millis(20),
            confirm_interval: std::time::Duration::from_millis(20),
            ..Default::default()
        },
    );
    let mut turner = Turner::new(Arc::clone(&source), Keypair::new(), TurnerConfig::default())
        .with_submitter(submitter.clone());
    {
        let mut svm = h.svm.lock().unwrap();
        svm.airdrop(&turner.keeper_pubkey(), 1_000_000_000).unwrap();
    }
    turner.refresh_watches().await.unwrap();

    h.warp(t0 + 200, 2);
    let outcomes = turner.tick().await.unwrap();
    assert_eq!(sent(&outcomes).len(), 1, "{outcomes:?}");

    // The turner returned before the cluster confirmed; the submitter
    // finishes the job and books the profit.
    let program = demo_id();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while submitter.profit_for(&program) == 0 && std::time::Instant::now() < deadline {
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    assert_eq!(
        submitter.profit_for(&program),
        PAYMENT as i64,
        "a landed crank should book its payment"
    );
    assert_eq!(h.entry_count(), 0, "the work actually landed");
}

/// A program whose recent cranks lost money gets skipped rather than
/// retried forever.
#[tokio::test]
async fn unprofitable_programs_are_skipped() {
    let mut h = setup(PAYMENT, 100, TurnerConfig::default());
    let t0 = h.t0;
    h.add_entry(t0 + 100);

    let source = Arc::new(LiteSvmSource::new(&h.svm, &h.watch_keys));
    let submitter = relay_crank_turner::spawn_submitter(
        Arc::clone(&source),
        relay_crank_turner::SubmitterConfig {
            blockhash_refresh: std::time::Duration::from_millis(20),
            confirm_interval: std::time::Duration::from_millis(20),
            ..Default::default()
        },
    );
    // Demand more profit than this program will ever show.
    let mut turner = Turner::new(
        Arc::clone(&source),
        Keypair::new(),
        TurnerConfig {
            min_program_profit: 1_000_000,
            ..TurnerConfig::default()
        },
    )
    .with_submitter(submitter);
    {
        let mut svm = h.svm.lock().unwrap();
        svm.airdrop(&turner.keeper_pubkey(), 1_000_000_000).unwrap();
    }
    turner.refresh_watches().await.unwrap();

    h.warp(t0 + 200, 2);
    let outcomes = turner.tick().await.unwrap();
    assert!(sent(&outcomes).is_empty(), "{outcomes:?}");
    assert!(
        outcomes
            .iter()
            .any(|o| matches!(o, Outcome::Skipped(_, SkipReason::Unprofitable))),
        "{outcomes:?}"
    );
    assert_eq!(h.entry_count(), 1, "nothing was cranked");
}

/// Metrics record what happened, including the subscription-vs-repoll
/// split that reveals a silently dead stream.
#[tokio::test]
async fn metrics_record_crank_outcomes() {
    let mut h = setup(PAYMENT, 100, TurnerConfig::default());
    h.refresh().await;
    let t0 = h.t0;
    h.add_entry(t0 + 100);
    h.warp(t0 + 200, 2);
    assert_eq!(sent(&h.tick().await).len(), 1);

    let encoded = relay_crank_turner::metrics::encode();
    assert!(
        encoded.contains("relay_cranks_total"),
        "crank counter missing from:\n{encoded}"
    );
    assert!(encoded.contains("relay_tick_seconds"));
    assert!(encoded.contains("relay_watches"));
    // A tick observed at least one crank outcome.
    assert!(
        encoded
            .lines()
            .any(|line| line.starts_with("relay_cranks_total") && !line.ends_with(" 0")),
        "no crank recorded in:\n{encoded}"
    );
}

// --- local simulation ---

/// The whole crank loop — resolver simulation *and* executor simulation —
/// runs in-process. The underlying source panics if asked to simulate, so
/// this fails loudly if anything falls through to the provider.
#[tokio::test]
async fn all_simulation_happens_locally() {
    let mut h = setup(PAYMENT, 100, TurnerConfig::default());
    let t0 = h.t0;
    h.add_entry(t0 + 100);

    let guarded = NoRemoteSimSource {
        inner: LiteSvmSource::new(&h.svm, &h.watch_keys),
        fetches: Arc::new(Mutex::new(0)),
    };
    let local = relay_crank_turner::LocalSimSource::new(
        guarded,
        relay_crank_turner::LocalSimConfig { pool_size: 2 },
    );
    let mut turner = Turner::new(local, Keypair::new(), TurnerConfig::default());
    {
        let mut svm = h.svm.lock().unwrap();
        svm.airdrop(&turner.keeper_pubkey(), 1_000_000_000).unwrap();
    }
    assert_eq!(turner.refresh_watches().await.unwrap().admitted, 1);

    h.warp(t0 + 200, 2);
    let outcomes = turner.tick().await.unwrap();
    assert_eq!(sent(&outcomes).len(), 1, "{outcomes:?}");
    assert_eq!(h.entry_count(), 0, "the crank actually landed on chain");

    let encoded = relay_crank_turner::metrics::encode();
    assert!(
        encoded.contains("relay_simulations_total"),
        "local simulations should be counted"
    );
}

/// A no-work resolve is entirely local: nothing is sent, and the provider
/// is never asked to simulate. This is what makes loose wake hints and
/// frequent `EverySlots` fallbacks affordable.
#[tokio::test]
async fn no_work_resolves_cost_nothing_remote() {
    let mut h = setup(PAYMENT, 100, TurnerConfig::default());
    let t0 = h.t0;
    h.add_entry(t0 + 100);
    h.cancel_entry(1); // stale-early hint: resolver will report no work

    let guarded = NoRemoteSimSource {
        inner: LiteSvmSource::new(&h.svm, &h.watch_keys),
        fetches: Arc::new(Mutex::new(0)),
    };
    let local = relay_crank_turner::LocalSimSource::new(guarded, Default::default());
    let mut turner = Turner::new(local, Keypair::new(), TurnerConfig::default());
    {
        let mut svm = h.svm.lock().unwrap();
        svm.airdrop(&turner.keeper_pubkey(), 1_000_000_000).unwrap();
    }
    turner.refresh_watches().await.unwrap();

    h.warp(t0 + 200, 2);
    let outcomes = turner.tick().await.unwrap();
    assert!(sent(&outcomes).is_empty(), "{outcomes:?}");
    assert!(
        outcomes
            .iter()
            .any(|o| matches!(o, Outcome::NoWork((_, _, 0)))),
        "{outcomes:?}"
    );
}

/// Accounts owned by a watched program are served from the cache without
/// ever refetching, which is what keeps local simulation off the network.
#[tokio::test]
async fn watched_program_accounts_are_never_refetched() {
    let h = setup(PAYMENT, 100, TurnerConfig::default());
    let counted = NoRemoteSimSource {
        inner: LiteSvmSource::new(&h.svm, &h.watch_keys),
        fetches: Arc::new(Mutex::new(0)),
    };
    let fetches = Arc::clone(&counted.fetches);
    let (_sender, receiver) = feed_channel();
    let cached = CachedSource::new(
        counted,
        receiver,
        CachedSourceConfig {
            relay_program: relay_id(),
            // Would otherwise refetch on every read.
            repoll_every: 1,
            watch_programs: vec![demo_id()],
        },
    );

    // First read populates the cache.
    cached.get_multiple_accounts(&[h.book]).await.unwrap();
    let after_first = *fetches.lock().unwrap();

    // The book is owned by a watched program, so subsequent reads are
    // served from cache despite repoll_every = 1.
    for _ in 0..5 {
        let served = cached.get_multiple_accounts(&[h.book]).await.unwrap();
        assert!(served[0].is_some());
    }
    assert_eq!(
        *fetches.lock().unwrap(),
        after_first,
        "watched-program accounts must not be refetched"
    );
}

// --- transaction packing ---

/// Independent cranks ride one transaction, sharing its signature fee.
/// Each keeps its own guard pair, and the guard account is safe to reuse
/// within a transaction because every triple re-arms before its executor.
#[tokio::test]
async fn cranks_pack_into_one_transaction() {
    let mut h = setup(PAYMENT, 100, TurnerConfig::default());
    let t0 = h.t0;
    h.add_entry(t0 + 100);
    let extra: Vec<Pubkey> = (0..2)
        .map(|_| {
            let book = h.add_book(PAYMENT, 100);
            h.add_entry_to(book, t0 + 100);
            book
        })
        .collect();

    let mut turner = Turner::new(
        LiteSvmSource::new(&h.svm, &h.watch_keys),
        Keypair::new(),
        TurnerConfig {
            max_cranks_per_tx: 3,
            ..TurnerConfig::default()
        },
    );
    {
        let mut svm = h.svm.lock().unwrap();
        svm.airdrop(&turner.keeper_pubkey(), 10_000_000_000)
            .unwrap();
    }
    assert_eq!(turner.refresh_watches().await.unwrap().admitted, 3);

    h.warp(t0 + 200, 2);
    let outcomes = turner.tick().await.unwrap();
    let sent_outcomes = sent(&outcomes);
    assert_eq!(sent_outcomes.len(), 3, "{outcomes:?}");

    // All three sweeps landed under one signature.
    let signatures: std::collections::HashSet<String> = sent_outcomes
        .iter()
        .filter_map(|o| match o {
            Outcome::Sent { signature, .. } => Some(signature.to_string()),
            _ => None,
        })
        .collect();
    assert_eq!(signatures.len(), 1, "expected one packed transaction");
    assert_eq!(h.entry_count(), 0);
    extra.iter().for_each(|book| {
        let data = h.svm.lock().unwrap().get_account(book).unwrap().data;
        let count = u32::from_le_bytes(
            data[ENTRY_COUNT_OFFSET..ENTRY_COUNT_OFFSET + 4]
                .try_into()
                .unwrap(),
        );
        assert_eq!(count, 0, "every packed crank did its work");
    });
}

/// `max_cranks_per_tx: 1` disables packing: one transaction each.
#[tokio::test]
async fn packing_can_be_disabled() {
    let mut h = setup(PAYMENT, 100, TurnerConfig::default());
    let t0 = h.t0;
    h.add_entry(t0 + 100);
    (0..2).for_each(|_| {
        let book = h.add_book(PAYMENT, 100);
        h.add_entry_to(book, t0 + 100);
    });

    let mut turner = Turner::new(
        LiteSvmSource::new(&h.svm, &h.watch_keys),
        Keypair::new(),
        TurnerConfig {
            max_cranks_per_tx: 1,
            ..TurnerConfig::default()
        },
    );
    {
        let mut svm = h.svm.lock().unwrap();
        svm.airdrop(&turner.keeper_pubkey(), 10_000_000_000)
            .unwrap();
    }
    turner.refresh_watches().await.unwrap();

    h.warp(t0 + 200, 2);
    let outcomes = turner.tick().await.unwrap();
    let signatures: std::collections::HashSet<String> = sent(&outcomes)
        .iter()
        .filter_map(|o| match o {
            Outcome::Sent { signature, .. } => Some(signature.to_string()),
            _ => None,
        })
        .collect();
    assert_eq!(sent(&outcomes).len(), 3, "{outcomes:?}");
    assert_eq!(signatures.len(), 3, "one transaction per crank");
}
