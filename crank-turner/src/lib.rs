//! Generic relay crank turner.
//!
//! The turner never encodes target-program logic. Everything it needs comes
//! from three places, all owned by the target program:
//!
//! 1. **Watches** — relay `WatchV0` accounts pointing at
//!    `(target account, offset)` condition blocks ([`relay_spec`]).
//! 2. **Wake hints** — each condition says when it's worth re-simulating
//!    (timestamp, slot, watched-bytes-changed, value-crossed,
//!    every-N-slots). Hints are discovery-only: firing early costs a
//!    simulation, and the instruction itself is the authoritative predicate.
//! 3. **Resolvers** — instructions the turner *simulates*, told which
//!    condition fired ([`relay_spec::FiredConditionV0`], appended to the
//!    instruction data); they stage the whole executor — program,
//!    discriminator, accounts, args — in one of their own accounts and
//!    return a pointer to it, so both account resolution and *which*
//!    instruction to run are program code (the on-chain twin of a
//!    `getRemainingAccounts` helper). Because the resolver is only
//!    simulated, the staging write never lands on chain.
//!
//! Per due condition: simulate resolver → no work? record and move on →
//! otherwise read the staged payload from post-simulation account state,
//! build the executor it names and bracket it with relay's payment guards
//! (`assert_paid_v0` reverts unless the keeper gained `min_payment`, so
//! simulation success implies payment), simulate, send. A stale local view
//! therefore costs at most a wasted simulation or a landed-but-failed
//! transaction fee — never a wrong crank.
//!
//! Chain access goes through [`ChainSource`] from `relay-chain-source`, and
//! the transports mirror what the velocity keeper stack uses:
//!
//! - [`RpcSource`] — plain RPC polling (the floor, zero infra).
//! - [`CachedSource`] fed by [`spawn_ws_feed`] —
//!   `programSubscribe`/`accountSubscribe`, the TS-keeper-bot path.
//! - [`CachedSource`] fed by [`spawn_grpc_feed`] — Yellowstone/geyser gRPC,
//!   the keep-rs path.
//!
//! Subscriptions only replace *reads*; simulation and submission always go
//! to RPC. Tests drive the full loop against litesvm.

pub mod filter;
pub mod metrics;
pub mod submit;
pub mod turner;
pub mod watches;

// Chain access lives in `relay-chain-source`, which knows nothing about
// relay: the watch-registry filters that make it relay-aware are built here
// (see [`watches`]) and handed in as generic `AccountFilter`s.
pub use filter::{RefreshSummary, RejectReason, WatchFilter};
pub use relay_chain_source::ws::derive_ws_url;
pub use relay_chain_source::{
    feed_channel, spawn_grpc_feed, spawn_ws_feed, AccountFilter, AccountUpdate, BlockhashInfo,
    CachedSource, CachedSourceConfig, ChainSource, ClockSnapshot, Coverage, FeedReceiver,
    FeedSender, GrpcFeedConfig, LocalSimConfig, LocalSimSource, ProgramSubscription, RpcSource,
    SignatureOutcome, SimOutcome,
};
pub use submit::{
    spawn as spawn_submitter, LagSnapshot, PendingTx, ProfitSnapshot, SubmitterConfig,
    SubmitterHandle, TxResult,
};
pub use turner::{
    names_transaction_signer, CondKey, Explanation, Outcome, SkipReason, Stage, Turner,
    TurnerConfig, Verdict, Watch, RELAY_PROGRAM_ID,
};
pub use watches::{watch_filter_sets, watch_subscription};
