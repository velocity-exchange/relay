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
//! suites from perturbing it.
//!
//! Requires the SBF build: `./scripts/build-programs.sh`.

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

const DEMO_SO: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../programs/target/deploy/demo_book.so"
);
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
        }
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
        Ok(pubkeys
            .iter()
            .map(|pubkey| {
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
    let program: Pubkey = DEMO_ID.parse().unwrap();
    let elf = std::fs::read(DEMO_SO).expect("demo_book.so; run scripts/build-programs.sh");
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
    let program: Pubkey = DEMO_ID.parse().unwrap();
    let elf = std::fs::read(DEMO_SO).expect("demo_book.so; run scripts/build-programs.sh");
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
