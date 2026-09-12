//! Local simulation: run resolver and executor simulations in an in-process
//! SVM instead of shipping every one to an RPC provider.
//!
//! Simulation is the turner's core loop — a resolver sim to discover work,
//! then an executor sim to verify payment, for every due condition, every
//! tick. Sending those to RPC costs 50-200ms each and is metered; running
//! them locally costs microseconds and nothing. That changes what the
//! turner can afford: wake hints can be loose, `EverySlots` fallbacks can
//! be frequent, and a no-work resolve becomes genuinely free.
//!
//! This works because a Solana transaction declares its entire account set
//! upfront, so a *lazy fork* is enough — no global state, no validator.
//! For each transaction: collect its account keys, populate them into a
//! pooled [`LiteSVM`] from the cache (falling back to the inner source for
//! anything cold), sync the clock, and execute. The same pattern as
//! anvil's mainnet-fork mode.
//!
//! It is wired as a [`ChainSource`] decorator, so it composes:
//! `LocalSimSource<CachedSource<RpcSource>>` reads through the
//! subscription cache and simulates in-process, while sends, blockhashes
//! and confirmations still go to the inner source. Accuracy is bounded by
//! cache freshness, which is the same bound the decision already had —
//! and the chain re-runs everything authoritatively at land time anyway.
//!
//! Keeping the pooled banks' SBF programs in step with the chain is a
//! subject of its own and lives in `programs`.

use std::collections::{HashMap, HashSet};

use anyhow::Result;
use async_trait::async_trait;
use litesvm::LiteSVM;
use solana_sdk::account::Account;
use solana_sdk::clock::Clock;
use solana_sdk::pubkey::Pubkey;
use solana_sdk::signature::Signature;
use solana_sdk::transaction::VersionedTransaction;
use tokio::sync::Mutex;
use tracing::warn;

use crate::metrics;
use crate::source::{
    AccountFilter, BlockhashInfo, ChainSource, ClockSnapshot, SignatureOutcome, SimOutcome,
};

mod programs;

#[cfg(test)]
use programs::PROGRAMDATA_ELF_OFFSET;
use programs::{BPF_LOADER, BPF_LOADER_UPGRADEABLE, NATIVE_LOADER};

#[derive(Debug, Clone)]
pub struct LocalSimConfig {
    /// How many SVM instances to keep warm. Each retains its loaded
    /// program cache, so reusing them avoids re-verifying ELFs (the
    /// expensive part — velocity's is megabytes).
    pub pool_size: usize,
    /// Credit the fee payer this many lamports **inside the simulation
    /// bank** when it has no account on chain.
    ///
    /// Off by default, and it must stay off for a turner: a keeper that has
    /// run out of SOL should fail loudly rather than simulate as though it
    /// had not. It exists for read-only inspection, where the caller holds
    /// no key at all and a synthetic payer is the only way to reach the
    /// interesting part of the simulation.
    pub synthetic_fee_payer_lamports: Option<u64>,
}

impl Default for LocalSimConfig {
    fn default() -> Self {
        Self {
            pool_size: 8,
            synthetic_fee_payer_lamports: None,
        }
    }
}

/// One pooled bank plus the set of programs already loaded into it.
/// Everything one simulation needs from the chain, gathered before the bank
/// is touched. Bundled so the seeding map travels with the accounts it was
/// fetched alongside, rather than as another positional argument.
struct Snapshot<'a> {
    keys: &'a [Pubkey],
    accounts: &'a [Option<Account>],
    clock: &'a ClockSnapshot,
    /// Programdata fetched this tick, by address. The authority on which
    /// version of a program is deployed — see `deploy_slot`.
    programdata: &'a HashMap<Pubkey, Account>,
}

struct Instance {
    svm: LiteSVM,
    programs: HashSet<Pubkey>,
    /// Tracks the most recent on-chain slot at which each upgradeable
    /// program's ELF was loaded. Compared on every cache lookup to detect
    /// upgrades — if the slot in the programdata account has advanced, the
    /// program was redeployed and must be re-loaded.
    program_slots: HashMap<Pubkey, u64>,
    /// Programs whose loader this simulator cannot host, so the warning is
    /// emitted once rather than on every tick.
    unhostable: HashSet<Pubkey>,
}

