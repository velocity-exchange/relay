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
//! ## Freshness
//!
//! A cache in front of a simulator is a correctness problem, not just a
//! performance one: serving a stale account makes the simulation decide
//! about a world that no longer exists. The rule here turns on one
//! distinction — *why* an account has been quiet:
//!
//! - **Covered** (the backend holds a live subscription for it, or for its
//!   owner program): silence means nothing changed, so the cached value is
//!   authoritative and free to serve.
//! - **Uncovered** (nobody is listening; it only ever arrived via a fetch):
//!   silence means nothing, so the value may be served for at most
//!   `max_age_uncovered` before it must be revalidated.
//!
//! That is why backends publish [`Coverage`] rather than the cache
//! inferring it from traffic. Two backstops sit under it:
//!
//! - **Heartbeat.** The clock sysvar is always in the interest set and
//!   changes every slot, so it is a liveness probe for the feed itself. If
//!   no update of any kind arrives for `feed_silence_timeout`, the feed is
//!   dead (not the chain quiet), coverage is disbelieved, and everything
//!   falls back to age-bounded revalidation.
//! - **Ceiling.** Even covered accounts are revalidated after
//!   `max_age_covered`, in case a subscription is live but lying.

use std::collections::{HashMap, HashSet};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use anyhow::Result;
use async_trait::async_trait;
use solana_sdk::account::Account;
use solana_sdk::clock::Clock;
use solana_sdk::pubkey::Pubkey;
use solana_sdk::signature::Signature;
use solana_sdk::sysvar;
use solana_sdk::transaction::Transaction;

use crate::feed::FeedReceiver;
use crate::metrics;
use crate::source::{ChainSource, ClockSnapshot, SimOutcome};

#[derive(Debug, Clone)]
pub struct CachedSourceConfig {
    /// The relay program whose watch accounts the backend streams.
    pub relay_program: Pubkey,
    /// How long an account with no live subscription may be served from
    /// cache before it must be refetched. Keep this short: these are
    /// exactly the accounts a simulation could be wrong about. Zero means
    /// always revalidate.
    pub max_age_uncovered: Duration,
    /// Ceiling for accounts that *are* covered by a live subscription —
    /// pure paranoia, in case a subscription is live but not delivering.
    pub max_age_covered: Duration,
    /// No feed update of any kind for this long means the feed is dead
    /// (the clock alone updates every slot), so coverage is disbelieved.
    pub feed_silence_timeout: Duration,
    /// Cache every account owned by these programs, not just the ones
    /// explicitly asked for. Point this at the protocol you crank and
    /// local simulation stops needing the network.
    pub watch_programs: Vec<Pubkey>,
}

impl Default for CachedSourceConfig {
    fn default() -> Self {
        Self {
            relay_program: crate::turner::TurnerConfig::default().relay_program,
            // About one slot: an uncovered account is never more than a
            // block behind what a simulation sees.
            max_age_uncovered: Duration::from_millis(400),
            max_age_covered: Duration::from_secs(30),
            feed_silence_timeout: Duration::from_secs(10),
            watch_programs: Vec::new(),
        }
    }
}

#[derive(Debug, Clone)]
struct CachedAccount {
    slot: u64,
    account: Option<Account>,
    /// When this value was last confirmed current, by a feed update or a
    /// fetch.
    observed: Instant,
}

