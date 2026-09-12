//! The SBF program cache behind the local simulator.
//!
//! Programs are the expensive thing to install — verification dominates, and
//! a large ELF is megabytes — so pooled instances keep them across ticks.
//! That cache has to be invalidated when the chain moves, and it has to be
//! invalidated *only* then: re-loading without cause silently converts the
//! pool into a per-simulation full program load, which looks exactly like
//! working upgrade detection from the outside.
//!
//! Upgradeable programs are versioned by the deploy slot in their
//! programdata account. A change is acted on; anything else — an unchanged
//! slot, or a slot that could not be read at all — leaves the cache alone,
//! so a failed programdata fetch degrades to "keep simulating against what
//! we have" rather than to churn.

use std::collections::HashMap;
use std::sync::LazyLock;

use anyhow::Result;
use solana_sdk::account::Account;
use solana_sdk::pubkey::Pubkey;
use tracing::{debug, info, warn};

use crate::metrics;
use crate::source::ChainSource;

use super::{Instance, LocalSimSource};

/// Offset of the ELF inside an upgradeable programdata account:
/// `enum tag (4) + slot (8) + Option<Pubkey> authority (1 + 32)`.
pub(super) const PROGRAMDATA_ELF_OFFSET: usize = 45;
/// Slot inside an upgradeable programdata account, right after the tag.
const PROGRAMDATA_SLOT_OFFSET: usize = 4;
/// Offset of the programdata address inside a `Program` account:
/// `enum tag (4)`.
const PROGRAM_PROGRAMDATA_OFFSET: usize = 4;

/// Loader whose program accounts hold the ELF directly.
pub(super) static BPF_LOADER: LazyLock<Pubkey> = LazyLock::new(|| {
    "BPFLoader2111111111111111111111111111111111"
        .parse()
        .unwrap()
});

/// Upgradeable loader: the ELF lives in a separate programdata account
/// named by the program account.
pub(super) static BPF_LOADER_UPGRADEABLE: LazyLock<Pubkey> = LazyLock::new(|| {
    "BPFLoaderUpgradeab1e11111111111111111111111"
        .parse()
        .unwrap()
});

/// Builtins (system, compute budget, ...) are already in every bank.
pub(super) static NATIVE_LOADER: LazyLock<Pubkey> = LazyLock::new(|| {
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
pub(super) static LOADER_V4: LazyLock<Pubkey> = LazyLock::new(|| {
    "LoaderV411111111111111111111111111111111111"
        .parse()
        .unwrap()
});

impl<Inner: ChainSource> LocalSimSource<Inner> {
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
    pub(super) fn load_program(
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
            warn_unhostable(instance, program_id, account);
            return;
        }

        // The chain's version of this program, from this tick's fetch.
        let deploy_slot = upgradeable
            .then(|| Self::deploy_slot(account, fresh))
            .flatten();
        let reloading = match self.evict_if_upgraded(instance, program_id, upgradeable, deploy_slot)
        {
            Some(reloading) => reloading,
            None => return,
        };

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

    /// Decide whether the cached copy of a program still stands.
    ///
    /// `None` means it does and there is nothing to load. `Some(reloading)`
    /// means the caller should load the ELF, and whether that load replaces
    /// an evicted entry rather than installing a first one.
    fn evict_if_upgraded(
        &self,
        instance: &mut Instance,
        program_id: &Pubkey,
        upgradeable: bool,
        deploy_slot: Option<u64>,
    ) -> Option<bool> {
        if !instance.programs.contains(program_id) {
            return Some(false);
        }
        // A non-upgradeable program cannot be redeployed, so its entry is
        // permanent.
        if !upgradeable {
            return None;
        }
        let cached = instance.program_slots.get(program_id).copied();
        match (cached, deploy_slot) {
            (Some(cached), Some(current)) if cached != current => {
                // Evict so the caller's load installs the fresh ELF.
                instance.programs.remove(program_id);
                instance.program_slots.remove(program_id);
                info!(
                    program = %program_id,
                    old_slot = cached,
                    new_slot = current,
                    "program upgrade detected, re-loading ELF"
                );
                Some(true)
            }
            // Unchanged, or nothing observed to change our mind.
            _ => None,
        }
    }

    /// Seed programdata accounts for the programs a transaction invokes, so
    /// [`Self::load_program`] can find their ELFs. Upgradeable programs
    /// name their programdata in account data, and that account is not part
    /// of the transaction, so it has to be fetched separately.
    ///
    /// Takes the transaction's already-read accounts rather than reading
    /// them again: seeding and bank population want the same set, and
    /// fetching it twice doubled the cold-account round trips.
    pub(super) async fn seed_programdata(
        &self,
        accounts: &[Option<Account>],
    ) -> Result<HashMap<Pubkey, Account>> {
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

/// Drop a pooled bank's copy of an account the chain no longer has.
///
/// Overwriting with a zeroed system-owned account is how a bank spells
/// "absent": there is no remove, and the state a transaction sees for a
/// zero-lamport, zero-data account is the state it sees for one that never
/// existed.
/// Loader v4, or something newer still.
///
/// These were previously filed as builtins and assumed present, which is
/// silent and wrong: the program never loads, and the failure surfaces later
/// as an unrelated simulation error. Warned once per program per instance.
fn warn_unhostable(instance: &mut Instance, program_id: &Pubkey, account: &Account) {
    if instance.unhostable.insert(*program_id) {
        warn!(
            program = %program_id,
            loader = %account.owner,
            loader_v4 = (account.owner == *LOADER_V4),
            "local sim cannot host this loader; simulations naming this \
             program will fail. Use --remote-sim for it."
        );
    }
}
