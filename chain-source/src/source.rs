//! Chain access behind a trait so transports are pluggable.
//!
//! [`RpcSource`](crate::rpc::RpcSource) is the reference implementation
//! (polling); [`crate::cached::CachedSource`] wraps it with a
//! subscription-fed cache for the websocket and gRPC paths;
//! [`crate::local_sim::LocalSimSource`] moves simulation in-process. Tests
//! use a litesvm-backed source.
//!
//! Program-account queries take [`AccountFilter`]s rather than any
//! protocol's account layout, so this layer stays generic: the caller says
//! "this size, these bytes at this offset" and each transport translates
//! that into its own filter language.

use anyhow::Result;
use async_trait::async_trait;
use solana_rpc_client_api::filter::{Memcmp, RpcFilterType};
use solana_sdk::account::Account;
use solana_sdk::hash::Hash;
use solana_sdk::pubkey::Pubkey;
use solana_sdk::signature::Signature;
use solana_sdk::transaction::VersionedTransaction;

/// Chain time, as the scheduler's single notion of "now" — always the
/// on-chain clock, never wall time, so wakes can't fire early relative to
/// what an executor's `Clock::get()` will see.
#[derive(Debug, Clone, Copy)]
pub struct ClockSnapshot {
    pub slot: u64,
    pub unix_timestamp: i64,
}

/// A blockhash and the block height past which it can no longer land.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BlockhashInfo {
    pub hash: Hash,
    pub last_valid_block_height: u64,
}

/// What became of a submitted signature, as far as the cluster knows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SignatureOutcome {
    Landed,
    Failed(String),
}

/// Simulation result, reduced to what the turner decides on.
#[derive(Debug, Clone, Default)]
pub struct SimOutcome {
    /// None = simulation succeeded.
    pub err: Option<String>,
    pub logs: Vec<String>,
    pub return_data: Option<Vec<u8>>,
    /// Compute units the simulation burned, for sizing the CU limit.
    pub units_consumed: u64,
    /// Post-execution state of the accounts the caller asked for, in the
    /// order requested. This is how a resolver's staged payload gets read
    /// without ever landing a transaction.
    pub accounts: Vec<Option<Account>>,
}

#[async_trait]
pub trait ChainSource: Send + Sync {
    async fn get_multiple_accounts(&self, pubkeys: &[Pubkey]) -> Result<Vec<Option<Account>>>;

    /// Accounts owned by `program`, pre-filtered by the provider.
    ///
    /// `filter_sets` is a union of alternatives: each inner set is one
    /// provider-side query (filters within a set are ANDed), and the results
    /// are concatenated. That shape exists because a memcmp can only match
    /// one value, so an allowlist of N values is N queries — still far
    /// cheaper than downloading everything and discarding it locally. An
    /// empty outer slice means one unfiltered query.
    async fn get_program_accounts(
        &self,
        program: &Pubkey,
        filter_sets: &[Vec<AccountFilter>],
    ) -> Result<Vec<(Pubkey, Account)>>;

    async fn clock(&self) -> Result<ClockSnapshot>;

    async fn latest_blockhash(&self) -> Result<BlockhashInfo>;

    /// Current block height, for deciding whether a blockhash has expired.
    async fn block_height(&self) -> Result<u64>;

    /// Status of submitted signatures, in the order asked for. `None` =
    /// not yet observed.
    async fn signature_statuses(
        &self,
        signatures: &[Signature],
    ) -> Result<Vec<Option<SignatureOutcome>>>;

    /// Simulate, returning post-execution data for `return_accounts` (in
    /// order) alongside logs and return data.
    async fn simulate_transaction(
        &self,
        tx: &VersionedTransaction,
        return_accounts: &[Pubkey],
    ) -> Result<SimOutcome>;

    async fn send_transaction(&self, tx: &VersionedTransaction) -> Result<Signature>;

    /// A recent prioritization fee (micro-lamports per CU) for
    /// transactions touching `accounts`. Providers differ wildly here, so
    /// treat it as a hint and clamp it.
    async fn recent_priority_fee(&self, accounts: &[Pubkey]) -> Result<u64>;
}

/// Sharing a source between the turner and the submitter is the norm, so
/// `Arc` forwards the trait rather than making every caller deref.
#[async_trait]
impl<T: ChainSource + ?Sized> ChainSource for std::sync::Arc<T> {
    async fn get_multiple_accounts(&self, pubkeys: &[Pubkey]) -> Result<Vec<Option<Account>>> {
        (**self).get_multiple_accounts(pubkeys).await
    }
    async fn get_program_accounts(
        &self,
        program: &Pubkey,
        filter_sets: &[Vec<AccountFilter>],
    ) -> Result<Vec<(Pubkey, Account)>> {
        (**self).get_program_accounts(program, filter_sets).await
    }
    async fn clock(&self) -> Result<ClockSnapshot> {
        (**self).clock().await
    }
    async fn latest_blockhash(&self) -> Result<BlockhashInfo> {
        (**self).latest_blockhash().await
    }
    async fn block_height(&self) -> Result<u64> {
        (**self).block_height().await
    }
    async fn signature_statuses(
        &self,
        signatures: &[Signature],
    ) -> Result<Vec<Option<SignatureOutcome>>> {
        (**self).signature_statuses(signatures).await
    }
    async fn simulate_transaction(
        &self,
        tx: &VersionedTransaction,
        return_accounts: &[Pubkey],
    ) -> Result<SimOutcome> {
        (**self).simulate_transaction(tx, return_accounts).await
    }
    async fn send_transaction(&self, tx: &VersionedTransaction) -> Result<Signature> {
        (**self).send_transaction(tx).await
    }
    async fn recent_priority_fee(&self, accounts: &[Pubkey]) -> Result<u64> {
        (**self).recent_priority_fee(accounts).await
    }
}

/// A provider-side account filter, in terms every transport can express.
/// Callers build these from their own account layouts; nothing in this crate
/// knows what the bytes mean.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AccountFilter {
    /// Exact account data length.
    DataSize(u64),
    /// `bytes` appear at `offset` in the account data.
    Memcmp { offset: usize, bytes: Vec<u8> },
}

impl AccountFilter {
    /// Convenience for a discriminator-style prefix match.
    pub fn prefix(bytes: impl Into<Vec<u8>>) -> Self {
        AccountFilter::Memcmp {
            offset: 0,
            bytes: bytes.into(),
        }
    }

    pub(crate) fn to_rpc(&self) -> RpcFilterType {
        match self {
            AccountFilter::DataSize(len) => RpcFilterType::DataSize(*len),
            AccountFilter::Memcmp { offset, bytes } => {
                RpcFilterType::Memcmp(Memcmp::new_raw_bytes(*offset, bytes.clone()))
            }
        }
    }

    /// Evaluate locally. A cache has to re-apply filters itself, because the
    /// backend's subscription may be broader than a given query.
    pub fn matches(&self, account: &Account) -> bool {
        match self {
            AccountFilter::DataSize(len) => account.data.len() as u64 == *len,
            AccountFilter::Memcmp { offset, bytes } => account
                .data
                .get(*offset..offset.saturating_add(bytes.len()))
                .is_some_and(|slice| slice == bytes.as_slice()),
        }
    }
}
