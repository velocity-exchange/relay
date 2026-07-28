//! Yellowstone/geyser gRPC subscription backend — the same transport
//! velocity's keep-rs hardcodes. One stream carrying two account filters:
//! owner-filtered watch-registry accounts and the explicit interest set
//! (targets, change-watched accounts, clock). Interest changes are pushed
//! as filter updates on the live stream; connection loss reconnects with
//! capped exponential backoff (rebuilding the client each time, per the
//! velocity-rs lesson about half-dead streams).
//!
//! Deliberately thin: translate updates into
//! [`crate::feed::AccountUpdate`]s. All merge/ordering logic lives in
//! `CachedSource`.

use std::collections::{HashMap, HashSet};
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use solana_sdk::account::Account;
use solana_sdk::pubkey::Pubkey;
use solana_sdk::sysvar;
use tokio::task::JoinHandle;
use tracing::{debug, warn};
use yellowstone_grpc_client::{ClientTlsConfig, GeyserGrpcClient};
use yellowstone_grpc_proto::geyser::CommitmentLevel;
use yellowstone_grpc_proto::prelude::{
    subscribe_request_filter_accounts_filter::Filter as AccountsFilterOneof,
    subscribe_request_filter_accounts_filter_memcmp::Data as AccountsFilterMemcmpOneof,
    subscribe_update::UpdateOneof, SubscribeRequest, SubscribeRequestFilterAccounts,
    SubscribeRequestFilterAccountsFilter, SubscribeRequestFilterAccountsFilterMemcmp,
    SubscribeRequestPing,
};

use crate::feed::{AccountUpdate, FeedSender};

#[derive(Debug, Clone)]
pub struct GrpcFeedConfig {
    pub endpoint: String,
    pub x_token: Option<String>,
}

/// `target_programs` narrows the watch-registry filter to those programs
/// (one filter entry each); empty streams every watch.
pub fn spawn_grpc_feed(
    config: GrpcFeedConfig,
    relay_program: Pubkey,
    target_programs: Vec<Pubkey>,
    feed: FeedSender,
) -> JoinHandle<()> {
    spawn_grpc_feed_with_programs(config, relay_program, target_programs, Vec::new(), feed)
}

/// `watch_programs` streams every account owned by those programs, which
/// is what keeps local simulation off the network.
pub fn spawn_grpc_feed_with_programs(
    config: GrpcFeedConfig,
    relay_program: Pubkey,
    target_programs: Vec<Pubkey>,
    watch_programs: Vec<Pubkey>,
    feed: FeedSender,
) -> JoinHandle<()> {
    tokio::spawn(run(
        config,
        relay_program,
        target_programs,
        watch_programs,
        feed,
    ))
}

/// Watch-registry filters: the relay program as owner, `WatchV0` size, and
/// — when the operator scoped the turner — a memcmp pinning
/// `target_program`, so other protocols' watches never cross the wire.
fn watch_filters(target_program: Option<&Pubkey>) -> Vec<SubscribeRequestFilterAccountsFilter> {
    [SubscribeRequestFilterAccountsFilter {
        filter: Some(AccountsFilterOneof::Datasize(
            relay_spec::WATCH_V0_LEN as u64,
        )),
    }]
    .into_iter()
    .chain(
        target_program.map(|pk| SubscribeRequestFilterAccountsFilter {
            filter: Some(AccountsFilterOneof::Memcmp(
                SubscribeRequestFilterAccountsFilterMemcmp {
                    offset: relay_spec::WATCH_TARGET_PROGRAM_OFFSET as u64,
                    data: Some(AccountsFilterMemcmpOneof::Bytes(pk.to_bytes().to_vec())),
                },
            )),
        }),
    )
    .collect()
}

