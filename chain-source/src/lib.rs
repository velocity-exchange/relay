//! Pluggable Solana chain access.
//!
//! One trait — [`ChainSource`] — with composable decorators, so a client
//! chooses its transport and its simulation strategy without either leaking
//! into its logic:
//!
//! - [`RpcSource`] — plain RPC polling (the floor, zero infra).
//! - [`CachedSource`] fed by [`spawn_ws_feed`] —
//!   `programSubscribe`/`accountSubscribe`.
//! - [`CachedSource`] fed by [`spawn_grpc_feed`] — Yellowstone/geyser gRPC.
//! - [`LocalSimSource`] — simulate in-process against a lazy fork of cached
//!   state instead of shipping every simulation to an RPC provider.
//!
//! The stack that motivated the split is `LocalSimSource<CachedSource<RpcSource>>`:
//! subscriptions replace *reads*, simulation runs locally in microseconds,
//! and only sends, blockhashes and confirmations reach the network.
//!
//! Two capabilities make it useful beyond cranking. `simulate_transaction`
//! returns post-execution state for accounts the caller names, so a program
//! that stages a payload in its own account can be read without ever landing
//! a transaction. And [`CachedSource`]'s watch-programs mode streams every
//! account owned by a named program, so simulations of that program's
//! instructions almost never pay an RPC fetch.
//!
//! Nothing here knows about any particular protocol: program-account queries
//! and subscriptions take transport-neutral [`AccountFilter`]s that the
//! caller builds.

pub mod cached;
pub mod feed;
pub mod grpc;
pub mod local_sim;
pub mod metrics;
pub mod source;
pub mod ws;

pub use cached::{CachedSource, CachedSourceConfig};
pub use feed::{feed_channel, AccountUpdate, Coverage, FeedReceiver, FeedSender};
pub use grpc::{spawn_grpc_feed, GrpcFeedConfig, ProgramSubscription};
pub use local_sim::{LocalSimConfig, LocalSimSource};
pub use source::{
    AccountFilter, BlockhashInfo, ChainSource, ClockSnapshot, RpcSource, SignatureOutcome,
    SimOutcome,
};
pub use ws::{derive_ws_url, spawn_ws_feed};
