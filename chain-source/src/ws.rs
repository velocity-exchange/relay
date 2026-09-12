//! Websocket subscription backend: `programSubscribe` per
//! [`ProgramSubscription`] + `accountSubscribe` per interested pubkey.
//!
//! Deliberately thin: translate notifications into
//! [`crate::feed::AccountUpdate`]s and rebuild the world on interest change
//! or connection loss. All merge/ordering logic lives in `CachedSource`.

use std::time::Duration;

use futures_util::StreamExt;
use solana_account_decoder::UiAccountEncoding;
use solana_commitment_config::CommitmentConfig;
use solana_pubsub_client::nonblocking::pubsub_client::PubsubClient;
use solana_rpc_client_api::config::{RpcAccountInfoConfig, RpcProgramAccountsConfig};
use solana_rpc_client_api::filter::RpcFilterType;
use solana_rpc_client_api::response::SlotInfo;
use solana_sdk::pubkey::Pubkey;
use solana_sdk::sysvar;
use tokio::task::JoinHandle;
use tracing::{debug, warn};

use crate::feed::{AccountUpdate, Coverage, FeedSender, SlotUpdate};

/// The unsubscribe closure `PubsubClient` hands back with each subscription.
type Unsub = Box<dyn FnOnce() -> futures_util::future::BoxFuture<'static, ()> + Send>;

/// A subscription's updates, borrowed from the client that opened it.
type Updates<'a> = futures_util::stream::BoxStream<'a, AccountUpdate>;
use crate::grpc::ProgramSubscription;
use crate::rpc::decode_ui_account;
use crate::source::AccountFilter;

/// Subscribe to each [`ProgramSubscription`] (filtered sets become one
/// `programSubscribe` each, since a memcmp matches one value) plus the
/// caller's interest set. An unfiltered subscription streams everything a
/// program owns, which is what keeps local simulation off the network.
pub fn spawn_ws_feed(
    ws_url: String,
    subscriptions: Vec<ProgramSubscription>,
    feed: FeedSender,
) -> JoinHandle<()> {
    tokio::spawn(run(ws_url, subscriptions, feed))
}

/// Derive a websocket url from an RPC url the way the Solana CLI does.
pub fn derive_ws_url(rpc_url: &str) -> String {
    let ws = rpc_url
        .replacen("https://", "wss://", 1)
        .replacen("http://", "ws://", 1);
    ws.replacen(":8899", ":8900", 1)
}

async fn run(ws_url: String, subscriptions: Vec<ProgramSubscription>, feed: FeedSender) {
    let mut backoff = Duration::from_secs(1);
    let mut interest = feed.interest.clone();
    loop {
        match session(&ws_url, &subscriptions, &feed, &mut interest).await {
            SessionEnd::InterestChanged => {
                // Immediate rebuild with the new set.
                backoff = Duration::from_secs(1);
            }
            SessionEnd::Failed { established, error } => {
                // A session that was live and dropped is not evidence the
                // endpoint is down, so do not inherit the delay built up
                // by earlier connect failures.
                if established {
                    backoff = Duration::from_secs(1);
                }
                warn!(error = %error, "ws session failed; reconnecting in {backoff:?}");
                tokio::time::sleep(backoff).await;
                backoff = (backoff * 2).min(Duration::from_secs(30));
            }
        }
    }
}

enum SessionEnd {
    InterestChanged,
    Failed {
        /// Whether this session ever got as far as subscribing. A
        /// connection that worked and then dropped should be retried
        /// promptly; only repeated *failures to establish* deserve a
        /// growing delay.
        established: bool,
        error: String,
    },
}

impl SessionEnd {
    fn failed(error: impl Into<String>) -> Self {
        Self::Failed {
            established: false,
            error: error.into(),
        }
    }
}

/// Everything one connected session subscribes to, and how it gives those
/// subscriptions back.
///
/// The teardown is the reason this is a type: unsubscribing is the same work
/// whether the session ended because the interest set moved or because the
/// connection died, and a path that forgets it leaks a subscription per
/// reconnect.
struct Subscriptions<'a> {
    /// Account and program updates, already tagged with their pubkey.
    streams: Vec<Updates<'a>>,
    unsubs: Vec<Unsub>,
    /// The accounts this session vouches for, once everything is subscribed.
    covered_accounts: Vec<Pubkey>,
}