impl Instance {
    fn new() -> Self {
        Self {
            // Transactions arrive signed against the *chain's* blockhash by
            // a keypair this bank has never seen, so both checks have to be
            // off; neither is what we are simulating for.
            svm: LiteSVM::new()
                .with_sigverify(false)
                .with_blockhash_check(false),
            programs: HashSet::new(),
            program_slots: HashMap::new(),
            unhostable: HashSet::new(),
        }
    }
}

pub struct LocalSimSource<Inner> {
    inner: Inner,
    config: LocalSimConfig,
    /// Idle instances. The lock is held only to take and return one, never
    /// across a fetch or an execution.
    pool: Mutex<Vec<Instance>>,
}

impl<Inner: ChainSource> LocalSimSource<Inner> {
    pub fn new(inner: Inner, config: LocalSimConfig) -> Self {
        Self {
            inner,
            config,
            pool: Mutex::new(Vec::new()),
        }
    }

    pub fn inner(&self) -> &Inner {
        &self.inner
    }

    async fn take(&self) -> Instance {
        self.pool.lock().await.pop().unwrap_or_else(Instance::new)
    }

    async fn put(&self, instance: Instance) {
        let mut pool = self.pool.lock().await;
        if pool.len() < self.config.pool_size {
            pool.push(instance);
        }
    }

    /// Populate `instance` with everything `tx` touches, then run it.
    async fn simulate_locally(
        &self,
        tx: &VersionedTransaction,
        keys: &[Pubkey],
        accounts: &[Option<Account>],
        return_accounts: &[Pubkey],
        programdata: &HashMap<Pubkey, Account>,
    ) -> Result<SimOutcome> {
        let clock = self.inner.clock().await?;

        let mut instance = self.take().await;
        let chain = Snapshot {
            keys,
            accounts,
            clock: &clock,
            programdata,
        };
        let outcome = self.load_and_run(&mut instance, tx, &chain, return_accounts);
        self.put(instance).await;
        outcome
    }

    /// Seed the bank from the chain snapshot, then run the transaction.
    fn load_and_run(
        &self,
        instance: &mut Instance,
        tx: &VersionedTransaction,
        chain: &Snapshot<'_>,
        return_accounts: &[Pubkey],
    ) -> Result<SimOutcome> {
        // Chain time drives every wake and most executor logic.
        instance.svm.set_sysvar(&Clock {
            slot: chain.clock.slot,
            unix_timestamp: chain.clock.unix_timestamp,
            ..Default::default()
        });
        self.seed_accounts(instance, chain);
        self.seed_fee_payer(instance, chain);
        Ok(outcome(
            instance.svm.simulate_transaction(tx.clone()),
            return_accounts,
        ))
    }

    /// Put the chain's version of every account the transaction names into
    /// the bank, loading programs rather than copying their bytes.
    fn seed_accounts(&self, instance: &mut Instance, chain: &Snapshot<'_>) {
        for (key, account) in chain.keys.iter().zip(chain.accounts.iter()) {
            let Some(account) = account else {
                // The chain says there is no such account. A pooled bank
                // may still be holding one from an earlier simulation —
                // banks are reused precisely so programs stay loaded — and
                // leaving that copy in place simulates a world where a
                // closed account is still open.
                forget_account(instance, key);
                continue;
            };
            if account.executable {
                self.load_program(instance, key, account, chain.programdata);
                continue;
            }
            if let Err(err) = instance.svm.set_account(*key, account.clone()) {
                warn!(account = %key, error = ?err, "local sim could not seed account");
            }
        }
    }

    /// Credit a fee payer that does not exist on chain, for read-only
    /// inspection where the caller holds no key.
    ///
    /// Runs after the seeding pass, which would otherwise clear it. A fee
    /// payer with no account cannot pay, and litesvm rejects the transaction
    /// before any instruction runs.
    fn seed_fee_payer(&self, instance: &mut Instance, chain: &Snapshot<'_>) {
        let Some(lamports) = self.config.synthetic_fee_payer_lamports else {
            return;
        };
        let payer_missing = chain.accounts.first().is_none_or(Option::is_none);
        if let (true, Some(payer)) = (payer_missing, chain.keys.first()) {
            let synthetic = Account {
                lamports,
                ..Default::default()
            };
            if let Err(err) = instance.svm.set_account(*payer, synthetic) {
                warn!(account = %payer, error = ?err, "could not seed a synthetic fee payer");
            }
        }
    }
}

