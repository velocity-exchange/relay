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
    feed_channel, AccountUpdate, CachedSource, CachedSourceConfig, ChainSource, ClockSnapshot,
    Outcome, SimOutcome, SkipReason, Stage, Turner, TurnerConfig,
};
use relay_spec as spec;
use sha2::{Digest, Sha256};
use solana_account::ReadableAccount;
use solana_sdk::account::Account;
use solana_sdk::clock::Clock;
use solana_sdk::hash::Hash;
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
}

#[async_trait]
impl ChainSource for LiteSvmSource {
    async fn get_multiple_accounts(&self, pubkeys: &[Pubkey]) -> Result<Vec<Option<Account>>> {
        let svm = self.svm.lock().unwrap();
        Ok(pubkeys.iter().map(|pk| svm.get_account(pk)).collect())
    }

    async fn get_watch_accounts(&self, program: &Pubkey) -> Result<Vec<(Pubkey, Account)>> {
        let svm = self.svm.lock().unwrap();
        let keys = self.watch_keys.lock().unwrap();
        Ok(keys
            .iter()
            .filter_map(|pk| svm.get_account(pk).map(|acc| (*pk, acc)))
            .filter(|(_, acc)| acc.owner == *program && acc.data.len() == relay_spec::WATCH_V0_LEN)
            .collect())
    }

    async fn clock(&self) -> Result<ClockSnapshot> {
        let clock: Clock = self.svm.lock().unwrap().get_sysvar();
        Ok(ClockSnapshot {
            slot: clock.slot,
            unix_timestamp: clock.unix_timestamp,
        })
    }

    async fn latest_blockhash(&self) -> Result<Hash> {
        let mut svm = self.svm.lock().unwrap();
        svm.expire_blockhash();
        Ok(svm.latest_blockhash())
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
                    accounts,
                }
            }
            Err(failed) => SimOutcome {
                err: Some(format!("{:?}", failed.err)),
                logs: failed.meta.logs,
                return_data: Some(failed.meta.return_data.data),
                accounts: Vec::new(),
            },
        })
    }

    async fn send_transaction(&self, tx: &Transaction) -> Result<Signature> {
        let mut svm = self.svm.lock().unwrap();
        svm.send_transaction(tx.clone())
            .map(|meta| meta.signature)
            .map_err(|failed| anyhow!("send failed: {:?}", failed.err))
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
    let source = LiteSvmSource {
        svm: Arc::clone(&svm),
        watch_keys: Arc::clone(&watch_keys),
    };
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
        self.turner.refresh_watches().await.unwrap()
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
    let balance_before = h.keeper_balance();

    // Past the first expiry: exactly one crank lands, the keeper nets the
    // payment minus the tx fee, and the hint self-repairs to the survivor.
    h.warp(t0 + 150, 2);
    let outcomes = h.tick().await;
    assert_eq!(sent(&outcomes).len(), 1, "{outcomes:?}");
    let delta = h.keeper_balance() as i64 - balance_before as i64;
    assert!(
        delta > 0 && delta <= PAYMENT as i64,
        "keeper delta {delta} should be payment minus fee"
    );
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
    assert_eq!(h.refresh().await, 2);

    let outcomes = h.tick().await;
    assert!(
        outcomes
            .iter()
            .any(|o| matches!(o, Outcome::Skipped(_, SkipReason::ParseFailed))),
        "{outcomes:?}"
    );
    // The real watch still evaluates alongside the junk one.
    assert!(outcomes.len() > 1);
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

    let before = h.keeper_balance();
    h.warp(t0 + 1000, 2);
    let outcomes = h.tick().await;
    assert_eq!(sent(&outcomes).len(), 1, "{outcomes:?}");
    assert!(h.keeper_balance() > before);
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
    let inner = LiteSvmSource {
        svm: Arc::clone(&h.svm),
        watch_keys: Arc::clone(&h.watch_keys),
    };
    let (sender, receiver) = feed_channel();
    let cached = CachedSource::new(
        inner,
        receiver,
        CachedSourceConfig {
            relay_program: relay_id(),
            repoll_every: 0, // never fall back: prove the cache is doing the work
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

    let inner = LiteSvmSource {
        svm: Arc::clone(&h.svm),
        watch_keys: Arc::clone(&h.watch_keys),
    };
    let (_sender, receiver) = feed_channel();
    let cached = CachedSource::new(
        inner,
        receiver,
        CachedSourceConfig {
            relay_program: relay_id(),
            // No live backend in this test, so repoll every read keeps the
            // cache honest against the inner source.
            repoll_every: 1,
        },
    );
    let mut turner = Turner::new(cached, Keypair::new(), TurnerConfig::default());
    {
        let mut svm = h.svm.lock().unwrap();
        svm.airdrop(&turner.keeper_pubkey(), 1_000_000_000).unwrap();
    }
    assert_eq!(turner.refresh_watches().await.unwrap(), 1);

    h.warp(t0 + 200, 2);
    let outcomes = turner.tick().await.unwrap();
    assert_eq!(sent(&outcomes).len(), 1, "{outcomes:?}");
    assert_eq!(h.entry_count(), 0);
}
