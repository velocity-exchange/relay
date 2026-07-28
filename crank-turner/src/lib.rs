//! Generic relay crank turner.
//!
//! The turner never encodes target-program logic. Everything it needs comes
//! from three places, all owned by the target program:
//!
//! 1. **Watches** — relay `WatchV0` accounts pointing at
//!    `(target account, offset)` condition blocks ([`relay_spec`]).
//! 2. **Wake hints** — each condition says when it's worth re-simulating
//!    (timestamp, watched-bytes-dirty, every-N-slots). Hints are
//!    discovery-only: firing early costs a simulation, and the instruction
//!    itself is the authoritative predicate.
//! 3. **Resolvers** — read-only instructions the turner *simulates*; their
//!    return data names the executor's account list and args, so account
//!    resolution is program code too (the on-chain twin of a
//!    `getRemainingAccounts` helper).
//!
//! Per due condition: simulate resolver → no work? record and move on →
//! otherwise build the executor (wrapped in `crank_v0`, which asserts the
//! keeper got paid `min_payment` — so simulation success implies payment),
//! simulate, send. A stale local view therefore costs at most a wasted
//! simulation or a landed-but-failed transaction fee — never a wrong crank.
//!
//! Chain access goes through [`source::ChainSource`] so transports are
//! pluggable: RPC polling ships here ([`source::RpcSource`]); geyser or
//! websocket sources slot in behind the same trait; tests drive the full
//! loop against litesvm.

pub mod source;
pub mod turner;

pub use source::{ChainSource, ClockSnapshot, RpcSource, SimOutcome};
pub use turner::{CondKey, Outcome, SkipReason, Stage, Turner, TurnerConfig, Watch};
