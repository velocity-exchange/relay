//! Generic relay crank turner.
//!
//! The turner never encodes target-program logic. Everything it needs comes
//! from three places, all owned by the target program:
//!
//! 1. **Watches** — relay `WatchV0` accounts pointing at
//!    `(target account, offset)` condition blocks ([`relay_spec`]).
//! 2. **Wake hints** — each condition says when it's worth re-simulating
//!    (timestamp, slot, watched-bytes-changed, every-N-slots). Hints are
//!    discovery-only: firing early costs a simulation, and the instruction
//!    itself is the authoritative predicate.
//! 3. **Resolvers** — instructions the turner *simulates*; they stage the
//!    executor's account list and args in one of their own accounts and
//!    return a pointer to it, so account resolution is program code too
//!    (the on-chain twin of a `getRemainingAccounts` helper). Because the
//!    resolver is only simulated, the staging write never lands on chain.
//!
//! Per due condition: simulate resolver → no work? record and move on →
//! otherwise read the staged payload from post-simulation account state,
//! build the executor (wrapped in `crank_v0`, which asserts the keeper got
//! paid `min_payment` — so simulation success implies payment), simulate,
//! send. A stale local view therefore costs at most a wasted simulation or
//! a landed-but-failed transaction fee — never a wrong crank.
//!
//! Chain access goes through [`source::ChainSource`], and the transports
//! mirror what the velocity keeper stack uses:
//!
//! - [`source::RpcSource`] — plain RPC polling (the floor, zero infra).
//! - [`cached::CachedSource`] fed by [`ws::spawn_ws_feed`] —
//!   `programSubscribe`/`accountSubscribe`, the TS-keeper-bot path.
//! - [`cached::CachedSource`] fed by [`grpc::spawn_grpc_feed`] —
//!   Yellowstone/geyser gRPC, the keep-rs path.
//!
//! Subscriptions only replace *reads*; simulation and submission always go
//! to RPC. Tests drive the full loop against litesvm.

pub mod cached;
pub mod feed;
pub mod filter;
pub mod grpc;
pub mod local_sim;
pub mod metrics;
pub mod source;
pub mod submit;
pub mod turner;
pub mod ws;

pub use cached::{CachedSource, CachedSourceConfig};
pub use feed::{feed_channel, AccountUpdate, FeedReceiver, FeedSender};
pub use filter::{RefreshSummary, RejectReason, WatchFilter};
pub use grpc::{spawn_grpc_feed, GrpcFeedConfig};
pub use local_sim::{LocalSimConfig, LocalSimSource};
pub use source::{
    BlockhashInfo, ChainSource, ClockSnapshot, RpcSource, SignatureOutcome, SimOutcome,
};
pub use submit::{spawn as spawn_submitter, PendingTx, SubmitterConfig, SubmitterHandle, TxResult};
pub use turner::{
    CondKey, Outcome, SkipReason, Stage, Turner, TurnerConfig, Watch, RELAY_PROGRAM_ID,
};
pub use ws::{derive_ws_url, spawn_ws_feed};