/// One connected session: subscribe to everything, pump until the interest
/// set changes or the connection dies.
async fn session(
    ws_url: &str,
    subscriptions: &[ProgramSubscription],
    feed: &FeedSender,
    interest: &mut tokio::sync::watch::Receiver<std::collections::HashSet<Pubkey>>,
) -> SessionEnd {
    feed.set_coverage(Coverage::default());
    let client = match PubsubClient::new(ws_url).await {
        Ok(client) => client,
        Err(err) => return SessionEnd::failed(format!("connect: {err}")),
    };
    let account_config = RpcAccountInfoConfig {
        encoding: Some(UiAccountEncoding::Base64),
        commitment: Some(CommitmentConfig::processed()),
        ..Default::default()
    };

    let (program_streams, mut unsubs) =
        match subscribe_programs(&client, subscriptions, &account_config).await {
            Ok(parts) => parts,
            Err(end) => return end,
        };

    // Slots, for fork detection. `slotSubscribe` carries the parent and the
    // root, which is everything the cache needs to tell a skipped slot from
    // an abandoned fork. A provider that does not support it degrades to
    // age-based revalidation rather than losing the session.
    let (mut slot_stream, unsubs_slot) = match client.slot_subscribe().await {
        Ok((stream, unsub)) => (Some(stream), Some(unsub)),
        Err(err) => {
            warn!(error = %err, "slot_subscribe failed; fork detection disabled this session");
            (None, None)
        }
    };

    let wanted = wanted_accounts(interest);
    let mut streams = match subscribe_accounts(&client, &wanted, &account_config, &mut unsubs).await
    {
        Ok(streams) => streams,
        Err(end) => return end,
    };
    streams.extend(program_streams);
    let subscribed = Subscriptions {
        streams,
        unsubs,
        covered_accounts: wanted,
    };

    // Everything above is subscribed, so silence about these accounts now
    // means "unchanged" rather than "nobody listening".
    feed.set_coverage(Coverage {
        accounts: subscribed.covered_accounts.iter().copied().collect(),
        // Only an unfiltered subscription covers every account a program
        // owns; a filtered one says nothing about the accounts it excludes.
        programs: subscriptions
            .iter()
            .filter(|subscription| subscription.filter_sets.is_empty())
            .map(|subscription| subscription.program)
            .collect(),
    });
    debug!(
        subscriptions = subscribed.streams.len(),
        "ws session subscribed"
    );

    let Subscriptions {
        streams, unsubs, ..
    } = subscribed;
    let mut merged = futures_util::stream::select_all(streams);
    let end = pump(&mut merged, &mut slot_stream, feed, interest).await;

    drop(merged);
    drop(slot_stream);
    tear_down(feed, unsubs, unsubs_slot).await;
    end
}

/// Give every subscription back, and stop vouching for what they covered.
///
/// The coverage reset happens before the unsubscribes complete on purpose:
/// the session is already over, and the cache must revalidate from the first
/// read after it rather than from whenever the provider acknowledges.
async fn tear_down(feed: &FeedSender, unsubs: Vec<Unsub>, slot: Option<Unsub>) {
    if let Some(unsub) = slot {
        unsub().await;
    }
    feed.set_coverage(Coverage::default());
    futures_util::future::join_all(unsubs.into_iter().map(|unsub| unsub())).await;
}

