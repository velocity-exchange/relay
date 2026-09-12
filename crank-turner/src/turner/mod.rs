//! The condition loop: wake evaluation → resolver simulation (told which
//! condition fired) → read the staged payload out of post-simulation account
//! state → build the executor **it names** → simulation → send.
//!
//! Deliberately a pull-style [`Turner::tick`] state machine, so tests and
//! alternative runtimes drive it deterministically. The daemon binary wraps
//! it in a timer.
//!
//! The executor goes into the transaction **directly**, bracketed by
//! relay's payment guards rather than wrapped in a CPI: `begin_guard_v0`
//! snapshots the keeper's balance, the executor runs, `assert_paid_v0`
//! reverts everything unless the keeper gained at least the turner's
//! price. That keeps all four CPI levels available to the executor's own
//! call stack (a velocity crank that calls into the CLOB needs them) and
//! costs two ~1k-CU instructions instead of an invoke.
//!
//! The loop is split across this folder by subject. `tick` runs one pass in
//! three phases; `decide` answers whether one condition is due; `crank`
//! takes a due condition through resolve, build and price; `pack` groups the
//! survivors into transactions; `payment` decides what each crank pays and
//! to whom; `safety` is the binding gate on what the turner will sign;
//! `refresh` re-scans the registry; `state` holds the per-condition
//! bookkeeping between ticks; `explain` is the read-only account of all of
//! it that the CLI prints; and `config` is what an operator sets.

mod config;
mod crank;
mod decide;
mod explain;
mod outcome;
mod pack;
mod payment;
mod refresh;
mod safety;
mod state;
mod tick;

#[cfg(test)]
mod tests;

pub use config::{TurnerConfig, RELAY_PROGRAM_ID};
pub use explain::{Explanation, Verdict};
pub use outcome::{Outcome, SkipReason, Stage};
pub use safety::names_transaction_signer;

use outcome::{outcome_key, skip_label, stage_label, wake_label};

use std::collections::HashMap;

use anyhow::Result;
use solana_sdk::account::Account;
use solana_sdk::pubkey::Pubkey;
use solana_sdk::signature::Keypair;
use solana_sdk::signer::Signer;

use crate::metrics;
use crate::submit::SubmitterHandle;
use relay_chain_source::ChainSource;
use state::CondState;

/// One registered watch, parsed from a `WatchV0` account.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Watch {
    /// Owner program of `target`, recorded on chain at registration.
    pub target_program: Pubkey,
    pub target: Pubkey,
    /// Who registered the watch, and the only key that may close it.
    pub creator: Pubkey,
    pub offset: u32,
}

/// Identity of a condition across ticks.
pub type CondKey = (Pubkey, u32, u8);

pub struct Turner<S: ChainSource> {
    source: S,
    keeper: Keypair,
    config: TurnerConfig,
    watches: Vec<Watch>,
    state: HashMap<CondKey, CondState>,
    /// Where signed transactions go. Without one the turner sends inline
    /// through the source and forgets — fine for tests and small
    /// deployments, but it neither confirms nor resends.
    submitter: Option<SubmitterHandle>,
}

impl<S: ChainSource> Turner<S> {
    pub fn new(source: S, keeper: Keypair, config: TurnerConfig) -> Self {
        Self {
            source,
            keeper,
            config,
            watches: Vec::new(),
            state: HashMap::new(),
            submitter: None,
        }
    }

    /// Route submissions through a [`SubmitterHandle`], which owns the
    /// shared blockhash, confirmation tracking, resends, and profitability
    /// accounting.
    pub fn with_submitter(mut self, submitter: SubmitterHandle) -> Self {
        self.submitter = Some(submitter);
        self
    }

    /// The registry as this turner sees it, after filtering.
    pub fn watches(&self) -> &[Watch] {
        &self.watches
    }

    pub fn keeper_pubkey(&self) -> Pubkey {
        self.keeper.pubkey()
    }

    pub fn source(&self) -> &S {
        &self.source
    }

    async fn load_map(&self, pubkeys: &[Pubkey]) -> Result<HashMap<Pubkey, Account>> {
        if pubkeys.is_empty() {
            return Ok(HashMap::new());
        }
        let accounts = self.source.get_multiple_accounts(pubkeys).await?;
        Ok(pubkeys
            .iter()
            .zip(accounts)
            .filter_map(|(pk, acc)| acc.map(|a| (*pk, a)))
            .collect())
    }
}
