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
    subscribe_update::UpdateOneof, SubscribeRequest, SubscribeRequestFilterAccounts,
    SubscribeRequestFilterAccountsFilter, SubscribeRequestPing,
};

use crate::feed::{AccountUpdate, FeedSender};

#[derive(Debug, Clone)]
pub struct GrpcFeedConfig {
    pub endpoint: String,
    pub x_token: Option<String>,
}

pub fn spawn_grpc_feed(
    config: GrpcFeedConfig,
    relay_program: Pubkey,
    feed: FeedSender,
) -> JoinHandle<()> {
    tokio::spawn(run(config, relay_program, feed))
}

fn build_request(relay_program: &Pubkey, interest: &HashSet<Pubkey>) -> SubscribeRequest {
    let accounts: HashMap<String, SubscribeRequestFilterAccounts> = [
        (
            "watches".to_string(),
            SubscribeRequestFilterAccounts {
                owner: vec![relay_program.to_string()],
                filters: vec![SubscribeRequestFilterAccountsFilter {
                    filter: Some(AccountsFilterOneof::Datasize(
                        relay_spec::WATCH_V0_LEN as u64,
                    )),
                }],
                ..Default::default()
            },
        ),
        (
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
        ),
    ]
    .into();
    SubscribeRequest {
        accounts,
        commitment: Some(CommitmentLevel::Processed as i32),
        ..Default::default()
    }
}

async fn run(config: GrpcFeedConfig, relay_program: Pubkey, feed: FeedSender) {
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

        let request = build_request(&relay_program, &interest.borrow_and_update());
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
                        let request =
                            build_request(&relay_program, &interest.borrow_and_update());
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