/// Translate litesvm's result into a [`SimOutcome`], picking out the
/// post-execution state of the accounts the caller asked to see.
///
/// A failed simulation returns no accounts: its post-state is the state
/// before the transaction, which a caller reading back a staged payload must
/// not mistake for an answer.
fn outcome(
    result: Result<
        litesvm::types::SimulatedTransactionInfo,
        litesvm::types::FailedTransactionMetadata,
    >,
    return_accounts: &[Pubkey],
) -> SimOutcome {
    match result {
        Ok(info) => SimOutcome {
            err: None,
            logs: info.meta.logs,
            return_data: Some(info.meta.return_data.data),
            units_consumed: info.meta.compute_units_consumed,
            accounts: return_accounts
                .iter()
                .map(|wanted| {
                    info.post_accounts
                        .iter()
                        .find(|(address, _)| address == wanted)
                        .map(|(_, account)| to_account(account))
                })
                .collect(),
        },
        Err(failed) => SimOutcome {
            err: Some(format!("{:?}", failed.err)),
            logs: failed.meta.logs,
            return_data: Some(failed.meta.return_data.data),
            units_consumed: failed.meta.compute_units_consumed,
            accounts: Vec::new(),
        },
    }
}

/// Drop a pooled bank's copy of an account the chain no longer has.
///
/// Overwriting with a zeroed system-owned account is how a bank spells
/// "absent": there is no remove, and the state a transaction sees for a
/// zero-lamport, zero-data account is the state it sees for one that never
/// existed.
fn forget_account(instance: &mut Instance, key: &Pubkey) {
    let held = instance.svm.get_account(key).is_some_and(|account| {
        // Never programs. A bank comes with its builtins installed and
        // keeps loaded ELFs on purpose, and none of them are in the
        // account map a source answers from — clearing those would empty
        // the bank of the very things pooling exists to retain.
        !account.executable
            && account.owner != *NATIVE_LOADER
            && account.owner != *BPF_LOADER
            && account.owner != *BPF_LOADER_UPGRADEABLE
            && (account.lamports > 0 || !account.data.is_empty())
    });
    if !held {
        return;
    }
    if let Err(err) = instance.svm.set_account(*key, Account::default()) {
        warn!(account = %key, error = ?err, "local sim could not clear a closed account");
    }
}

fn to_account(account: &solana_account::AccountSharedData) -> Account {
    use solana_account::ReadableAccount;
    Account {
        lamports: account.lamports(),
        data: account.data().to_vec(),
        owner: Pubkey::new_from_array(account.owner().to_bytes()),
        executable: account.executable(),
        rent_epoch: account.rent_epoch(),
    }
}

#[async_trait]
impl<Inner: ChainSource> ChainSource for LocalSimSource<Inner> {
    async fn get_multiple_accounts(&self, pubkeys: &[Pubkey]) -> Result<Vec<Option<Account>>> {
        self.inner.get_multiple_accounts(pubkeys).await
    }

