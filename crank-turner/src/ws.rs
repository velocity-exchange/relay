//! Websocket subscription backend: `programSubscribe` on the relay program
//! (watch registry) + `accountSubscribe` per interested pubkey — the same
//! paths velocity's TS keeper bots and velocity-rs's ws mode use.
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
use solana_sdk::pubkey::Pubkey;
use solana_sdk::sysvar;
use tokio::task::JoinHandle;
use tracing::{debug, warn};

use crate::feed::{AccountUpdate, Coverage, FeedSender};

/// `target_programs` narrows the watch-registry subscription to those
/// programs (one subscription each); empty subscribes to every watch.
pub fn spawn_ws_feed(
    ws_url: String,
    relay_program: Pubkey,
    target_programs: Vec<Pubkey>,
    feed: FeedSender,
) -> JoinHandle<()> {
    spawn_ws_feed_with_programs(ws_url, relay_program, target_programs, Vec::new(), feed)
}

/// `watch_programs` streams every account owned by those programs, which
/// is what keeps local simulation off the network.
pub fn spawn_ws_feed_with_programs(
    ws_url: String,
    relay_program: Pubkey,
    target_programs: Vec<Pubkey>,
    watch_programs: Vec<Pubkey>,
    feed: FeedSender,
) -> JoinHandle<()> {
    tokio::spawn(run(
        ws_url,
        relay_program,
        target_programs,
        watch_programs,
        feed,
    ))
}

/// Derive a websocket url from an RPC url the way the Solana CLI does.
pub fn derive_ws_url(rpc_url: &str) -> String {
    let ws = rpc_url
        .replacen("https://", "wss://", 1)
        .replacen("http://", "ws://", 1);
    ws.replacen(":8899", ":8900", 1)
}

async fn run(
    ws_url: String,
    relay_program: Pubkey,
    target_programs: Vec<Pubkey>,
    watch_programs: Vec<Pubkey>,
    feed: FeedSender,
) {
    let mut backoff = Duration::from_secs(1);
    let mut interest = feed.interest.clone();
    loop {
        match session(
            &ws_url,
            relay_program,
            &target_programs,
            &watch_programs,
            &feed,
            &mut interest,
        )
        .await
        {
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
    relay_program: Pubkey,
    target_programs: &[Pubkey],
    watch_programs: &[Pubkey],
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

    // Watch registry: one program subscription per allowed target program
    // (memcmp can only match one value), or a single unfiltered one.
    let watch_queries: Vec<Option<Pubkey>> = if target_programs.is_empty() {
        vec![None]
    } else {
        target_programs.iter().copied().map(Some).collect()
    };
    let mut program_streams = Vec::new();
    let mut program_unsubs = Vec::new();
    for target_program in &watch_queries {
        match client
            .program_subscribe(
                &relay_program,
                Some(RpcProgramAccountsConfig {
                    filters: Some(crate::source::watch_filters(target_program.as_ref())),
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
            Err(err) => return SessionEnd::failed(format!("program_subscribe: {err}")),
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
    // Whole-program subscriptions: everything these programs own, so a
    // local simulation finds its accounts already cached.
    for program in watch_programs {
        match client
            .program_subscribe(
                program,
                Some(RpcProgramAccountsConfig {
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
                            account: response.value.account.decode(),
                            slot: response.context.slot,
                        })
                        .boxed(),
                );
                unsubs.push(unsub);
            }
            Err(err) => return SessionEnd::failed(format!("program_subscribe {program}: {err}")),
        }
    }
    // Everything above is subscribed, so silence about these accounts now
    // means "unchanged" rather than "nobody listening".
    feed.set_coverage(Coverage {
        accounts: wanted.iter().copied().collect(),
        programs: watch_programs.iter().copied().collect(),
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
