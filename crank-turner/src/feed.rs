//! The push half of a subscribed source: backends (websocket, gRPC) stream
//! [`AccountUpdate`]s into a [`crate::cached::CachedSource`], and the source
//! publishes the set of pubkeys it wants watched back to the backend.
//!
//! Kept deliberately dumb — everything stateful (slot ordering, cache
//! merging, fallback) lives in `CachedSource`, which is what the tests pin.

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

/// Backend side: push updates, watch the interest set.
#[derive(Clone)]
pub struct FeedSender {
    pub updates: mpsc::UnboundedSender<AccountUpdate>,
    pub interest: watch::Receiver<HashSet<Pubkey>>,
}

/// Source side: drain updates, publish interest.
pub struct FeedReceiver {
    pub updates: mpsc::UnboundedReceiver<AccountUpdate>,
    pub interest: watch::Sender<HashSet<Pubkey>>,
}

pub fn feed_channel() -> (FeedSender, FeedReceiver) {
    let (update_tx, update_rx) = mpsc::unbounded_channel();
    let (interest_tx, interest_rx) = watch::channel(HashSet::new());
    (
        FeedSender {
            updates: update_tx,
            interest: interest_rx,
        },
        FeedReceiver {
            updates: update_rx,
            interest: interest_tx,
        },
    )
}