fn build_request(
    relay_program: &Pubkey,
    target_programs: &[Pubkey],
    watch_programs: &[Pubkey],
    interest: &HashSet<Pubkey>,
) -> SubscribeRequest {
    let watch_entries: Vec<(String, SubscribeRequestFilterAccounts)> = if target_programs.is_empty()
    {
        vec![(
            "watches".to_string(),
            SubscribeRequestFilterAccounts {
                owner: vec![relay_program.to_string()],
                filters: watch_filters(None),
                ..Default::default()
            },
        )]
    } else {
        target_programs
            .iter()
            .enumerate()
            .map(|(i, target_program)| {
                (
                    format!("watches-{i}"),
                    SubscribeRequestFilterAccounts {
                        owner: vec![relay_program.to_string()],
                        filters: watch_filters(Some(target_program)),
                        ..Default::default()
                    },
                )
            })
            .collect()
    };
    let accounts: HashMap<String, SubscribeRequestFilterAccounts> = watch_entries
        .into_iter()
        .chain([(
            "interest".to_string(),
            SubscribeRequestFilterAccounts {
                account: interest
                    .iter()
                    .copied()
                    .chain([sysvar::clock::id()])
                    .map(|pk| pk.to_string())
                    .collect(),
                ..Default::default()
            },
        )])
        // Everything owned by the watched programs, so local simulation
        // finds its accounts already cached.
        .chain(watch_programs.iter().enumerate().map(|(i, program)| {
            (
                format!("owned-{i}"),
                SubscribeRequestFilterAccounts {
                    owner: vec![program.to_string()],
                    ..Default::default()
                },
            )
        }))
        .collect();
    SubscribeRequest {
        accounts,
        commitment: Some(CommitmentLevel::Processed as i32),
        ..Default::default()
    }
}

async fn run(
    config: GrpcFeedConfig,
    relay_program: Pubkey,
    target_programs: Vec<Pubkey>,
    watch_programs: Vec<Pubkey>,
    feed: FeedSender,
) {
    let mut backoff = Duration::from_secs(1);
    let mut interest = feed.interest.clone();
    loop {
        let client = GeyserGrpcClient::build_from_shared(config.endpoint.clone())
            .and_then(|builder| builder.x_token(config.x_token.clone()))
            .and_then(|builder| builder.tls_config(ClientTlsConfig::new().with_native_roots()));
        let mut client = match client {
            Ok(builder) => match builder.connect().await {
                Ok(client) => client,
                Err(err) => {
                    warn!(error = %err, "grpc connect failed; retrying in {backoff:?}");
                    tokio::time::sleep(backoff).await;
                    backoff = (backoff * 2).min(Duration::from_secs(30));
                    continue;
                }
            },
            Err(err) => {
                warn!(error = %err, "grpc builder failed; retrying in {backoff:?}");
                tokio::time::sleep(backoff).await;
                backoff = (backoff * 2).min(Duration::from_secs(30));
                continue;
            }
        };

        let request = build_request(
            &relay_program,
            &target_programs,
            &watch_programs,
            &interest.borrow_and_update(),
        );
        let (mut sink, mut stream) = match client.subscribe_with_request(Some(request)).await {
            Ok(pair) => pair,
            Err(err) => {
                warn!(error = %err, "grpc subscribe failed; retrying in {backoff:?}");
                tokio::time::sleep(backoff).await;
                backoff = (backoff * 2).min(Duration::from_secs(30));
                continue;
            }
        };
        backoff = Duration::from_secs(1);
        debug!("grpc subscribed");

        loop {
            tokio::select! {
                message = stream.next() => match message {
                    Some(Ok(msg)) => match msg.update_oneof {
                        Some(UpdateOneof::Account(update)) => {
                            let Some(info) = update.account else { continue };
                            let Ok(pubkey) = Pubkey::try_from(info.pubkey.as_slice()) else {
                                continue;
                            };
                            let Ok(owner) = Pubkey::try_from(info.owner.as_slice()) else {
                                continue;
                            };
                            let account = AccountUpdate {
                                pubkey,
                                account: Some(Account {
                                    lamports: info.lamports,
                                    data: info.data,
                                    owner,
                                    executable: info.executable,
                                    rent_epoch: info.rent_epoch,
                                }),
                                slot: update.slot,
                            };
                            if feed.updates.send(account).is_err() {
                                return; // receiver dropped: shut down
                            }
                        }
                        Some(UpdateOneof::Ping(_)) => {
                            // Keeps ping-expecting load balancers happy.
                            let _ = sink
                                .send(SubscribeRequest {
                                    ping: Some(SubscribeRequestPing { id: 1 }),
                                    ..Default::default()
                                })
                                .await;
                        }
                        _ => {}
                    },
                    Some(Err(err)) => {
                        warn!(error = %err, "grpc stream error; reconnecting");
                        break;
                    }
                    None => {
                        warn!("grpc stream ended; reconnecting");
                        break;
                    }
                },
                changed = interest.changed() => match changed {
                    Ok(()) => {
                        // Filter update on the live stream — no reconnect.
                        let request = build_request(
                            &relay_program,
                            &target_programs,
                            &watch_programs,
                            &interest.borrow_and_update(),
                        );
                        if let Err(err) = sink.send(request).await {
                            warn!(error = %err, "grpc filter update failed; reconnecting");
                            break;
                        }
                    }
                    Err(_) => return, // interest sender dropped: shut down
                },
            }
        }
    }
}
