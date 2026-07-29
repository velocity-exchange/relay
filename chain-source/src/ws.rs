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
use solana_sdk::pubkey::Pubkey;
use solana_sdk::sysvar;
use tokio::task::JoinHandle;
use tracing::{debug, warn};

use crate::feed::{AccountUpdate, Coverage, FeedSender};
use crate::grpc::ProgramSubscription;
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

    // One `programSubscribe` per (program, filter set); an unfiltered
    // subscription is a single stream over everything the program owns.
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
    let mut program_streams = Vec::new();
    let mut program_unsubs = Vec::new();
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
                program_streams.push(stream);
                program_unsubs.push(unsub);
            }
            Err(err) => return SessionEnd::failed(format!("program_subscribe {program}: {err}")),
        }
    }

    // Interested accounts (targets, change-watched accounts, clock): one
    // account subscription each, tagged with its pubkey.
    let mut wanted: Vec<Pubkey> = interest.borrow_and_update().iter().copied().collect();
    if !wanted.contains(&sysvar::clock::id()) {
        wanted.push(sysvar::clock::id());
    }
    let mut streams = Vec::new();
    let mut unsubs = program_unsubs;
    for pubkey in &wanted {
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
                            account: response.value.decode(),
                            slot: response.context.slot,
                        })
                        .boxed(),
                );
                unsubs.push(unsub);
            }
            Err(err) => return SessionEnd::failed(format!("account_subscribe {pubkey}: {err}")),
        }
    }
    streams.extend(program_streams.into_iter().map(|stream| {
        stream
            .map(|response| AccountUpdate {
                pubkey: response
                    .value
                    .pubkey
                    .parse()
                    .unwrap_or_else(|_| Pubkey::default()),
                account: response.value.account.decode(),
                slot: response.context.slot,
            })
            .boxed()
    }));
    // Everything above is subscribed, so silence about these accounts now
    // means "unchanged" rather than "nobody listening".
    feed.set_coverage(Coverage {
        accounts: wanted.iter().copied().collect(),
        // Only an unfiltered subscription covers every account a program
        // owns; a filtered one says nothing about the accounts it excludes.
        programs: subscriptions
            .iter()
            .filter(|subscription| subscription.filter_sets.is_empty())
            .map(|subscription| subscription.program)
            .collect(),
    });
    debug!(subscriptions = streams.len(), "ws session subscribed");

    let mut merged = futures_util::stream::select_all(streams);
    let end = loop {
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
            changed = interest.changed() => break match changed {
                Ok(()) => SessionEnd::InterestChanged,
                Err(_) => SessionEnd::Failed {
                    established: true,
                    error: "interest sender dropped".into(),
                },
            },
        }
    };
    drop(merged);
    // The session is over: stop vouching for anything before the
    // reconnect, so the cache revalidates in the meantime.
    feed.set_coverage(Coverage::default());
    futures_util::future::join_all(unsubs.into_iter().map(|unsub| unsub())).await;
    end
}
