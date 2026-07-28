//! A [`ChainSource`] that serves account reads from a subscription-fed
//! cache, falling back to an inner source (normally RPC) on misses.
//! Transactions, blockhashes, and simulation always go to the inner source —
//! the cache only exists so account/clock reads ride the push feed instead
//! of polling.
//!
//! Interest is learned two ways. Any pubkey a caller asks for is added to
//! the published interest set, which backends subscribe to individually.
//! On top of that, `watch_programs` streams **every** account owned by the
//! named programs — the setting that makes local simulation cheap: a
//! turner cranking its own protocol names that protocol's program id and
//! then almost never pays an RPC fetch to simulate, because every account
//! a crank touches is already resident.
//!
//! Watch-registry accounts arrive via the backend's program-owner
//! subscription after a one-time warm start through the inner source.
//!
//! Insurance against silently-dropped subscriptions (the tuktuk dual
//! ws+poll pattern): every `repoll_every` reads of a given account the cache
//! treats it as a miss and refetches through the inner source. Set to 0 to
//! disable.

use std::collections::{HashMap, HashSet};
use std::sync::Mutex;

use anyhow::Result;
use async_trait::async_trait;
use solana_sdk::account::Account;
use solana_sdk::clock::Clock;
use solana_sdk::pubkey::Pubkey;
use solana_sdk::signature::Signature;
use solana_sdk::sysvar;
use solana_sdk::transaction::Transaction;

use crate::feed::FeedReceiver;
use crate::source::{ChainSource, ClockSnapshot, SimOutcome};

#[derive(Debug, Clone)]
pub struct CachedSourceConfig {
    /// The relay program whose watch accounts the backend streams.
    pub relay_program: Pubkey,
    /// Refetch a cached account through the inner source every N reads of
    /// it (0 = never). Cheap insurance against a dead subscription.
    pub repoll_every: u64,
    /// Cache every account owned by these programs, not just the ones
    /// explicitly asked for. Point this at the protocol you crank and
    /// local simulation stops needing the network.
    pub watch_programs: Vec<Pubkey>,
}

impl Default for CachedSourceConfig {
    fn default() -> Self {
        Self {
            relay_program: crate::turner::TurnerConfig::default().relay_program,
            repoll_every: 32,
            watch_programs: Vec::new(),
        }
    }
}

#[derive(Debug, Clone)]
struct CachedAccount {
    slot: u64,
    account: Option<Account>,
    reads: u64,
}

struct CacheState {
    feed: FeedReceiver,
    accounts: HashMap<Pubkey, CachedAccount>,
    /// Pubkeys ever observed as relay watch accounts (from the warm start or
    /// the backend's owner subscription).
    watch_keys: HashSet<Pubkey>,
    interested: HashSet<Pubkey>,
    warm: bool,
}

pub struct CachedSource<Inner> {
    inner: Inner,
    config: CachedSourceConfig,
    state: Mutex<CacheState>,
}

impl<Inner: ChainSource> CachedSource<Inner> {
    pub fn new(inner: Inner, feed: FeedReceiver, config: CachedSourceConfig) -> Self {
        let source = Self {
            inner,
            config,
            state: Mutex::new(CacheState {
                feed,
                accounts: HashMap::new(),
                watch_keys: HashSet::new(),
                interested: HashSet::new(),
                warm: false,
            }),
        };
        // The clock rides the feed like any other account.
        source.with_state(|state, config| {
            Self::ensure_interest(state, config, &[sysvar::clock::id()]);
        });
        source
    }

    pub fn inner(&self) -> &Inner {
        &self.inner
    }

    fn with_state<T>(&self, f: impl FnOnce(&mut CacheState, &CachedSourceConfig) -> T) -> T {
        let mut state = self.state.lock().unwrap();
        Self::drain(&mut state, &self.config);
        f(&mut state, &self.config)
    }

    /// Apply pending feed updates. Updates are ordered per account by slot;
    /// stale ones (older slot than cached) are dropped.
    fn drain(state: &mut CacheState, config: &CachedSourceConfig) {
        while let Ok(update) = state.feed.updates.try_recv() {
            let is_watch = update.account.as_ref().is_some_and(|acc| {
                acc.owner == config.relay_program && acc.data.len() == relay_spec::WATCH_V0_LEN
            });
            let entry = state
                .accounts
                .entry(update.pubkey)
                .or_insert_with(|| CachedAccount {
                    slot: 0,
                    account: None,
                    reads: 0,
                });
            if update.slot >= entry.slot {
                entry.slot = update.slot;
                entry.account = update.account;
            }
            if is_watch {
                state.watch_keys.insert(update.pubkey);
            }
        }
    }

    fn ensure_interest(state: &mut CacheState, _config: &CachedSourceConfig, pubkeys: &[Pubkey]) {
        // Republish only when the set actually grew — every send wakes the
        // backend into rebuilding its subscriptions.
        let before = state.interested.len();
        state.interested.extend(pubkeys.iter().copied());
        if state.interested.len() != before {
            let _ = state.feed.interest.send(state.interested.clone());
        }
    }

    /// Accounts owned by a watched program are streamed wholesale, so
    /// they are never a cache miss even on first read.
    fn is_watched_program(config: &CachedSourceConfig, account: &Option<Account>) -> bool {
        account
            .as_ref()
            .is_some_and(|account| config.watch_programs.contains(&account.owner))
    }

