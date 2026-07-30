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

use std::collections::HashSet;
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
use tracing::{debug, warn};

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
/// Offset of the ELF inside an upgradeable programdata account:
/// `enum tag (4) + slot (8) + Option<Pubkey> authority (1 + 32)`.
const PROGRAMDATA_ELF_OFFSET: usize = 45;
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
struct Instance {
    svm: LiteSVM,
    programs: HashSet<Pubkey>,
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
    ) -> Result<SimOutcome> {
        let keys: Vec<Pubkey> = tx.message.account_keys.clone();
        // Cache-first; only genuinely cold accounts reach the network.
        let accounts = self.inner.get_multiple_accounts(&keys).await?;
        let clock = self.inner.clock().await?;

        let mut instance = self.take().await;
        let outcome =
            self.load_and_run(&mut instance, tx, &keys, &accounts, &clock, return_accounts);
        self.put(instance).await;
        outcome
    }

    fn load_and_run(
        &self,
        instance: &mut Instance,
        tx: &Transaction,
        keys: &[Pubkey],
        accounts: &[Option<Account>],
        clock: &ClockSnapshot,
        return_accounts: &[Pubkey],
    ) -> Result<SimOutcome> {
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

        for (key, account) in keys.iter().zip(accounts) {
            let Some(account) = account else { continue };
            if account.executable {
                self.load_program(instance, key, account);
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

    /// Load a program's ELF into the bank once. Programs are the expensive
    /// thing to install (verification), so pooled instances keep them.
    fn load_program(&self, instance: &mut Instance, program_id: &Pubkey, account: &Account) {
        if instance.programs.contains(program_id) {
            return;
        }
        let elf = if account.owner == *BPF_LOADER_UPGRADEABLE {
            // The program account only names its programdata; fetching that
            // is the caller's job (it is in the transaction's account keys
            // only for upgrades, so we look it up lazily below).
            match self.programdata_elf(instance, account) {
                Some(elf) => elf,
                None => return,
            }
        } else if account.owner == *BPF_LOADER {
            account.data.clone()
        } else {
            // Builtins (system, compute budget, ...) are already present.
            instance.programs.insert(*program_id);
            return;
        };

        let loader = account.owner;
        match instance
            .svm
            .add_program_with_loader(*program_id, &elf, loader)
        {
            Ok(()) => {
                instance.programs.insert(*program_id);
                debug!(program = %program_id, bytes = elf.len(), "loaded program into local sim");
            }
            Err(err) => warn!(program = %program_id, error = ?err, "local sim program load failed"),
        }
    }

    /// Pull the ELF out of an upgradeable program's programdata account,
    /// which the bank may already hold from a previous simulation.
    fn programdata_elf(&self, instance: &Instance, account: &Account) -> Option<Vec<u8>> {
        let address = account
            .data
            .get(PROGRAM_PROGRAMDATA_OFFSET..PROGRAM_PROGRAMDATA_OFFSET + 32)?;
        let address = Pubkey::try_from(address).ok()?;
        let programdata = instance.svm.get_account(&address)?;
        programdata
            .data
            .get(PROGRAMDATA_ELF_OFFSET..)
            .map(Vec::from)
    }

    /// Seed programdata accounts for the programs a transaction invokes, so
    /// [`Self::load_program`] can find their ELFs. Upgradeable programs
    /// name their programdata in account data, and that account is not part
    /// of the transaction, so it has to be fetched separately.
    async fn seed_programdata(&self, keys: &[Pubkey]) -> Result<()> {
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
            return Ok(());
        }
        let fetched = self.inner.get_multiple_accounts(&programdata).await?;
        let mut pool = self.pool.lock().await;
        if pool.is_empty() {
            pool.push(Instance::new());
        }
        pool.iter_mut().for_each(|instance| {
            programdata
                .iter()
                .zip(&fetched)
                .filter_map(|(key, account)| account.as_ref().map(|a| (key, a)))
                .for_each(|(key, account)| {
                    let _ = instance.svm.set_account(*key, account.clone());
                });
        });
        Ok(())
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
        // Programs invoked by the transaction may need their programdata
        // seeded before their ELF can be recovered.
        let program_ids: Vec<Pubkey> = tx
            .message
            .instructions
            .iter()
            .filter_map(|ix| {
                tx.message
                    .account_keys
                    .get(ix.program_id_index as usize)
                    .copied()
            })
            .collect();
        if let Err(err) = self.seed_programdata(&program_ids).await {
            warn!(error = %format!("{err:#}"), "programdata seed failed; simulating anyway");
        }
        let outcome = self.simulate_locally(tx, return_accounts).await;
        metrics::SIMULATIONS
            .with_label_values(&[if outcome.is_ok() { "local" } else { "error" }])
            .inc();
        outcome
    }

    async fn send_transaction(&self, tx: &Transaction) -> Result<Signature> {
        self.inner.send_transaction(tx).await
    }
}
