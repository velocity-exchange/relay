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
use std::ops::ControlFlow;
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
    SubscribeRequestFilterSlots, SubscribeRequestPing, SubscribeUpdate, SubscribeUpdateAccount,
    SubscribeUpdateSlot,
};

use crate::feed::{AccountUpdate, Coverage, FeedSender, SlotUpdate};
use crate::source::AccountFilter;

#[derive(Debug, Clone)]
pub struct GrpcFeedConfig {
    pub endpoint: String,
    pub x_token: Option<String>,
}

/// A provider-side subscription to accounts owned by one program.
///
/// `filter_sets` is a union of alternatives, matching
/// [`ChainSource::get_program_accounts`](crate::ChainSource::get_program_accounts):
/// each inner set becomes one stream entry (filters ANDed), because a memcmp
/// matches a single value. Empty means "every account this program owns".
#[derive(Debug, Clone)]
pub struct ProgramSubscription {
    pub program: Pubkey,
    pub filter_sets: Vec<Vec<AccountFilter>>,
}

impl ProgramSubscription {
    /// Every account owned by `program` — the mode that keeps local
    /// simulation off the network.
    pub fn all(program: Pubkey) -> Self {
        Self {
            program,
            filter_sets: Vec::new(),
        }
    }
}

/// Subscribe to `subscriptions` (each a program plus optional filters) and
/// the caller's explicit interest set. Filtered subscriptions keep another
/// protocol's accounts from ever crossing the wire; unfiltered ones make a
/// program's whole state resident so simulating its instructions is free.
pub fn spawn_grpc_feed(
    config: GrpcFeedConfig,
    subscriptions: Vec<ProgramSubscription>,
    feed: FeedSender,
) -> JoinHandle<()> {
    tokio::spawn(run(config, subscriptions, feed))
}

/// Programs whose subscription carries no filters, and are therefore fully
/// covered by the stream.
fn unfiltered_programs(subscriptions: &[ProgramSubscription]) -> std::collections::HashSet<Pubkey> {
    subscriptions
        .iter()
        .filter(|subscription| subscription.filter_sets.is_empty())
        .map(|subscription| subscription.program)
        .collect()
}

fn to_grpc_filter(filter: &AccountFilter) -> SubscribeRequestFilterAccountsFilter {
    SubscribeRequestFilterAccountsFilter {
        filter: Some(match filter {
            AccountFilter::DataSize(len) => AccountsFilterOneof::Datasize(*len),
            AccountFilter::Memcmp { offset, bytes } => {
                AccountsFilterOneof::Memcmp(SubscribeRequestFilterAccountsFilterMemcmp {
                    offset: *offset as u64,
                    data: Some(AccountsFilterMemcmpOneof::Bytes(bytes.clone())),
                })
            }
        }),
    }
}

fn build_request(
    subscriptions: &[ProgramSubscription],
    interest: &HashSet<Pubkey>,
) -> SubscribeRequest {
    let owned_entries: Vec<(String, SubscribeRequestFilterAccounts)> = subscriptions
        .iter()
        .enumerate()
        .flat_map(|(i, subscription)| {
            let owner = vec![subscription.program.to_string()];
            if subscription.filter_sets.is_empty() {
                return vec![(
                    format!("owned-{i}"),
                    SubscribeRequestFilterAccounts {
                        owner,
                        ..Default::default()
                    },
                )];
            }
            subscription
                .filter_sets
                .iter()
                .enumerate()
                .map(|(j, set)| {
                    (
                        format!("owned-{i}-{j}"),
                        SubscribeRequestFilterAccounts {
                            owner: owner.clone(),
                            filters: set.iter().map(to_grpc_filter).collect(),
                            ..Default::default()
                        },
                    )
                })
                .collect()
        })
        .collect();
    let accounts: HashMap<String, SubscribeRequestFilterAccounts> = owned_entries
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
        .collect();
    SubscribeRequest {
        accounts,
        // Slot statuses, for fork detection: a `processed` account write can
        // be taken back, and nothing in the account stream says so.
        slots: [(
            "slots".to_string(),
            SubscribeRequestFilterSlots {
                filter_by_commitment: Some(false),
                ..Default::default()
            },
        )]
        .into_iter()
        .collect(),
        commitment: Some(CommitmentLevel::Processed as i32),
        ..Default::default()
    }
}

/// Why a live subscription ended.
enum SessionEnd {
    /// The stream is gone or unusable; reconnect after a delay.
    Reconnect,
    /// The feed or the interest channel was dropped; the backend shuts down.
    ShutDown,
}

async fn run(config: GrpcFeedConfig, subscriptions: Vec<ProgramSubscription>, feed: FeedSender) {
    let mut backoff = Duration::from_secs(1);
    let mut interest = feed.interest.clone();
    loop {
        let Some(mut client) = connect(&config, &mut backoff).await else {
            continue;
        };

        let request = build_request(&subscriptions, &interest.borrow_and_update());
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
        // Subscribed: silence about these accounts now means "unchanged".
        publish_coverage(&feed, &interest, &subscriptions);
        debug!("grpc subscribed");

        if let SessionEnd::ShutDown =
            pump(&mut stream, &mut sink, &feed, &mut interest, &subscriptions).await
        {
            return;
        }

        // The delay before reconnecting is deliberate: a provider that
        // accepts connections and then immediately errors the stream would
        // otherwise be reconnected to as fast as the loop can turn.
        tokio::time::sleep(backoff).await;
        backoff = (backoff * 2).min(Duration::from_secs(30));
    }
}

