//! Program-cache invalidation: an ELF must be re-loaded when the program is
//! upgraded on chain, and *only* then.
//!
//! Both directions are silent when broken, which is why they are pinned
//! here. Never re-loading means every simulation runs code the chain no
//! longer has. Re-loading too often means re-verifying a multi-megabyte ELF
//! on every simulation, which defeats the instance pool while looking
//! exactly like working upgrade detection.
//!
//! Two details make this test faithful, and it proves nothing without them.
//! The programdata account must sit at the derived PDA `[program_id]` under
//! the upgradeable loader, because that is where a real chain puts it *and*
//! where litesvm installs its own copy — the collision is the whole bug.
//! And the reload counter must be read as a delta from its own test binary:
//! it is a process-global static, so a separate binary is what keeps other
//! suites from perturbing it. Within this binary the tests take
//! [`RELOADS_OBSERVED`] for the same reason.

use std::collections::HashMap;
use std::sync::Mutex;

use anyhow::Result;
use async_trait::async_trait;
use relay_chain_source::{
    AccountFilter, ChainSource, ClockSnapshot, LocalSimConfig, LocalSimSource, SignatureOutcome,
    SimOutcome,
};
use solana_sdk::account::Account;
use solana_sdk::instruction::Instruction;
use solana_sdk::message::Message;
use solana_sdk::pubkey::Pubkey;
use solana_sdk::signature::{Keypair, Signature, Signer};
use solana_sdk::transaction::Transaction;

use relay_test_fixtures::DEMO_BOOK_SO;
/// demo-book's declared id. Using the real one matters: an anchor program
/// checks it, so a program loaded at some other address never dispatches.
const DEMO_ID: &str = "6PqZZeykcFwncPxs5LjjxzQshdRV29mpsFtmT3QS9jRZ";
/// `UpgradeableLoaderState` discriminants.
const PROGRAM_TAG: u32 = 2;
const PROGRAMDATA_TAG: u32 = 3;
const FIRST_DEPLOY_SLOT: u64 = 1_000;

fn upgradeable_loader() -> Pubkey {
    "BPFLoaderUpgradeab1e11111111111111111111111"
        .parse()
        .unwrap()
}

/// The accounts a chain would serve, with a programdata slot the test can
/// advance to stand in for a redeploy.
struct FakeChain {
    accounts: Mutex<HashMap<Pubkey, Account>>,
    programdata: Pubkey,
    /// Stop serving the programdata account, standing in for a fetch that
    /// failed or a provider that returned nothing for it.
    hide_programdata: Mutex<bool>,
}

impl FakeChain {
    fn new(program: Pubkey, elf: Vec<u8>) -> Self {
        // Exactly where a real chain keeps it, and exactly where litesvm
        // will install its own — see the module docs.
        let programdata =
            Pubkey::find_program_address(&[program.as_ref()], &upgradeable_loader()).0;

        let mut program_data = PROGRAM_TAG.to_le_bytes().to_vec();
        program_data.extend_from_slice(programdata.as_ref());

        let accounts = HashMap::from([
            (
                program,
                Account {
                    lamports: 1_000_000_000,
                    data: program_data,
                    owner: upgradeable_loader(),
                    executable: true,
                    rent_epoch: 0,
                },
            ),
            (
                programdata,
                Account {
                    lamports: 1_000_000_000,
                    data: programdata_bytes(FIRST_DEPLOY_SLOT, &elf),
                    owner: upgradeable_loader(),
                    executable: false,
                    rent_epoch: 0,
                },
            ),
        ]);
        Self {
            accounts: Mutex::new(accounts),
            programdata,
            hide_programdata: Mutex::new(false),
        }
    }

    fn hide_programdata(&self, hide: bool) {
        *self.hide_programdata.lock().unwrap() = hide;
    }

    /// Stand in for `solana program deploy` on an existing program: same
    /// address, new ELF, and a deploy slot that has moved on.
    fn redeploy(&self, slot: u64, elf: &[u8]) {
        let mut accounts = self.accounts.lock().unwrap();
        let account = accounts.get_mut(&self.programdata).expect("programdata");
        account.data = programdata_bytes(slot, elf);
    }
}

/// `tag (4) + slot (8) + Option<Pubkey> authority (1 + 32) + ELF`.
fn programdata_bytes(slot: u64, elf: &[u8]) -> Vec<u8> {
    let mut data = PROGRAMDATA_TAG.to_le_bytes().to_vec();
    data.extend_from_slice(&slot.to_le_bytes());
    data.push(1);
    data.extend_from_slice(&[0u8; 32]);
    data.extend_from_slice(elf);
    data
}

