//! A [`ChainSource`] that serves account reads from a subscription-fed
//! cache, falling back to an inner source (normally RPC) on misses.
//! Transactions, blockhashes, and simulation always go to the inner source —
//! the cache only exists so account/clock reads ride the push feed instead
//! of polling.
//!
//! Interest is learned two ways. Any pubkey a caller asks for is added to
//! the published interest set, which backends subscribe to individually.
//! On top of that, `covered_programs` streams **every** account owned by the
//! named programs — the setting that makes local simulation cheap: a client
//! simulating its own protocol names that protocol's program id and then
//! almost never pays an RPC fetch, because every account its instructions
//! touch is already resident.
//!
//! Accounts matching an `indexed_programs` subscription are additionally
//! tracked by key, so [`ChainSource::get_program_accounts`] can be answered
//! from the cache after a one-time warm start through the inner source.
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

use tracing::warn;

use crate::feed::{FeedReceiver, SlotUpdate};
use crate::grpc::ProgramSubscription;
use crate::metrics;
use crate::source::{AccountFilter, ChainSource, ClockSnapshot, SimOutcome};

#[derive(Debug, Clone)]
pub struct CachedSourceConfig {
    /// The relay program whose watch accounts the backend streams.
    /// Program-account queries the cache should answer locally. Accounts
    /// matching one of these are indexed by key as they arrive, so a
    /// `get_program_accounts` for the same program is served from cache
    /// after the first warm-up fetch.
    pub indexed_programs: Vec<ProgramSubscription>,
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
    pub covered_programs: Vec<Pubkey>,
}

