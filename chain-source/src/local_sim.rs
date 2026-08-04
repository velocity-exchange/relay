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

use std::collections::{HashMap, HashSet};
use std::sync::LazyLock;

use anyhow::Result;
use async_trait::async_trait;
use litesvm::LiteSVM;
use solana_sdk::account::Account;
use solana_sdk::clock::Clock;
use solana_sdk::pubkey::Pubkey;
use solana_sdk::signature::Signature;
use solana_sdk::transaction::Transaction;
use tokio::sync::Mutex;
use tracing::{debug, info, warn};

use crate::metrics;
use crate::source::{
    AccountFilter, BlockhashInfo, ChainSource, ClockSnapshot, SignatureOutcome, SimOutcome,
};

/// Loader whose program accounts hold the ELF directly.
static BPF_LOADER: LazyLock<Pubkey> = LazyLock::new(|| {
    "BPFLoader2111111111111111111111111111111111"
        .parse()
        .unwrap()
});
/// Upgradeable loader: the ELF lives in a separate programdata account
/// named by the program account.
static BPF_LOADER_UPGRADEABLE: LazyLock<Pubkey> = LazyLock::new(|| {
    "BPFLoaderUpgradeab1e11111111111111111111111"
        .parse()
        .unwrap()
});

/// Builtins (system, compute budget, ...) are already in every bank.
static NATIVE_LOADER: LazyLock<Pubkey> = LazyLock::new(|| {
    "NativeLoader1111111111111111111111111111111"
        .parse()
        .unwrap()
});