#[async_trait]
impl ChainSource for FakeChain {
    async fn get_multiple_accounts(&self, pubkeys: &[Pubkey]) -> Result<Vec<Option<Account>>> {
        let accounts = self.accounts.lock().unwrap();
        let hidden = *self.hide_programdata.lock().unwrap();
        Ok(pubkeys
            .iter()
            .map(|pubkey| {
                if hidden && *pubkey == self.programdata {
                    return None;
                }
                accounts.get(pubkey).cloned().or_else(|| {
                    // Anything else — the fee payer — is a plain funded
                    // account, so the transaction can be paid for.
                    Some(Account {
                        lamports: 10_000_000_000,
                        ..Default::default()
                    })
                })
            })
            .collect())
    }

    async fn get_program_accounts(
        &self,
        _program: &Pubkey,
        _filter_sets: &[Vec<AccountFilter>],
    ) -> Result<Vec<(Pubkey, Account)>> {
        Ok(Vec::new())
    }

    async fn clock(&self) -> Result<ClockSnapshot> {
        Ok(ClockSnapshot {
            slot: 42,
            unix_timestamp: 1_700_000_000,
        })
    }

    async fn latest_blockhash(&self) -> Result<relay_chain_source::BlockhashInfo> {
        unimplemented!("local simulation never asks for a blockhash")
    }

    async fn block_height(&self) -> Result<u64> {
        Ok(1)
    }

    async fn signature_statuses(
        &self,
        signatures: &[Signature],
    ) -> Result<Vec<Option<SignatureOutcome>>> {
        Ok(vec![None; signatures.len()])
    }

    async fn simulate_transaction(
        &self,
        _tx: &Transaction,
        _return_accounts: &[Pubkey],
    ) -> Result<SimOutcome> {
        unimplemented!("LocalSimSource simulates; it never delegates")
    }

    async fn send_transaction(&self, _tx: &Transaction) -> Result<Signature> {
        unimplemented!("this test never sends")
    }

    async fn recent_priority_fee(&self, _accounts: &[Pubkey]) -> Result<u64> {
        Ok(0)
    }
}

/// Held for the whole of every test that reads [`reloads`].
///
/// The counter is process-global and keyed by program id, and every test here
/// drives the *same* program: an anchor program checks its own address, so it
/// only dispatches when loaded at the one it declares, and the tests cannot
/// each take a program of their own. Cargo runs them in parallel, so without
/// this a reload one test triggers lands inside another's before/after window
/// and is counted against it — the redeploy test's single legitimate reload
/// failing the two that assert no reload happened. Under load that is most of
/// the time; on an idle machine it passes, which is the worst version of it.
static RELOADS_OBSERVED: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

fn reloads() -> u64 {
    relay_chain_source::metrics::PROGRAM_RELOADS
        .with_label_values(&[&relay_chain_source::metrics::program_label(
            &DEMO_ID.parse().unwrap(),
        )])
        .get()
}

/// One simulation naming the program, so `load_program` runs for it. The
/// instruction data is deliberate nonsense: the program rejects it, and a
/// rejected instruction still required the ELF to be loaded and dispatched,
/// which is all this test cares about.
async fn simulate(source: &LocalSimSource<FakeChain>, program: Pubkey) -> SimOutcome {
    let payer = Keypair::new();
    let ix = Instruction {
        program_id: program,
        accounts: vec![],
        data: vec![7; 8],
    };
    let message = Message::new(&[ix], Some(&payer.pubkey()));
    let tx = Transaction::new_unsigned(message);
    source
        .simulate_transaction(&tx, &[])
        .await
        .expect("simulation runs")
}

#[tokio::test]
async fn a_steady_program_is_loaded_once_and_then_left_alone() {
    let _counter = RELOADS_OBSERVED.lock().await;
    let program: Pubkey = DEMO_ID.parse().unwrap();
    let elf = relay_test_fixtures::elf(DEMO_BOOK_SO);
    let source = LocalSimSource::new(
        FakeChain::new(program, elf),
        LocalSimConfig {
            pool_size: 1,
            ..LocalSimConfig::default()
        },
    );

    let before = reloads();
    let first = simulate(&source, program).await;
    // Dispatched into the program rather than bouncing off the loader: an
    // unrecognised discriminator is the program's own rejection, whereas
    // IncorrectProgramId would mean the ELF never ran and this test would
    // be measuring nothing.
    assert!(
        first.err.is_some(),
        "nonsense instruction data should be rejected"
    );
    assert!(
        first.units_consumed > 0,
        "the program never executed, so nothing here is being tested: {first:?}"
    );

    // Nothing has been upgraded, so no amount of simulating may re-load.
    for _ in 0..5 {
        simulate(&source, program).await;
    }
    assert_eq!(
        reloads() - before,
        0,
        "the ELF was re-verified without an upgrade — the pool is defeated and \
         every simulation is paying full program-load cost"
    );
}