/// One `programSubscribe` per (program, filter set); an unfiltered
/// subscription is a single stream over everything the program owns.
async fn subscribe_programs<'a>(
    client: &'a PubsubClient,
    subscriptions: &[ProgramSubscription],
    account_config: &RpcAccountInfoConfig,
) -> Result<(Vec<Updates<'a>>, Vec<Unsub>), SessionEnd> {
    let queries: Vec<(Pubkey, Option<Vec<RpcFilterType>>)> = subscriptions
        .iter()
        .flat_map(|subscription| {
            if subscription.filter_sets.is_empty() {
                return vec![(subscription.program, None)];
            }
            subscription
                .filter_sets
                .iter()
                .map(|set| {
                    (
                        subscription.program,
                        Some(set.iter().map(AccountFilter::to_rpc).collect()),
                    )
                })
                .collect()
        })
        .collect();

    let mut streams = Vec::new();
    let mut unsubs = Vec::new();
    for (program, filters) in &queries {
        match client
            .program_subscribe(
                program,
                Some(RpcProgramAccountsConfig {
                    filters: filters.clone(),
                    account_config: account_config.clone(),
                    ..Default::default()
                }),
            )
            .await
        {
            Ok((stream, unsub)) => {
                streams.push(
                    stream
                        .map(|response| AccountUpdate {
                            pubkey: response
                                .value
                                .pubkey
                                .parse()
                                .unwrap_or_else(|_| Pubkey::default()),
                            account: decode_ui_account(response.value.account),
                            slot: response.context.slot,
                        })
                        .boxed(),
                );
                unsubs.push(unsub);
            }
            Err(err) => {
                return Err(SessionEnd::failed(format!(
                    "program_subscribe {program}: {err}"
                )))
            }
        }
    }
    Ok((streams, unsubs))
}

/// Interested accounts (targets, change-watched accounts, clock). The clock
/// is always in the set: it is an account like any other, and serving it
/// blind is what froze every time-based wake the last time a feed died.
fn wanted_accounts(
    interest: &mut tokio::sync::watch::Receiver<std::collections::HashSet<Pubkey>>,
) -> Vec<Pubkey> {
    let mut wanted: Vec<Pubkey> = interest.borrow_and_update().iter().copied().collect();
    if !wanted.contains(&sysvar::clock::id()) {
        wanted.push(sysvar::clock::id());
    }
    wanted
}

/// One account subscription each, tagged with its pubkey so the merged
/// stream still says what changed.
async fn subscribe_accounts<'a>(
    client: &'a PubsubClient,
    wanted: &[Pubkey],
    account_config: &RpcAccountInfoConfig,
    unsubs: &mut Vec<Unsub>,
) -> Result<Vec<Updates<'a>>, SessionEnd> {
    let mut streams = Vec::new();
    for pubkey in wanted {
        match client
            .account_subscribe(pubkey, Some(account_config.clone()))
            .await
        {
            Ok((stream, unsub)) => {
                let pk = *pubkey;
                streams.push(
                    stream
                        .map(move |response| AccountUpdate {
                            pubkey: pk,
                            account: decode_ui_account(response.value),
                            slot: response.context.slot,
                        })
                        .boxed(),
                );
                unsubs.push(unsub);
            }
            Err(err) => {
                return Err(SessionEnd::failed(format!(
                    "account_subscribe {pubkey}: {err}"
                )))
            }
        }
    }
    Ok(streams)
}

/// Pump updates until the interest set changes or the connection dies.
async fn pump(
    merged: &mut futures_util::stream::SelectAll<Updates<'_>>,
    slot_stream: &mut Option<impl futures_util::Stream<Item = SlotInfo> + Unpin>,
    feed: &FeedSender,
    interest: &mut tokio::sync::watch::Receiver<std::collections::HashSet<Pubkey>>,
) -> SessionEnd {
    loop {
        tokio::select! {
            update = merged.next() => match update {
                Some(update) => {
                    if feed.updates.send(update).is_err() {
                        break SessionEnd::Failed {
                            established: true,
                            error: "feed receiver dropped".into(),
                        };
                    }
                }
                // All streams ended: connection is gone.
                None => break SessionEnd::Failed {
                    established: true,
                    error: "ws streams ended".into(),
                },
            },
            // `Pin` dance avoided by only polling when a stream exists.
            slot = async {
                match slot_stream.as_mut() {
                    Some(stream) => stream.next().await,
                    // No slot subscription: park forever rather than
                    // spinning on `None`.
                    None => std::future::pending().await,
                }
            } => match slot {
                Some(info) => {
                    feed.set_slot(SlotUpdate::Processed {
                        slot: info.slot,
                        parent: Some(info.parent),
                    });
                    feed.set_slot(SlotUpdate::Rooted { slot: info.root });
                }
                None => break SessionEnd::Failed {
                    established: true,
                    error: "ws slot stream ended".into(),
                },
            },
            changed = interest.changed() => break match changed {
                Ok(()) => SessionEnd::InterestChanged,
                Err(_) => SessionEnd::Failed {
                    established: true,
                    error: "interest sender dropped".into(),
                },
            },
        }
    }
}