    async fn get_program_accounts(
        &self,
        program: &Pubkey,
        filter_sets: &[Vec<AccountFilter>],
    ) -> Result<Vec<(Pubkey, Account)>> {
        self.inner.get_program_accounts(program, filter_sets).await
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

    /// The whole point: simulate in-process, never over RPC.
    async fn simulate_transaction(
        &self,
        tx: &VersionedTransaction,
        return_accounts: &[Pubkey],
    ) -> Result<SimOutcome> {
        // One read of everything the transaction touches. Cache-first, so
        // only genuinely cold accounts reach the network. A v1 message
        // carries every address inline, so the static keys are the whole
        // set.
        let keys: Vec<Pubkey> = tx.message.static_account_keys().to_vec();
        let accounts = self.inner.get_multiple_accounts(&keys).await?;
        // Programs the transaction can reach may need their programdata
        // seeded before their ELF can be recovered. That is every
        // executable account in the key list, not just top-level program
        // ids — a CPI target rides along as a plain account meta.
        let programdata = match self.seed_programdata(&accounts).await {
            Ok(found) => found,
            Err(err) => {
                warn!(error = %format!("{err:#}"), "programdata seed failed; simulating anyway");
                HashMap::new()
            }
        };
        let outcome = self
            .simulate_locally(tx, &keys, &accounts, return_accounts, &programdata)
            .await;
        metrics::SIMULATIONS
            .with_label_values(&[if outcome.is_ok() { "local" } else { "error" }])
            .inc();
        outcome
    }

    async fn send_transaction(&self, tx: &VersionedTransaction) -> Result<Signature> {
        self.inner.send_transaction(tx).await
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use solana_sdk::hash::Hash;
    use solana_sdk::instruction::{AccountMeta, Instruction};
    use solana_sdk::signature::{Keypair, Signer};
    use solana_sdk::transaction::Transaction;

    use super::*;
    use crate::source::{
        AccountFilter, BlockhashInfo, ClockSnapshot, SignatureOutcome, SimOutcome,
    };

    /// An account map the test can change between simulations; everything
    /// else is unreachable in these tests.
    struct MapSource(std::sync::Mutex<HashMap<Pubkey, Account>>);

    impl MapSource {
        fn new(accounts: HashMap<Pubkey, Account>) -> Self {
            Self(std::sync::Mutex::new(accounts))
        }

        fn close(&self, pubkey: &Pubkey) {
            self.0.lock().unwrap().remove(pubkey);
        }
    }

    #[async_trait]
    impl ChainSource for MapSource {
        async fn get_multiple_accounts(&self, pubkeys: &[Pubkey]) -> Result<Vec<Option<Account>>> {
            let accounts = self.0.lock().unwrap();
            Ok(pubkeys.iter().map(|pk| accounts.get(pk).cloned()).collect())
        }
        async fn get_program_accounts(
            &self,
            _program: &Pubkey,
            _filter_sets: &[Vec<AccountFilter>],
        ) -> Result<Vec<(Pubkey, Account)>> {
            unreachable!()
        }
        async fn clock(&self) -> Result<ClockSnapshot> {
            Ok(ClockSnapshot {
                slot: 1,
                unix_timestamp: 1,
            })
        }
        async fn latest_blockhash(&self) -> Result<BlockhashInfo> {
            unreachable!()
        }
        async fn block_height(&self) -> Result<u64> {
            unreachable!()
        }
        async fn signature_statuses(
            &self,
            _signatures: &[Signature],
        ) -> Result<Vec<Option<SignatureOutcome>>> {
            unreachable!()
        }
        async fn simulate_transaction(
            &self,
            _tx: &VersionedTransaction,
            _return_accounts: &[Pubkey],
        ) -> Result<SimOutcome> {
            unreachable!("local sim must never fall through to the provider")
        }
        async fn send_transaction(&self, _tx: &VersionedTransaction) -> Result<Signature> {
            unreachable!()
        }
        async fn recent_priority_fee(&self, _accounts: &[Pubkey]) -> Result<u64> {
            unreachable!()
        }
    }

    /// An upgradeable-loader (program, programdata) pair holding `elf`.
    fn upgradeable_program(elf: &[u8]) -> (Account, Pubkey, Account) {
        let programdata_key = Pubkey::new_unique();
        let mut program_data = vec![2, 0, 0, 0];
        program_data.extend_from_slice(programdata_key.as_ref());
        let program = Account {
            lamports: 1,
            data: program_data,
            owner: *BPF_LOADER_UPGRADEABLE,
            executable: true,
            rent_epoch: 0,
        };
        let mut pd_data = vec![0u8; PROGRAMDATA_ELF_OFFSET];
        pd_data.extend_from_slice(elf);
        let programdata = Account {
            lamports: 1,
            data: pd_data,
            owner: *BPF_LOADER_UPGRADEABLE,
            executable: false,
            rent_epoch: 0,
        };
        (program, programdata_key, programdata)
    }

    /// An upgradeable program reachable only as a CPI target — an account
    /// meta on some instruction, never a top-level program id — still gets
    /// its programdata seeded and its ELF loaded. This is exactly the shape
    /// of a staged executor that CPIs into a registered quoter program: the
    /// old top-level-only seeding left the target uninvokable and every
    /// executor simulation failing `UnsupportedProgramId`.
    #[tokio::test]
    async fn cpi_target_programs_get_their_programdata_seeded() {
        let elf = relay_test_fixtures::elf(relay_test_fixtures::DEMO_BOOK_SO);

        let payer = Keypair::new();
        let cpi_target = Pubkey::new_unique();
        let (program, programdata_key, programdata) = upgradeable_program(&elf);
        let mut accounts = HashMap::new();
        accounts.insert(
            payer.pubkey(),
            Account {
                lamports: 1_000_000_000,
                ..Default::default()
            },
        );
        accounts.insert(cpi_target, program);
        accounts.insert(programdata_key, programdata);
        let local = LocalSimSource::new(MapSource::new(accounts), LocalSimConfig::default());

        // The target rides along as a plain meta, the way an executor's
        // account list carries the program it will CPI. The instruction
        // itself is a compute-budget builtin, which ignores its accounts —
        // the point is only that `cpi_target` is never a top-level program.
        let ix = Instruction {
            program_id: "ComputeBudget111111111111111111111111111111"
                .parse()
                .unwrap(),
            accounts: vec![AccountMeta::new_readonly(cpi_target, false)],
            // SetComputeUnitLimit(1_000_000)
            data: vec![2, 0x40, 0x42, 0x0F, 0x00],
        };
        let tx = Transaction::new_signed_with_payer(
            &[ix],
            Some(&payer.pubkey()),
            &[&payer],
            Hash::default(),
        );

        let outcome = local
            .simulate_transaction(&VersionedTransaction::from(tx), &[])
            .await
            .unwrap();
        assert_eq!(outcome.err, None, "logs: {:?}", outcome.logs);
        let instance = local.pool.lock().await.pop().expect("instance pooled");
        assert!(
            instance.programs.contains(&cpi_target),
            "CPI-target ELF was not loaded into the local sim"
        );
    }

    /// Banks are pooled and reused, so what one simulation seeds is still
    /// there for the next one. An account that has since been closed on
    /// chain must not survive that reuse: the next simulation would decide
    /// against a world where it is still open, which is exactly the class
    /// of wrong answer local simulation is supposed to avoid.
    #[tokio::test]
    async fn a_closed_account_does_not_survive_in_a_pooled_bank() {
        let payer = Keypair::new();
        let closing = Pubkey::new_unique();
        let mut accounts = HashMap::new();
        accounts.insert(
            payer.pubkey(),
            Account {
                lamports: 1_000_000_000,
                ..Default::default()
            },
        );
        accounts.insert(
            closing,
            Account {
                lamports: 5_000_000,
                data: vec![7u8; 32],
                ..Default::default()
            },
        );
        let source = MapSource::new(accounts);
        let local = LocalSimSource::new(
            source,
            LocalSimConfig {
                pool_size: 1,
                ..LocalSimConfig::default()
            },
        );

        let tx = |payer: &Keypair| {
            Transaction::new_signed_with_payer(
                &[Instruction {
                    program_id: "ComputeBudget111111111111111111111111111111"
                        .parse()
                        .unwrap(),
                    accounts: vec![AccountMeta::new_readonly(closing, false)],
                    // SetComputeUnitLimit(1_000_000)
                    data: vec![2, 0x40, 0x42, 0x0F, 0x00],
                }],
                Some(&payer.pubkey()),
                &[payer],
                Hash::default(),
            )
        };

        local
            .simulate_transaction(&VersionedTransaction::from(tx(&payer)), &[])
            .await
            .unwrap();
        {
            let pool = local.pool.lock().await;
            let seeded = pool[0].svm.get_account(&closing).expect("seeded");
            assert_eq!(seeded.lamports, 5_000_000);
        }

        local.inner().close(&closing);
        local
            .simulate_transaction(&VersionedTransaction::from(tx(&payer)), &[])
            .await
            .unwrap();
        let pool = local.pool.lock().await;
        let after = pool[0].svm.get_account(&closing).unwrap_or_default();
        assert_eq!(after.lamports, 0, "closed account survived in the bank");
        assert!(after.data.is_empty(), "closed account kept its data");
    }
}
