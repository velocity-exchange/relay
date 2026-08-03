//! What a turner is willing to crank.
//!
//! The registry is permissionless: anyone can register a watch pointing at
//! any account. Without a filter, a protocol that registers thousands of
//! multi-megabyte targets paying one lamport a crank would consume a
//! turner's bandwidth, subscription budget, and simulation time for free.
//! [`WatchFilter`] is how an operator scopes their turner to work they
//! actually want.
//!
//! Filters apply in cost order, cheapest first:
//!
//! 1. **Server-side** — `target_program` (and `creator`) lead the
//!    `WatchV0` layout, so an allowlist becomes a `getProgramAccounts` /
//!    geyser memcmp filter. Watches outside it are never even transmitted.
//! 2. **Registry-only** — denylists and target pins are evaluated against
//!    the 112-byte watch account alone, before any target is fetched.
//! 3. **Post-fetch** — target size and owner-drift checks, once, at
//!    registry-refresh time.
//! 4. **Post-parse** — the per-condition fee bar. A watch whose conditions
//!    all pay under it is dropped from the working set until the next
//!    refresh, so its target stops being fetched and subscribed.
//!
//! Only the first is enforceable by the RPC/geyser provider; the rest are
//! local. All of them are resource policy, never correctness — the on-chain
//! `crank_v0` payment assert is what actually guarantees a turner gets paid.

use std::collections::HashSet;

use solana_sdk::pubkey::Pubkey;

use crate::turner::Watch;

/// Why a watch was excluded, for logging and metrics.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum RejectReason {
    /// Target program is not on the allowlist.
    ProgramNotAllowed,
    /// Target program is on the denylist.
    ProgramBlocked,
    /// Watch creator is not on the allowlist.
    CreatorNotAllowed,
    /// Target pubkey is not on the allowlist.
    TargetNotAllowed,
    /// Target account is larger than `max_target_bytes`.
    TargetTooLarge,
    /// Target account is missing.
    TargetMissing,
    /// The recorded `target_program` no longer matches the account's owner.
    OwnerDrift,
    /// Target data at the watch offset is not a parseable condition block.
    Unparseable,
    /// No active condition pays at least `min_crank_payment`.
    PaysTooLittle,
    /// Dropped by `max_watches` after everything else passed.
    OverCapacity,
}

/// Operator policy for which watches a turner tracks. Empty allowlists mean
/// "no restriction"; a turner with a default filter cranks everything it can
/// profitably serve.
#[derive(Debug, Clone, Default)]
pub struct WatchFilter {
    /// Only track watches whose target is owned by one of these programs.
    /// Pushed down to the RPC/geyser provider as a memcmp filter, so
    /// non-matching watches never reach the turner.
    pub allowed_target_programs: HashSet<Pubkey>,
    /// Never track these programs. Applied after the allowlist.
    pub blocked_target_programs: HashSet<Pubkey>,
    /// Only track watches registered by these keys.
    pub allowed_creators: HashSet<Pubkey>,
    /// Only track these exact target accounts.
    pub allowed_targets: HashSet<Pubkey>,
    /// Drop watches whose target account exceeds this size. Targets are
    /// fetched every tick and (on subscription transports) streamed, so a
    /// huge account is a standing cost.
    pub max_target_bytes: Option<usize>,
    /// Hard ceiling on tracked watches, applied last.
    pub max_watches: Option<usize>,
}

/// Stable metric label for a rejection. Spelled out rather than derived so
/// renaming a variant cannot silently rename a dashboard series.
pub fn reject_label(reason: &RejectReason) -> &'static str {
    match reason {
        RejectReason::ProgramNotAllowed => "program_not_allowed",
        RejectReason::ProgramBlocked => "program_blocked",
        RejectReason::CreatorNotAllowed => "creator_not_allowed",
        RejectReason::TargetNotAllowed => "target_not_allowed",
        RejectReason::TargetTooLarge => "target_too_large",
        RejectReason::TargetMissing => "target_missing",
        RejectReason::OwnerDrift => "owner_drift",
        RejectReason::Unparseable => "unparseable",
        RejectReason::PaysTooLittle => "pays_too_little",
        RejectReason::OverCapacity => "over_capacity",
    }
}

impl WatchFilter {
    /// Allowlist a single program — the common case for a protocol running
    /// a turner for its own cranks.
    pub fn for_programs(programs: impl IntoIterator<Item = Pubkey>) -> Self {
        Self {
            allowed_target_programs: programs.into_iter().collect(),
            ..Self::default()
        }
    }

    /// Programs to push down to the provider as a server-side filter, or
    /// empty for "everything". Callers issue one filtered query per entry.
    pub fn server_side_programs(&self) -> Vec<Pubkey> {
        let mut programs: Vec<Pubkey> = self
            .allowed_target_programs
            .iter()
            .filter(|pk| !self.blocked_target_programs.contains(pk))
            .copied()
            .collect();
        programs.sort_by_key(|pk| pk.to_bytes());
        programs
    }

    /// Registry-only admission: everything decidable from the watch account
    /// itself, before a single target byte is fetched.
    pub fn check_registry(&self, watch: &Watch) -> Result<(), RejectReason> {
        if !self.allowed_target_programs.is_empty()
            && !self.allowed_target_programs.contains(&watch.target_program)
        {
            return Err(RejectReason::ProgramNotAllowed);
        }
        if self.blocked_target_programs.contains(&watch.target_program) {
            return Err(RejectReason::ProgramBlocked);
        }
        if !self.allowed_creators.is_empty() && !self.allowed_creators.contains(&watch.creator) {
            return Err(RejectReason::CreatorNotAllowed);
        }
        if !self.allowed_targets.is_empty() && !self.allowed_targets.contains(&watch.target) {
            return Err(RejectReason::TargetNotAllowed);
        }
        Ok(())
    }

    /// Post-fetch admission: what the target account itself reveals.
    pub fn check_target(
        &self,
        watch: &Watch,
        account: Option<&solana_sdk::account::Account>,
    ) -> Result<(), RejectReason> {
        let account = account.ok_or(RejectReason::TargetMissing)?;
        if self
            .max_target_bytes
            .is_some_and(|max| account.data.len() > max)
        {
            return Err(RejectReason::TargetTooLarge);
        }
        // The registry records the owner at registration; if the account
        // has since changed hands the recorded program is a lie by now.
        if account.owner != watch.target_program {
            return Err(RejectReason::OwnerDrift);
        }
        Ok(())
    }
}

/// Per-refresh accounting of what the filter admitted and dropped.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RefreshSummary {
    pub admitted: usize,
    pub rejected: Vec<(Watch, RejectReason)>,
}

impl RefreshSummary {
    pub fn rejected_for(&self, reason: RejectReason) -> usize {
        self.rejected
            .iter()
            .filter(|(_, why)| *why == reason)
            .count()
    }
}