struct CacheState {
    feed: FeedReceiver,
    accounts: HashMap<Pubkey, CachedAccount>,
    /// Pubkeys ever observed as relay watch accounts (from the warm start or
    /// the backend's owner subscription).
    watch_keys: HashSet<Pubkey>,
    interested: HashSet<Pubkey>,
    warm: bool,
    /// Last update of any kind — the feed's heartbeat.
    last_feed_update: Option<Instant>,
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
                last_feed_update: None,
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
            let now = Instant::now();
            state.last_feed_update = Some(now);
            let entry = state
                .accounts
                .entry(update.pubkey)
                .or_insert_with(|| CachedAccount {
                    slot: 0,
                    account: None,
                    observed: now,
                });
            if update.slot >= entry.slot {
                entry.slot = update.slot;
                entry.account = update.account;
                entry.observed = now;
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

    /// Is the feed itself alive? The clock sysvar updates every slot, so
    /// silence here means the subscription is dead rather than the chain
    /// being quiet.
    fn feed_healthy(state: &CacheState, config: &CachedSourceConfig, now: Instant) -> bool {
        state
            .last_feed_update
            .is_some_and(|last| now.duration_since(last) < config.feed_silence_timeout)
    }

    /// Must this account be refetched before it is safe to use?
    ///
    /// The whole freshness policy lives here: never seen ⇒ yes; covered by
    /// a live subscription on a healthy feed ⇒ only past the paranoia
    /// ceiling; otherwise ⇒ past the (short) uncovered age.
    fn needs_revalidation(
        state: &CacheState,
        config: &CachedSourceConfig,
        coverage: &crate::feed::Coverage,
        healthy: bool,
        pubkey: &Pubkey,
        now: Instant,
    ) -> bool {
        let Some(entry) = state.accounts.get(pubkey) else {
            return true;
        };
        let owner = entry.account.as_ref().map(|account| account.owner);
        let covered = healthy && coverage.covers(pubkey, owner.as_ref());
        let max_age = if covered {
            config.max_age_covered
        } else {
            config.max_age_uncovered
        };
        metrics::CACHE_READS
            .with_label_values(&[if covered { "covered" } else { "uncovered" }])
            .inc();
        now.duration_since(entry.observed) >= max_age
    }
}

#[async_trait]
impl<Inner: ChainSource> ChainSource for CachedSource<Inner> {
    async fn get_multiple_accounts(&self, pubkeys: &[Pubkey]) -> Result<Vec<Option<Account>>> {
        let now = Instant::now();
        let misses: Vec<Pubkey> = self.with_state(|state, config| {
            Self::ensure_interest(state, config, pubkeys);
            let coverage = state.feed.coverage.borrow().clone();
            let healthy = Self::feed_healthy(state, config, now);
            metrics::FEED_HEALTHY
                .with_label_values(&["accounts"])
                .set(healthy as i64);
            pubkeys
                .iter()
                .filter(|pk| Self::needs_revalidation(state, config, &coverage, healthy, pk, now))
                .copied()
                .collect()
        });

        // Which reads the subscription served vs which fell through to
        // RPC: a subscription that has silently died shows up here first.
        metrics::UPDATE_SOURCE
            .with_label_values(&["subscription"])
            .inc_by((pubkeys.len() - misses.len()) as u64);
        metrics::UPDATE_SOURCE
            .with_label_values(&["repoll"])
            .inc_by(misses.len() as u64);

        if !misses.is_empty() {
            let fetched = self.inner.get_multiple_accounts(&misses).await?;
            let fetched_at = Instant::now();
            self.with_state(|state, _| {
                misses.iter().zip(fetched).for_each(|(pk, account)| {
                    match state.accounts.get_mut(pk) {
                        // A fetch confirms the value is current as of now,
                        // whatever its slot: that is what resets the age.
                        Some(entry) => {
                            entry.account = account;
                            entry.observed = fetched_at;
                        }
                        None => {
                            state.accounts.insert(
                                *pk,
                                CachedAccount {
                                    slot: 0,
                                    account,
                                    observed: fetched_at,
                                },
                            );
                        }
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
            let fetched_at = Instant::now();
            self.with_state(|state, _| {
                accounts.into_iter().for_each(|(pk, account)| {
                    state.watch_keys.insert(pk);
                    state.accounts.entry(pk).or_insert_with(|| CachedAccount {
                        slot: 0,
                        account: Some(account),
                        observed: fetched_at,
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

    /// The clock goes through the same freshness rule as any other
    /// account, which matters more here than anywhere else: a clock served
    /// blindly from cache freezes the moment the feed dies, and every
    /// timestamp and slot wake is then evaluated against a "now" that
    /// never advances. The turner would go silently idle while still
    /// looking healthy — refreshing its registry, reporting no work.
    async fn clock(&self) -> Result<ClockSnapshot> {
        let cached = self
            .get_multiple_accounts(&[sysvar::clock::id()])
            .await?
            .into_iter()
            .next()
            .flatten()
            .and_then(|account| bincode::deserialize::<Clock>(&account.data).ok());
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
