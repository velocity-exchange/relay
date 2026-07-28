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
use solana_rpc_client_api::filter::{Memcmp, RpcFilterType};
use solana_sdk::pubkey::Pubkey;
use solana_sdk::sysvar;
use tokio::task::JoinHandle;
use tracing::{debug, warn};

use crate::feed::{AccountUpdate, FeedSender};

pub fn spawn_ws_feed(ws_url: String, relay_program: Pubkey, feed: FeedSender) -> JoinHandle<()> {
    tokio::spawn(run(ws_url, relay_program, feed))
}

/// Derive a websocket url from an RPC url the way the Solana CLI does.
pub fn derive_ws_url(rpc_url: &str) -> String {
    let ws = rpc_url
        .replacen("https://", "wss://", 1)
        .replacen("http://", "ws://", 1);
    ws.replacen(":8899", ":8900", 1)
}

async fn run(ws_url: String, relay_program: Pubkey, feed: FeedSender) {
    let mut backoff = Duration::from_secs(1);
    let mut interest = feed.interest.clone();
    loop {
        match session(&ws_url, relay_program, &feed, &mut interest).await {
            SessionEnd::InterestChanged => {
                // Immediate rebuild with the new set.
                backoff = Duration::from_secs(1);
            }
            SessionEnd::Failed(err) => {
                warn!(error = %err, "ws session failed; reconnecting in {backoff:?}");
                tokio::time::sleep(backoff).await;
                backoff = (backoff * 2).min(Duration::from_secs(30));
            }
        }
    }
}

enum SessionEnd {
    InterestChanged,
    Failed(String),
}

/// One connected session: subscribe to everything, pump until the interest
/// set changes or the connection dies.
async fn session(
    ws_url: &str,
    relay_program: Pubkey,
    feed: &FeedSender,
    interest: &mut tokio::sync::watch::Receiver<std::collections::HashSet<Pubkey>>,
) -> SessionEnd {
    let client = match PubsubClient::new(ws_url).await {
        Ok(client) => client,
        Err(err) => return SessionEnd::Failed(format!("connect: {err}")),
    };
    let account_config = RpcAccountInfoConfig {
        encoding: Some(UiAccountEncoding::Base64),
        commitment: Some(CommitmentConfig::processed()),
        ..Default::default()
    };

    // Watch registry: one program subscription, filtered to WatchV0 shape.
    let (program_stream, program_unsub) = match client
        .program_subscribe(
            &relay_program,
            Some(RpcProgramAccountsConfig {
                filters: Some(vec![
                    RpcFilterType::DataSize(relay_spec::WATCH_V0_LEN as u64),
                    RpcFilterType::Memcmp(Memcmp::new_raw_bytes(
                        0,
                        relay_spec::WATCH_V0_DISCRIMINATOR.to_vec(),
                    )),
                ]),
                account_config: account_config.clone(),
                ..Default::default()
            }),
        )
        .await
    {
        Ok(sub) => sub,
        Err(err) => return SessionEnd::Failed(format!("program_subscribe: {err}")),
    };

    // Interested accounts (targets, change-watched accounts, clock): one
    // account subscription each, tagged with its pubkey.
    let mut wanted: Vec<Pubkey> = interest.borrow_and_update().iter().copied().collect();
    if !wanted.contains(&sysvar::clock::id()) {
        wanted.push(sysvar::clock::id());
    }
    let mut streams = Vec::new();
    let mut unsubs = vec![program_unsub];
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
            Err(err) => return SessionEnd::Failed(format!("account_subscribe {pubkey}: {err}")),
        }
    }
    streams.push(
        program_stream
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
    debug!(subscriptions = streams.len(), "ws session subscribed");

    let mut merged = futures_util::stream::select_all(streams);
    let end = loop {
        tokio::select! {
            update = merged.next() => match update {
                Some(update) => {
                    if feed.updates.send(update).is_err() {
                        break SessionEnd::Failed("feed receiver dropped".into());
                    }
                }
                // All streams ended: connection is gone.
                None => break SessionEnd::Failed("ws streams ended".into()),
            },
            changed = interest.changed() => break match changed {
                Ok(()) => SessionEnd::InterestChanged,
                Err(_) => SessionEnd::Failed("interest sender dropped".into()),
            },
        }
    };
    drop(merged);
    futures_util::future::join_all(unsubs.into_iter().map(|unsub| unsub())).await;
    end
}
