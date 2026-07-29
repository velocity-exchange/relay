//! The push half of a subscribed source: backends (websocket, gRPC) stream
//! [`AccountUpdate`]s into a [`crate::cached::CachedSource`], and the source
//! publishes the set of pubkeys it wants watched back to the backend.
//!
//! Backends also publish [`Coverage`]: what they currently have live
//! subscriptions for. That is what lets the cache tell "no news because
//! nothing changed" (safe to serve) apart from "no news because nobody is
//! listening" (must revalidate) — the distinction that keeps a stale
//! account out of a simulation.
//!
//! Kept deliberately dumb — everything stateful (slot ordering, cache
//! merging, freshness) lives in `CachedSource`, which is what the tests pin.

use std::collections::HashSet;

use solana_sdk::account::Account;
use solana_sdk::pubkey::Pubkey;
use tokio::sync::{mpsc, watch};

/// One account observation from a subscription backend.
#[derive(Debug, Clone)]
pub struct AccountUpdate {
    pub pubkey: Pubkey,
    /// `None` = account closed/absent at this slot.
    pub account: Option<Account>,
    pub slot: u64,
}

/// What a backend currently has live subscriptions for. Empty means no
/// live session, so nothing may be trusted on the strength of silence.
#[derive(Debug, Clone, Default)]
pub struct Coverage {
    /// Individually subscribed pubkeys.
    pub accounts: HashSet<Pubkey>,
    /// Programs whose accounts are streamed wholesale.
    pub programs: HashSet<Pubkey>,
}

impl Coverage {
    /// Is this account's every write guaranteed to reach us?
    pub fn covers(&self, pubkey: &Pubkey, owner: Option<&Pubkey>) -> bool {
        self.accounts.contains(pubkey) || owner.is_some_and(|owner| self.programs.contains(owner))
    }

    pub fn is_empty(&self) -> bool {
        self.accounts.is_empty() && self.programs.is_empty()
    }
}

/// Backend side: push updates, watch the interest set, publish coverage.
#[derive(Clone)]
pub struct FeedSender {
    pub updates: mpsc::UnboundedSender<AccountUpdate>,
    pub interest: watch::Receiver<HashSet<Pubkey>>,
    pub coverage: watch::Sender<Coverage>,
}

impl FeedSender {
    /// Declare what this backend is now streaming. Call after a successful
    /// subscribe, and with [`Coverage::default`] the moment a session
    /// drops — the cache stops trusting silence immediately.
    pub fn set_coverage(&self, coverage: Coverage) {
        let _ = self.coverage.send(coverage);
    }
}

/// Source side: drain updates, publish interest, read coverage.
pub struct FeedReceiver {
    pub updates: mpsc::UnboundedReceiver<AccountUpdate>,
    pub interest: watch::Sender<HashSet<Pubkey>>,
    pub coverage: watch::Receiver<Coverage>,
}

pub fn feed_channel() -> (FeedSender, FeedReceiver) {
    let (update_tx, update_rx) = mpsc::unbounded_channel();
    let (interest_tx, interest_rx) = watch::channel(HashSet::new());
    let (coverage_tx, coverage_rx) = watch::channel(Coverage::default());
    (
        FeedSender {
            updates: update_tx,
            interest: interest_rx,
            coverage: coverage_tx,
        },
        FeedReceiver {
            updates: update_rx,
            interest: interest_tx,
            coverage: coverage_rx,
        },
    )
}