impl Default for CachedSourceConfig {
    fn default() -> Self {
        Self {
            indexed_programs: Vec::new(),
            // About one slot: an uncovered account is never more than a
            // block behind what a simulation sees.
            max_age_uncovered: Duration::from_millis(400),
            max_age_covered: Duration::from_secs(30),
            feed_silence_timeout: Duration::from_secs(10),
            covered_programs: Vec::new(),
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
    indexed_keys: HashSet<Pubkey>,
    interested: HashSet<Pubkey>,
    /// When each program's account set was last read through the inner
    /// source. Per program, not a single flag: a warm start for one says
    /// nothing about another's accounts being resident.
    warmed: HashMap<Pubkey, Instant>,
    /// Last update of any kind — the feed's heartbeat.
    last_feed_update: Option<Instant>,
    /// Highest slot seen processing. A processed slot at or below this, or
    /// one built on a parent below it, means the cluster switched forks.
    fork_tip: u64,
    /// Highest rooted slot: the floor below which nothing can be undone.
    root: u64,
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
                indexed_keys: HashSet::new(),
                interested: HashSet::new(),
                warmed: HashMap::new(),
                last_feed_update: None,
                fork_tip: 0,
                root: 0,
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
    ///
    /// Accounts first, then slots. The two arrive on separate channels, so
    /// their relative order is not guaranteed, and doing fork invalidation
    /// last means a write from the fork being abandoned cannot slip in
    /// behind it. The cost is that fresh writes from the *new* fork get
    /// dropped too, since they are also above the fork point — conservative
    /// in the direction that costs a refetch rather than a wrong simulation.
    fn drain(state: &mut CacheState, config: &CachedSourceConfig) {
        while let Ok(update) = state.feed.updates.try_recv() {
            let is_indexed = update
                .account
                .as_ref()
                .is_some_and(|acc| matches_any(acc, &config.indexed_programs));
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
            if is_indexed {
                state.indexed_keys.insert(update.pubkey);
            }
        }
        Self::drain_slots(state);
        metrics::CACHED_ACCOUNTS
            .with_label_values(&["accounts"])
            .set(state.accounts.len() as i64);
        metrics::CACHED_ACCOUNTS
            .with_label_values(&["indexed"])
            .set(state.indexed_keys.len() as i64);
    }

    /// Track the fork the cluster is on, and drop provisional writes when it
    /// changes.
    ///
    /// This is the half of `processed` reads that the account stream cannot
    /// cover on its own. A write on an abandoned fork produces no canonical
    /// write, so no correcting notification is ever sent, and silence is
    /// indistinguishable from "unchanged" — the cached value would stand
    /// until the age ceiling expired, feeding simulations a world that never
    /// happened. Dropping the entry sends it back through the normal
    /// revalidation path, where it is refetched before use.
    fn drain_slots(state: &mut CacheState) {
        while let Ok(update) = state.feed.slots.try_recv() {
            state.last_feed_update = Some(Instant::now());
            let (slot, parent) = match update {
                SlotUpdate::Rooted { slot } => {
                    state.root = state.root.max(slot);
                    continue;
                }
                SlotUpdate::Processed { slot, parent } => (slot, parent),
            };
            // Two shapes of fork switch: a slot we have already passed
            // showing up again, or a new slot built on something older than
            // what we had already seen processed. Skipped slots — normal and
            // frequent — are neither: they leave the tip advancing and the
            // parent at the previous tip.
            let switched = state.fork_tip > 0
                && (slot <= state.fork_tip || parent.is_some_and(|p| p < state.fork_tip));
            state.fork_tip = state.fork_tip.max(slot);
            if !switched {
                continue;
            }
            // Everything above the common ancestor is suspect. Without a
            // parent, fall back to the last rooted slot.
            let settled = parent.unwrap_or(state.root);
            let before = state.accounts.len();
            state.accounts.retain(|_, entry| entry.slot <= settled);
            let dropped = before - state.accounts.len();
            state
                .indexed_keys
                .retain(|key| state.accounts.contains_key(key));
            metrics::REORGS.with_label_values(&["detected"]).inc();
            metrics::REORGS
                .with_label_values(&["accounts_dropped"])
                .inc_by(dropped as u64);
            warn!(
                slot,
                parent = ?parent,
                settled,
                dropped,
                "fork switch: dropped provisional accounts"
            );
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

    /// Answered from the cache only while a live subscription is actually
    /// carrying the program's accounts.
    ///
    /// The membership of a program's account set is a read like any other,
    /// and it goes stale the same way: an account registered after the warm
    /// start reaches the cache through the subscription and nowhere else, so
    /// a query served from a cache the feed is no longer filling would keep
    /// answering with the set as it stood when the feed died. The same
    /// coverage rule the per-account path uses therefore decides how long a
    /// warm start is good for.
    async fn get_program_accounts(
        &self,
        program: &Pubkey,
        filter_sets: &[Vec<AccountFilter>],
    ) -> Result<Vec<(Pubkey, Account)>> {
        let now = Instant::now();
        let rewarm = self.with_state(|state, config| {
            let covered = Self::feed_healthy(state, config, now)
                && state.feed.coverage.borrow().programs.contains(program);
            let max_age = if covered {
                config.max_age_covered
            } else {
                config.max_age_uncovered
            };
            state
                .warmed
                .get(program)
                .is_none_or(|warmed| now.duration_since(*warmed) >= max_age)
        });
        if rewarm {
            let accounts = self
                .inner
                .get_program_accounts(program, filter_sets)
                .await?;
            let fetched_at = Instant::now();
            self.with_state(|state, _| {
                let seen: HashSet<Pubkey> = accounts.iter().map(|(pk, _)| *pk).collect();
                accounts.into_iter().for_each(|(pk, account)| {
                    state.indexed_keys.insert(pk);
                    let entry = state.accounts.entry(pk).or_insert(CachedAccount {
                        slot: 0,
                        account: None,
                        observed: fetched_at,
                    });
                    entry.account = Some(account);
                    entry.observed = fetched_at;
                });
                // An account the cache still holds but the provider did not
                // return, though it matches this very query, is gone —
                // closed, or reassigned. Leaving it indexed would keep it in
                // every later answer, which is the failure a re-warm exists
                // to correct.
                let vanished: Vec<Pubkey> = state
                    .indexed_keys
                    .iter()
                    .filter(|pk| !seen.contains(*pk))
                    .filter(|pk| {
                        state.accounts.get(pk).is_some_and(|entry| {
                            // Not one the feed delivered while the scan was
                            // in flight: the provider's answer predates it,
                            // and dropping it would lose a brand new account
                            // until something wrote to it again.
                            entry.observed <= now
                                && entry.account.as_ref().is_some_and(|account| {
                                    account.owner == *program
                                        && matches_filters(account, filter_sets)
                                })
                        })
                    })
                    .copied()
                    .collect();
                vanished.iter().for_each(|pk| {
                    state.indexed_keys.remove(pk);
                });
                state.warmed.insert(*program, fetched_at);
            });
        }
        // The backend's subscription may be broader than this query, so the
        // caller's own filters are re-applied locally.
        Ok(self.with_state(|state, _| {
            state
                .indexed_keys
                .iter()
                .filter_map(|pk| {
                    state
                        .accounts
                        .get(pk)
                        .and_then(|e| e.account.clone())
                        .filter(|acc| acc.owner == *program && matches_filters(acc, filter_sets))
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

/// Whether `account` satisfies at least one of `filter_sets` (an empty outer
/// slice means "no restriction"), the local twin of the provider-side filter.
fn matches_filters(account: &Account, filter_sets: &[Vec<AccountFilter>]) -> bool {
    filter_sets.is_empty()
        || filter_sets
            .iter()
            .any(|set| set.iter().all(|filter| filter.matches(account)))
}

/// Whether `account` belongs to any of the indexed program subscriptions.
fn matches_any(account: &Account, subscriptions: &[ProgramSubscription]) -> bool {
    subscriptions.iter().any(|subscription| {
        account.owner == subscription.program && matches_filters(account, &subscription.filter_sets)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::feed::{feed_channel, AccountUpdate};
    use crate::{BlockhashInfo, SignatureOutcome, SimOutcome};
    use solana_sdk::{signature::Signature, transaction::Transaction};

    /// A program-account provider the test drives: it counts the queries it
    /// answers and its answer can change between them, which is the whole
    /// point — a cache that never re-reads cannot notice either.
    struct ProgramSource {
        accounts: std::sync::Mutex<Vec<(Pubkey, Account)>>,
        queries: std::sync::atomic::AtomicUsize,
    }

    impl ProgramSource {
        fn new(accounts: Vec<(Pubkey, Account)>) -> Self {
            Self {
                accounts: std::sync::Mutex::new(accounts),
                queries: std::sync::atomic::AtomicUsize::new(0),
            }
        }

        fn queries(&self) -> usize {
            self.queries.load(std::sync::atomic::Ordering::SeqCst)
        }

        fn serve(&self, accounts: Vec<(Pubkey, Account)>) {
            *self.accounts.lock().unwrap() = accounts;
        }
    }

    #[async_trait::async_trait]
    impl ChainSource for ProgramSource {
        async fn get_multiple_accounts(&self, pubkeys: &[Pubkey]) -> Result<Vec<Option<Account>>> {
            let accounts = self.accounts.lock().unwrap();
            Ok(pubkeys
                .iter()
                .map(|pk| {
                    accounts
                        .iter()
                        .find(|(key, _)| key == pk)
                        .map(|(_, account)| account.clone())
                })
                .collect())
        }
        async fn get_program_accounts(
            &self,
            _program: &Pubkey,
            _filter_sets: &[Vec<AccountFilter>],
        ) -> Result<Vec<(Pubkey, Account)>> {
            self.queries
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok(self.accounts.lock().unwrap().clone())
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
            unreachable!()
        }
        async fn send_transaction(&self, _tx: &Transaction) -> Result<Signature> {
            unreachable!()
        }
        async fn recent_priority_fee(&self, _accounts: &[Pubkey]) -> Result<u64> {
            unreachable!()
        }
    }

    fn owned_by(program: Pubkey) -> Account {
        Account {
            lamports: 1,
            data: Vec::new(),
            owner: program,
            executable: false,
            rent_epoch: 0,
        }
    }

    /// A warm start is a read like any other and goes stale like any other.
    ///
    /// The membership of a program's account set only changes through the
    /// subscription, so while one is live and delivering the cache may answer
    /// from itself; once it is not, the set the cache holds is a snapshot of
    /// whenever the feed died and has to be re-read.
    #[tokio::test]
    async fn a_warm_start_expires_on_the_same_coverage_rule_as_an_account() {
        let program = Pubkey::new_unique();
        let (sender, receiver) = feed_channel();
        let inner = ProgramSource::new(vec![(Pubkey::new_unique(), owned_by(program))]);
        let source = CachedSource::new(
            inner,
            receiver,
            CachedSourceConfig {
                max_age_covered: Duration::from_secs(3600),
                max_age_uncovered: Duration::ZERO,
                ..Default::default()
            },
        );

        // Covered by a live subscription: the first query warms, the second
        // is answered from the cache.
        sender.set_coverage(crate::feed::Coverage {
            accounts: HashSet::new(),
            programs: HashSet::from([program]),
        });
        source.with_state(|state, _| state.last_feed_update = Some(Instant::now()));
        source.get_program_accounts(&program, &[]).await.unwrap();
        source.get_program_accounts(&program, &[]).await.unwrap();
        assert_eq!(
            source.inner().queries(),
            1,
            "a covered program was re-read while its subscription was live"
        );

        // Coverage withdrawn: the cache no longer knows what it is missing,
        // so every query re-reads.
        sender.set_coverage(crate::feed::Coverage::default());
        source.get_program_accounts(&program, &[]).await.unwrap();
        assert_eq!(
            source.inner().queries(),
            2,
            "an uncovered program was served from a cache nothing is filling"
        );
    }

    /// An account the provider stops returning is gone — closed, or
    /// reassigned to another owner. Re-reading is what notices; dropping it
    /// from the index is what makes the notice count, because the answer is
    /// assembled from the index rather than from the provider's reply.
    #[tokio::test]
    async fn a_re_read_forgets_an_account_that_is_no_longer_there() {
        let program = Pubkey::new_unique();
        let staying = Pubkey::new_unique();
        let closing = Pubkey::new_unique();
        let (_sender, receiver) = feed_channel();
        let inner = ProgramSource::new(vec![
            (staying, owned_by(program)),
            (closing, owned_by(program)),
        ]);
        // No coverage and no feed, so every query re-reads.
        let source = CachedSource::new(
            inner,
            receiver,
            CachedSourceConfig {
                max_age_uncovered: Duration::ZERO,
                ..Default::default()
            },
        );

        let first = source.get_program_accounts(&program, &[]).await.unwrap();
        assert_eq!(first.len(), 2);

        source.inner().serve(vec![(staying, owned_by(program))]);
        let second = source.get_program_accounts(&program, &[]).await.unwrap();
        assert_eq!(
            second.iter().map(|(pk, _)| *pk).collect::<Vec<_>>(),
            vec![staying],
            "a closed account survived the re-read that should have forgotten it"
        );
    }

    /// The fork state machine, driven directly. There is no way to make a
    /// single-node test validator fork, so this is where the behaviour is
    /// pinned; the transports' job is only to translate their own slot
    /// notifications into these events.
    struct Harness {
        state: CacheState,
        config: CachedSourceConfig,
        sender: crate::feed::FeedSender,
    }

    impl Harness {
        fn new() -> Self {
            let (sender, receiver) = feed_channel();
            Self {
                state: CacheState {
                    feed: receiver,
                    accounts: HashMap::new(),
                    indexed_keys: HashSet::new(),
                    interested: HashSet::new(),
                    warmed: HashMap::new(),
                    last_feed_update: None,
                    fork_tip: 0,
                    root: 0,
                },
                config: CachedSourceConfig::default(),
                sender,
            }
        }

        /// An account write observed at `slot`.
        fn wrote(&mut self, pubkey: Pubkey, slot: u64) {
            self.sender
                .updates
                .send(AccountUpdate {
                    pubkey,
                    account: Some(Account {
                        lamports: slot,
                        ..Default::default()
                    }),
                    slot,
                })
                .unwrap();
        }

        fn slot(&mut self, update: SlotUpdate) {
            self.sender.set_slot(update);
        }

        fn drain(&mut self) {
            CachedSource::<crate::source::RpcSource>::drain(&mut self.state, &self.config);
        }

        fn cached(&self, pubkey: &Pubkey) -> Option<u64> {
            self.state.accounts.get(pubkey).map(|entry| entry.slot)
        }
    }

    /// Slots get skipped constantly on a live cluster: the parent of a new
    /// slot is routinely below `slot - 1`. That is not a fork switch, and
    /// treating it as one would throw the cache away every few slots.
    #[test]
    fn skipped_slots_are_not_a_fork_switch() {
        let mut h = Harness::new();
        let account = Pubkey::new_unique();

        h.slot(SlotUpdate::Processed {
            slot: 100,
            parent: Some(99),
        });
        h.wrote(account, 100);
        h.drain();
        assert_eq!(h.cached(&account), Some(100));

        // 101 and 102 skipped; 103 builds straight on 100, which is the tip.
        h.slot(SlotUpdate::Processed {
            slot: 103,
            parent: Some(100),
        });
        h.drain();
        // Surviving the skip *is* the assertion: an invalidation would have
        // dropped this. The counter is a process-global static, so asserting
        // on it here would race the other tests in this module.
        assert_eq!(h.cached(&account), Some(100), "cache survived a skip");
    }

    /// The case the whole mechanism exists for: a write lands at `processed`
    /// on a fork that is then abandoned. No canonical write follows, so no
    /// correcting notification ever arrives — without this the phantom value
    /// would be served until the age ceiling expired.
    #[test]
    fn a_fork_switch_drops_writes_from_the_abandoned_fork() {
        let mut h = Harness::new();
        let (settled, phantom) = (Pubkey::new_unique(), Pubkey::new_unique());

        h.slot(SlotUpdate::Rooted { slot: 100 });
        h.slot(SlotUpdate::Processed {
            slot: 101,
            parent: Some(100),
        });
        h.wrote(settled, 100);
        h.wrote(phantom, 101);
        h.drain();
        assert_eq!(h.cached(&phantom), Some(101));

        // The cluster switches: a new slot built on 100, so 101 is gone.
        h.slot(SlotUpdate::Processed {
            slot: 102,
            parent: Some(100),
        });
        h.drain();
        assert_eq!(h.cached(&phantom), None, "provisional write survived");
        assert_eq!(
            h.cached(&settled),
            Some(100),
            "a write at or below the fork point is settled and must be kept"
        );
    }

    /// A slot number reappearing is the other shape of the same thing.
    #[test]
    fn a_repeated_slot_is_a_fork_switch() {
        let mut h = Harness::new();
        let account = Pubkey::new_unique();

        h.slot(SlotUpdate::Processed {
            slot: 200,
            parent: Some(199),
        });
        h.wrote(account, 200);
        h.drain();

        h.slot(SlotUpdate::Processed {
            slot: 200,
            parent: Some(199),
        });
        h.drain();
        assert_eq!(h.cached(&account), None);
    }

    /// Confirmed and finalized statuses repeat slots the tip has already
    /// passed. Only the processed status may move the tip, or every slot
    /// would read as a switch and the cache would never hold anything.
    #[test]
    fn rooted_events_never_read_as_a_fork_switch() {
        let mut h = Harness::new();
        let account = Pubkey::new_unique();

        h.slot(SlotUpdate::Processed {
            slot: 300,
            parent: Some(299),
        });
        h.wrote(account, 300);
        h.drain();

        (295..=300).for_each(|slot| h.slot(SlotUpdate::Rooted { slot }));
        h.drain();
        assert_eq!(h.cached(&account), Some(300));
        assert_eq!(h.state.root, 300);
    }

    /// Dropping an account must drop it from the program index too, or a
    /// cached `get_program_accounts` would answer with a key it can no
    /// longer resolve.
    #[test]
    fn invalidation_also_clears_the_program_index() {
        let mut h = Harness::new();
        let account = Pubkey::new_unique();

        h.slot(SlotUpdate::Processed {
            slot: 400,
            parent: Some(399),
        });
        h.wrote(account, 400);
        h.drain();
        h.state.indexed_keys.insert(account);

        h.slot(SlotUpdate::Processed {
            slot: 401,
            parent: Some(399),
        });
        h.drain();
        assert!(h.state.accounts.is_empty());
        assert!(h.state.indexed_keys.is_empty());
    }

    /// Without a parent (a transport that reports only the slot), the last
    /// rooted slot is the fallback floor.
    #[test]
    fn a_missing_parent_falls_back_to_the_root() {
        let mut h = Harness::new();
        let (settled, provisional) = (Pubkey::new_unique(), Pubkey::new_unique());

        h.slot(SlotUpdate::Rooted { slot: 500 });
        h.slot(SlotUpdate::Processed {
            slot: 505,
            parent: None,
        });
        h.wrote(settled, 500);
        h.wrote(provisional, 505);
        h.drain();

        h.slot(SlotUpdate::Processed {
            slot: 502,
            parent: None,
        });
        h.drain();
        assert_eq!(h.cached(&settled), Some(500));
        assert_eq!(h.cached(&provisional), None);
    }
}