/// Loader v4. Recognised only so it can be reported as unhostable: litesvm
/// rejects every loader but the two BPF ones with `InvalidLoader`, and a v4
/// program keeps its ELF (and its deploy slot) inside the program account
/// behind a 48-byte header rather than in a separate programdata account.
/// Both the load and the upgrade detection would need new code, and neither
/// can be exercised until the simulator can host one.
static LOADER_V4: LazyLock<Pubkey> = LazyLock::new(|| {
    "LoaderV411111111111111111111111111111111111"
        .parse()
        .unwrap()
});
/// Offset of the ELF inside an upgradeable programdata account:
/// `enum tag (4) + slot (8) + Option<Pubkey> authority (1 + 32)`.
const PROGRAMDATA_ELF_OFFSET: usize = 45;
/// Slot inside an upgradeable programdata account, right after the tag.
const PROGRAMDATA_SLOT_OFFSET: usize = 4;
/// Offset of the programdata address inside a `Program` account:
/// `enum tag (4)`.
const PROGRAM_PROGRAMDATA_OFFSET: usize = 4;

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
        tx: &Transaction,
        return_accounts: &[Pubkey],
        programdata: &HashMap<Pubkey, Account>,
    ) -> Result<SimOutcome> {
        let keys: Vec<Pubkey> = tx.message.account_keys.clone();
        // Cache-first; only genuinely cold accounts reach the network.
        let accounts = self.inner.get_multiple_accounts(&keys).await?;
        let clock = self.inner.clock().await?;

        let mut instance = self.take().await;
        let chain = Snapshot {
            keys: &keys,
            accounts: &accounts,
            clock: &clock,
            programdata,
        };
        let outcome = self.load_and_run(&mut instance, tx, &chain, return_accounts);
        self.put(instance).await;
        outcome
    }

    fn load_and_run(
        &self,
        instance: &mut Instance,
        tx: &Transaction,
        chain: &Snapshot<'_>,
        return_accounts: &[Pubkey],
    ) -> Result<SimOutcome> {
        let Snapshot {
            keys,
            accounts,
            clock,
            programdata,
        } = chain;
        // Chain time drives every wake and most executor logic.
        instance.svm.set_sysvar(&Clock {
            slot: clock.slot,
            unix_timestamp: clock.unix_timestamp,
            ..Default::default()
        });

        // A fee payer with no account cannot pay, and litesvm rejects the
        // transaction before any instruction runs.
        if let Some(lamports) = self.config.synthetic_fee_payer_lamports {
            let payer_missing = accounts.first().is_none_or(Option::is_none);
            if let (true, Some(payer)) = (payer_missing, keys.first()) {
                let synthetic = Account {
                    lamports,
                    ..Default::default()
                };
                if let Err(err) = instance.svm.set_account(*payer, synthetic) {
                    warn!(account = %payer, error = ?err, "could not seed a synthetic fee payer");
                }
            }
        }

        for (key, account) in keys.iter().zip(accounts.iter()) {
            let Some(account) = account else { continue };
            if account.executable {
                self.load_program(instance, key, account, programdata);
                continue;
            }
            if let Err(err) = instance.svm.set_account(*key, account.clone()) {
                warn!(account = %key, error = ?err, "local sim could not seed account");
            }
        }

        let result = instance.svm.simulate_transaction(tx.clone());
        Ok(match result {
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
        })
    }

    /// The address of an upgradeable program's programdata account, named by
    /// the program account itself.
    fn programdata_address(program: &Account) -> Option<Pubkey> {
        let bytes = program
            .data
            .get(PROGRAM_PROGRAMDATA_OFFSET..PROGRAM_PROGRAMDATA_OFFSET + 32)?;
        Pubkey::try_from(bytes).ok()
    }

    /// The slot an upgradeable program was last deployed at, read **only**
    /// from the copy fetched from chain this tick.
    ///
    /// Deliberately no fallback to the bank. `add_program_with_loader`
    /// installs its own programdata at the same derived address, stamped with
    /// the bank's clock slot, so the bank's copy is the simulator's
    /// bookkeeping rather than the chain's — comparing against it would
    /// report an upgrade on every tick the clock advanced. Absent means
    /// "unknown", which the caller treats as "change nothing".
    fn deploy_slot(program: &Account, fresh: &HashMap<Pubkey, Account>) -> Option<u64> {
        let programdata = fresh.get(&Self::programdata_address(program)?)?;
        let bytes = programdata
            .data
            .get(PROGRAMDATA_SLOT_OFFSET..PROGRAMDATA_SLOT_OFFSET + 8)?;
        Some(u64::from_le_bytes(bytes.try_into().ok()?))
    }

    /// The ELF behind an upgradeable program. Unlike the deploy slot this
    /// does fall back to the bank, because a programdata account named
    /// directly in the transaction arrives through the normal account loop
    /// and never passes through the seeding map — and for the bytes
    /// themselves the bank's copy is the same ELF either way.
    fn programdata_elf(
        &self,
        instance: &Instance,
        program: &Account,
        fresh: &HashMap<Pubkey, Account>,
    ) -> Option<Vec<u8>> {
        let address = Self::programdata_address(program)?;
        let programdata = fresh
            .get(&address)
            .cloned()
            .or_else(|| instance.svm.get_account(&address))?;
        programdata
            .data
            .get(PROGRAMDATA_ELF_OFFSET..)
            .map(Vec::from)
    }

    /// Load a program's ELF into the bank, re-loading it if the program has
    /// been redeployed since.
    ///
    /// Programs are the expensive thing to install — verification dominates,
    /// and velocity's ELF is megabytes — so pooled instances keep them across
    /// ticks. That cache has to be invalidated when the chain moves, and it
    /// has to be invalidated *only* then: re-loading without cause silently
    /// converts the pool into a per-simulation full program load, which looks
    /// exactly like working upgrade detection from the outside.
    ///
    /// Upgradeable programs are versioned by the deploy slot in their
    /// programdata account. A change is acted on; anything else — an
    /// unchanged slot, or a slot that could not be read at all — leaves the
    /// cache alone, so a failed programdata fetch degrades to "keep
    /// simulating against what we have" rather than to churn.
    fn load_program(
        &self,
        instance: &mut Instance,
        program_id: &Pubkey,
        account: &Account,
        fresh: &HashMap<Pubkey, Account>,
    ) {
        // Builtins are in every bank already; nothing to load or track.
        if account.owner == *NATIVE_LOADER {
            instance.programs.insert(*program_id);
            return;
        }
        let upgradeable = account.owner == *BPF_LOADER_UPGRADEABLE;
        if !upgradeable && account.owner != *BPF_LOADER {
            // Loader v4, or something newer still. Previously these were
            // filed as builtins and assumed present, which is silent and
            // wrong: the program never loads and the failure surfaces later
            // as an unrelated simulation error.
            if instance.unhostable.insert(*program_id) {
                warn!(
                    program = %program_id,
                    loader = %account.owner,
                    loader_v4 = (account.owner == *LOADER_V4),
                    "local sim cannot host this loader; simulations naming this \
                     program will fail. Use --remote-sim for it."
                );
            }
            return;
        }

        // The chain's version of this program, from this tick's fetch.
        let deploy_slot = upgradeable
            .then(|| Self::deploy_slot(account, fresh))
            .flatten();

        let mut reloading = false;
        if instance.programs.contains(program_id) {
            // A non-upgradeable program cannot be redeployed, so its entry
            // is permanent.
            if !upgradeable {
                return;
            }
            let cached = instance.program_slots.get(program_id).copied();
            match (cached, deploy_slot) {
                (Some(cached), Some(current)) if cached != current => {
                    // Evict so the load below installs the fresh ELF.
                    instance.programs.remove(program_id);
                    instance.program_slots.remove(program_id);
                    reloading = true;
                    info!(
                        program = %program_id,
                        old_slot = cached,
                        new_slot = current,
                        "program upgrade detected, re-loading ELF"
                    );
                }
                // Unchanged, or nothing observed to change our mind.
                _ => return,
            }
        }

        let elf = if upgradeable {
            match self.programdata_elf(instance, account, fresh) {
                Some(elf) => elf,
                None => {
                    warn!(
                        program = %program_id,
                        "programdata not seeded; program will be uninvokable in local sim"
                    );
                    return;
                }
            }
        } else {
            account.data.clone()
        };

        let loader = account.owner;
        match instance
            .svm
            .add_program_with_loader(*program_id, &elf, loader)
        {
            Ok(()) => {
                instance.programs.insert(*program_id);
                if let Some(slot) = deploy_slot {
                    instance.program_slots.insert(*program_id, slot);
                }
                // Counted after the fact, so it only ever reports a re-load
                // that actually completed. It needs its own flag: the branch
                // above evicts the program first, so the insert here always
                // reports a fresh entry.
                if reloading {
                    metrics::PROGRAM_RELOADS
                        .with_label_values(&[&metrics::program_label(program_id)])
                        .inc();
                }
                debug!(program = %program_id, bytes = elf.len(), "loaded program into local sim");
            }
            Err(err) => warn!(program = %program_id, error = ?err, "local sim program load failed"),
        }
    }

    /// Seed programdata accounts for the programs a transaction invokes, so
    /// [`Self::load_program`] can find their ELFs. Upgradeable programs
    /// name their programdata in account data, and that account is not part
    /// of the transaction, so it has to be fetched separately.
    async fn seed_programdata(&self, keys: &[Pubkey]) -> Result<HashMap<Pubkey, Account>> {
        let accounts = self.inner.get_multiple_accounts(keys).await?;
        let programdata: Vec<Pubkey> = accounts
            .iter()
            .flatten()
            .filter(|account| account.executable && account.owner == *BPF_LOADER_UPGRADEABLE)
            .filter_map(|account| {
                let bytes = account
                    .data
                    .get(PROGRAM_PROGRAMDATA_OFFSET..PROGRAM_PROGRAMDATA_OFFSET + 32)?;
                Pubkey::try_from(bytes).ok()
            })
            .collect();
        if programdata.is_empty() {
            return Ok(HashMap::new());
        }
        let fetched = self.inner.get_multiple_accounts(&programdata).await?;
        let found: HashMap<Pubkey, Account> = programdata
            .iter()
            .zip(&fetched)
            .filter_map(|(key, account)| account.as_ref().map(|a| (*key, a.clone())))
            .collect();
        let mut pool = self.pool.lock().await;
        if pool.is_empty() {
            pool.push(Instance::new());
        }
        pool.iter_mut().for_each(|instance| {
            found.iter().for_each(|(key, account)| {
                let _ = instance.svm.set_account(*key, account.clone());
            });
        });
        // Handed back as well as seeded: the bank's copy gets overwritten by
        // program loading, so version comparisons read this one instead.
        Ok(found)
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
        tx: &Transaction,
        return_accounts: &[Pubkey],
    ) -> Result<SimOutcome> {
        // Programs the transaction can reach may need their programdata
        // seeded before their ELF can be recovered. That is every
        // executable account in the key list, not just top-level program
        // ids — a CPI target rides along as a plain account meta.
        let programdata = match self.seed_programdata(&tx.message.account_keys).await {
            Ok(found) => found,
            Err(err) => {
                warn!(error = %format!("{err:#}"), "programdata seed failed; simulating anyway");
                HashMap::new()
            }
        };
        let outcome = self
            .simulate_locally(tx, return_accounts, &programdata)
            .await;
        metrics::SIMULATIONS
            .with_label_values(&[if outcome.is_ok() { "local" } else { "error" }])
            .inc();
        outcome
    }

    async fn send_transaction(&self, tx: &Transaction) -> Result<Signature> {
        self.inner.send_transaction(tx).await
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use solana_sdk::hash::Hash;
    use solana_sdk::instruction::{AccountMeta, Instruction};
    use solana_sdk::signature::{Keypair, Signer};

    use super::*;
    use crate::source::{
        AccountFilter, BlockhashInfo, ClockSnapshot, SignatureOutcome, SimOutcome,
    };

    /// A fixed account map; everything else is unreachable in these tests.
    struct MapSource(HashMap<Pubkey, Account>);

    #[async_trait]
    impl ChainSource for MapSource {
        async fn get_multiple_accounts(&self, pubkeys: &[Pubkey]) -> Result<Vec<Option<Account>>> {
            Ok(pubkeys.iter().map(|pk| self.0.get(pk).cloned()).collect())
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
            _tx: &Transaction,
            _return_accounts: &[Pubkey],
        ) -> Result<SimOutcome> {
            unreachable!("local sim must never fall through to the provider")
        }
        async fn send_transaction(&self, _tx: &Transaction) -> Result<Signature> {
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
        let elf = std::fs::read(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../programs/target/deploy/demo_book.so"
        ))
        .expect("demo_book.so missing — run scripts/build-programs.sh first");

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
        let local = LocalSimSource::new(MapSource(accounts), LocalSimConfig::default());

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

        let outcome = local.simulate_transaction(&tx, &[]).await.unwrap();
        assert_eq!(outcome.err, None, "logs: {:?}", outcome.logs);
        let instance = local.pool.lock().await.pop().expect("instance pooled");
        assert!(
            instance.programs.contains(&cpi_target),
            "CPI-target ELF was not loaded into the local sim"
        );
    }
}