    /// A cached entry is served unless it has never been seen, or its repoll
    /// counter came due.
    fn is_miss(entry: Option<&mut CachedAccount>, repoll_every: u64) -> bool {
        match entry {
            None => true,
            Some(entry) => {
                entry.reads += 1;
                repoll_every != 0 && entry.reads.is_multiple_of(repoll_every)
            }
        }
    }
}

#[async_trait]
impl<Inner: ChainSource> ChainSource for CachedSource<Inner> {
    async fn get_multiple_accounts(&self, pubkeys: &[Pubkey]) -> Result<Vec<Option<Account>>> {
        let misses: Vec<Pubkey> = self.with_state(|state, config| {
            Self::ensure_interest(state, config, pubkeys);
            pubkeys
                .iter()
                .filter(|pk| {
                    let watched = Self::is_watched_program(
                        config,
                        &state.accounts.get(pk).and_then(|e| e.account.clone()),
                    );
                    let miss = Self::is_miss(state.accounts.get_mut(pk), config.repoll_every);
                    // A program-streamed account is authoritative: the feed
                    // carries every write to it, so never refetch.
                    miss && !watched
                })
                .copied()
                .collect()
        });

        // Which reads the subscription served vs which fell through to
        // RPC: a subscription that has silently died shows up here first.
        crate::metrics::UPDATE_SOURCE
            .with_label_values(&["subscription"])
            .inc_by((pubkeys.len() - misses.len()) as u64);
        crate::metrics::UPDATE_SOURCE
            .with_label_values(&["repoll"])
            .inc_by(misses.len() as u64);

        if !misses.is_empty() {
            let fetched = self.inner.get_multiple_accounts(&misses).await?;
            self.with_state(|state, _| {
                misses.iter().zip(fetched).for_each(|(pk, account)| {
                    let entry = state.accounts.entry(*pk).or_insert_with(|| CachedAccount {
                        slot: 0,
                        account: None,
                        reads: 0,
                    });
                    // Slot 0: any real feed update (even one racing in the
                    // queue) outranks an RPC snapshot of unknown slot.
                    if entry.slot == 0 {
                        entry.account = account;
                    }
                });
            });
        }

        Ok(self.with_state(|state, _| {
            pubkeys
                .iter()
                .map(|pk| state.accounts.get(pk).and_then(|e| e.account.clone()))
                .collect()
        }))
    }

    async fn get_watch_accounts(
        &self,
        program: &Pubkey,
        target_programs: &[Pubkey],
    ) -> Result<Vec<(Pubkey, Account)>> {
        let warm = self.with_state(|state, _| state.warm);
        if !warm {
            let accounts = self
                .inner
                .get_watch_accounts(program, target_programs)
                .await?;
            self.with_state(|state, _| {
                accounts.into_iter().for_each(|(pk, account)| {
                    state.watch_keys.insert(pk);
                    state.accounts.entry(pk).or_insert_with(|| CachedAccount {
                        slot: 0,
                        account: Some(account),
                        reads: 0,
                    });
                });
                state.warm = true;
            });
        }
        // The backend's own subscription filter may be broader than the
        // caller's allowlist (or absent), so re-apply it here.
        let allowed: std::collections::HashSet<[u8; 32]> =
            target_programs.iter().map(|pk| pk.to_bytes()).collect();
        Ok(self.with_state(|state, config| {
            state
                .watch_keys
                .iter()
                .filter_map(|pk| {
                    state
                        .accounts
                        .get(pk)
                        .and_then(|e| e.account.clone())
                        .filter(|acc| {
                            acc.owner == config.relay_program
                                && acc.data.len() == relay_spec::WATCH_V0_LEN
                        })
                        .filter(|acc| {
                            allowed.is_empty()
                                || relay_spec::WatchV0::read_from_account(&acc.data)
                                    .is_ok_and(|w| allowed.contains(&w.target_program))
                        })
                        .map(|acc| (*pk, acc))
                })
                .collect()
        }))
    }

    async fn clock(&self) -> Result<ClockSnapshot> {
        let cached = self.with_state(|state, _| {
            state
                .accounts
                .get(&sysvar::clock::id())
                .and_then(|e| e.account.as_ref())
                .and_then(|acc| bincode::deserialize::<Clock>(&acc.data).ok())
        });
        match cached {
            Some(clock) => Ok(ClockSnapshot {
                slot: clock.slot,
                unix_timestamp: clock.unix_timestamp,
            }),
            None => self.inner.clock().await,
        }
    }

    async fn latest_blockhash(&self) -> Result<crate::source::BlockhashInfo> {
        self.inner.latest_blockhash().await
    }

    async fn block_height(&self) -> Result<u64> {
        self.inner.block_height().await
    }

    async fn signature_statuses(
        &self,
        signatures: &[Signature],
    ) -> Result<Vec<Option<crate::source::SignatureOutcome>>> {
        self.inner.signature_statuses(signatures).await
    }

    async fn recent_priority_fee(&self, accounts: &[Pubkey]) -> Result<u64> {
        self.inner.recent_priority_fee(accounts).await
    }

    async fn simulate_transaction(
        &self,
        tx: &Transaction,
        return_accounts: &[Pubkey],
    ) -> Result<SimOutcome> {
        self.inner.simulate_transaction(tx, return_accounts).await
    }

    async fn send_transaction(&self, tx: &Transaction) -> Result<Signature> {
        self.inner.send_transaction(tx).await
    }
}