#[tokio::test]
async fn a_redeploy_is_detected_exactly_once() {
    let _counter = RELOADS_OBSERVED.lock().await;
    let program: Pubkey = DEMO_ID.parse().unwrap();
    let elf = relay_test_fixtures::elf(DEMO_BOOK_SO);
    let chain = FakeChain::new(program, elf.clone());
    let source = LocalSimSource::new(
        chain,
        LocalSimConfig {
            pool_size: 1,
            ..LocalSimConfig::default()
        },
    );

    let before = reloads();
    simulate(&source, program).await;
    assert_eq!(reloads() - before, 0, "the first load is not a re-load");

    // Redeploy: same ELF is fine, the deploy slot is what moves.
    source.inner().redeploy(FIRST_DEPLOY_SLOT + 1, &elf);
    simulate(&source, program).await;
    assert_eq!(
        reloads() - before,
        1,
        "an upgrade went undetected: simulation would keep running the old ELF"
    );

    // And it settles: the new slot is now the cached one.
    for _ in 0..3 {
        simulate(&source, program).await;
    }
    assert_eq!(
        reloads() - before,
        1,
        "detection did not settle after the upgrade was picked up"
    );
}

/// Losing sight of the deploy slot must not churn the cache.
///
/// The bank holds its own programdata copy at the same derived address,
/// stamped with the simulator's clock rather than the chain's deploy slot, so
/// a version check that fell back to it would report an upgrade on every tick
/// the clock advanced — turning a failed fetch into a permanent full
/// program load per simulation. Unknown has to mean "change nothing".
#[tokio::test]
async fn losing_the_programdata_read_does_not_churn_the_cache() {
    let _counter = RELOADS_OBSERVED.lock().await;
    let program: Pubkey = DEMO_ID.parse().unwrap();
    let elf = relay_test_fixtures::elf(DEMO_BOOK_SO);
    let source = LocalSimSource::new(
        FakeChain::new(program, elf),
        LocalSimConfig {
            pool_size: 1,
            ..LocalSimConfig::default()
        },
    );

    let before = reloads();
    simulate(&source, program).await;

    // The provider stops serving programdata. The ELF is already in the bank,
    // so simulation carries on — but the version is now unknown.
    source.inner().hide_programdata(true);
    for _ in 0..5 {
        simulate(&source, program).await;
    }
    assert_eq!(
        reloads() - before,
        0,
        "an unreadable deploy slot was treated as a version change"
    );

    // And once it comes back, an unchanged slot is still no reload.
    source.inner().hide_programdata(false);
    simulate(&source, program).await;
    assert_eq!(reloads() - before, 0);
}

/// A loader this simulator cannot host must not be filed as a builtin.
///
/// litesvm rejects every loader but the two BPF ones, so a loader-v4 program
/// cannot be loaded at all. Treating it as "already present" made that
/// silent: nothing loaded, and the failure surfaced later as an unrelated
/// simulation error with no hint about the cause.
#[tokio::test]
async fn an_unhostable_loader_fails_the_simulation_rather_than_pretending() {
    let _counter = RELOADS_OBSERVED.lock().await;
    let program: Pubkey = DEMO_ID.parse().unwrap();
    let elf = relay_test_fixtures::elf(DEMO_BOOK_SO);
    let chain = FakeChain::new(program, elf);
    // Re-own the program account by loader v4, which litesvm has no support
    // for. The ELF layout differs too (header, then bytes, inside the program
    // account), so there is nothing to salvage here — only to report.
    {
        let mut accounts = chain.accounts.lock().unwrap();
        let account = accounts.get_mut(&program).expect("program");
        account.owner = "LoaderV411111111111111111111111111111111111"
            .parse()
            .unwrap();
    }
    let source = LocalSimSource::new(
        chain,
        LocalSimConfig {
            pool_size: 1,
            ..LocalSimConfig::default()
        },
    );

    let outcome = simulate(&source, program).await;
    assert!(
        outcome.err.is_some(),
        "an unhostable program must fail the simulation, not appear to work: {outcome:?}"
    );
    // Nothing ran, so nothing was metered.
    assert_eq!(
        outcome.units_consumed, 0,
        "the program should never have executed: {outcome:?}"
    );
    // And it is never counted as a re-load, however many times it is seen.
    let before = reloads();
    for _ in 0..3 {
        simulate(&source, program).await;
    }
    assert_eq!(reloads() - before, 0);
}