/// One connection attempt. `None` means it failed and the delay has already
/// been served, so the caller retries.
async fn connect(
    config: &GrpcFeedConfig,
    backoff: &mut Duration,
) -> Option<GeyserGrpcClient<impl yellowstone_grpc_client::Interceptor>> {
    let built = GeyserGrpcClient::build_from_shared(config.endpoint.clone())
        .and_then(|builder| builder.x_token(config.x_token.clone()))
        .and_then(|builder| builder.tls_config(ClientTlsConfig::new().with_native_roots()));
    let attempt = match built {
        Ok(builder) => builder.connect().await.map_err(|err| format!("{err}")),
        Err(err) => Err(format!("{err}")),
    };
    match attempt {
        Ok(client) => Some(client),
        Err(error) => {
            warn!(%error, "grpc connect failed; retrying in {backoff:?}");
            tokio::time::sleep(*backoff).await;
            *backoff = (*backoff * 2).min(Duration::from_secs(30));
            None
        }
    }
}

/// Vouch for what this subscription now covers.
///
/// Only an unfiltered subscription covers every account a program owns; a
/// filtered one says nothing about what it excludes.
fn publish_coverage(
    feed: &FeedSender,
    interest: &tokio::sync::watch::Receiver<HashSet<Pubkey>>,
    subscriptions: &[ProgramSubscription],
) {
    feed.set_coverage(Coverage {
        accounts: interest.borrow().iter().copied().collect(),
        programs: unfiltered_programs(subscriptions),
    });
}

/// Pump one live subscription until it dies or the interest set moves.
async fn pump<St, Si, E>(
    stream: &mut St,
    sink: &mut Si,
    feed: &FeedSender,
    interest: &mut tokio::sync::watch::Receiver<HashSet<Pubkey>>,
    subscriptions: &[ProgramSubscription],
) -> SessionEnd
where
    St: futures_util::Stream<Item = Result<SubscribeUpdate, E>> + Unpin,
    Si: futures_util::Sink<SubscribeRequest> + Unpin,
    E: std::fmt::Display,
{
    loop {
        tokio::select! {
            message = stream.next() => match message {
                Some(Ok(msg)) => match msg.update_oneof {
                    Some(UpdateOneof::Account(update)) => match feed_account(feed, update) {
                        ControlFlow::Continue(()) => {}
                        ControlFlow::Break(end) => return end,
                    },
                    Some(UpdateOneof::Slot(update)) => feed_slot(feed, update),
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
                    feed.set_coverage(Coverage::default());
                    return SessionEnd::Reconnect;
                }
                None => {
                    warn!("grpc stream ended; reconnecting");
                    feed.set_coverage(Coverage::default());
                    return SessionEnd::Reconnect;
                }
            },
            changed = interest.changed() => match changed {
                Ok(()) => {
                    // Filter update on the live stream — no reconnect.
                    let request = build_request(subscriptions, &interest.borrow_and_update());
                    if sink.send(request).await.is_err() {
                        warn!("grpc filter update failed; reconnecting");
                        feed.set_coverage(Coverage::default());
                        return SessionEnd::Reconnect;
                    }
                    // Newly interested accounts are covered from here.
                    publish_coverage(feed, interest, subscriptions);
                }
                Err(_) => return SessionEnd::ShutDown, // interest sender dropped
            },
        }
    }
}

/// Forward one account update, and say whether the session can continue.
/// It cannot once the feed receiver is gone: there is nothing left to serve.
///
/// An update that cannot be decoded is dropped rather than reported: the
/// stream is still good, and the next update may well be readable.
fn feed_account(feed: &FeedSender, update: SubscribeUpdateAccount) -> ControlFlow<SessionEnd> {
    let Some(info) = update.account else {
        return ControlFlow::Continue(());
    };
    let (Ok(pubkey), Ok(owner)) = (
        Pubkey::try_from(info.pubkey.as_slice()),
        Pubkey::try_from(info.owner.as_slice()),
    ) else {
        return ControlFlow::Continue(());
    };
    match feed.updates.send(AccountUpdate {
        pubkey,
        account: Some(Account {
            lamports: info.lamports,
            data: info.data,
            owner,
            executable: info.executable,
            rent_epoch: info.rent_epoch,
        }),
        slot: update.slot,
    }) {
        Ok(()) => ControlFlow::Continue(()),
        Err(_) => ControlFlow::Break(SessionEnd::ShutDown),
    }
}

/// Forward one slot event.
///
/// One event arrives per status per slot, so only the processed one may move
/// the fork tip — confirmed and finalized repeat a slot already passed and
/// would read as a switch every slot.
fn feed_slot(feed: &FeedSender, update: SubscribeUpdateSlot) {
    let event = match CommitmentLevel::try_from(update.status) {
        Ok(CommitmentLevel::Processed) => SlotUpdate::Processed {
            slot: update.slot,
            parent: update.parent,
        },
        Ok(CommitmentLevel::Finalized) => SlotUpdate::Rooted { slot: update.slot },
        _ => return,
    };
    feed.set_slot(event);
}
