//! Chain access behind a trait so transports are pluggable. `RpcSource` is
//! the reference implementation (polling); [`crate::cached::CachedSource`]
//! wraps it with a subscription-fed cache for the websocket and gRPC paths;
//! [`crate::local_sim::LocalSimSource`] moves simulation in-process. Tests
//! use a litesvm-backed source.
//!
//! Program-account queries take [`AccountFilter`]s rather than any
//! protocol's account layout, so this layer stays generic: the caller says
//! "this size, these bytes at this offset" and each transport translates
//! that into its own filter language.

use anyhow::{anyhow, Context, Result};
use async_trait::async_trait;
use base64::Engine as _;
use futures_util::StreamExt;
use solana_account_decoder::{UiAccountData, UiAccountEncoding};
use solana_client::nonblocking::rpc_client::RpcClient;
use solana_commitment_config::CommitmentConfig;
use solana_rpc_client_api::config::{
    RpcAccountInfoConfig, RpcProgramAccountsConfig, RpcSendTransactionConfig,
    RpcSimulateTransactionAccountsConfig, RpcSimulateTransactionConfig,
};
use solana_rpc_client_api::filter::{Memcmp, RpcFilterType};
use solana_sdk::account::Account;
use solana_sdk::clock::Clock;
use solana_sdk::hash::Hash;
use solana_sdk::pubkey::Pubkey;
use solana_sdk::signature::Signature;
use solana_sdk::sysvar;
use solana_sdk::transaction::Transaction;
use solana_transaction_status_client_types::TransactionConfirmationStatus;

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
        tx: &Transaction,
        return_accounts: &[Pubkey],
    ) -> Result<SimOutcome>;

    async fn send_transaction(&self, tx: &Transaction) -> Result<Signature>;

    /// A recent prioritization fee (micro-lamports per CU) for
    /// transactions touching `accounts`. Providers differ wildly here, so
    /// treat it as a hint and clamp it.
    async fn recent_priority_fee(&self, accounts: &[Pubkey]) -> Result<u64>;
}

/// RPC's per-call ceiling for `getMultipleAccounts`.
const MAX_ACCOUNTS_PER_CALL: usize = 100;
/// How many of those chunk calls to have in flight at once.
const MAX_CONCURRENT_ACCOUNT_CALLS: usize = 8;

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
        tx: &Transaction,
        return_accounts: &[Pubkey],
    ) -> Result<SimOutcome> {
        (**self).simulate_transaction(tx, return_accounts).await
    }
    async fn send_transaction(&self, tx: &Transaction) -> Result<Signature> {
        (**self).send_transaction(tx).await
    }
    async fn recent_priority_fee(&self, accounts: &[Pubkey]) -> Result<u64> {
        (**self).recent_priority_fee(accounts).await
    }
}

/// RPC-polling source. Reads at `processed` commitment — fast, possibly
/// provisional state is fine because every action is simulated before it is
/// sent, and sent transactions are re-validated on chain.
pub struct RpcSource {
    client: RpcClient,
}

impl RpcSource {
    pub fn new(url: String) -> Self {
        Self {
            client: RpcClient::new_with_commitment(url, CommitmentConfig::processed()),
        }
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

fn decode_ui_account(ui: solana_account_decoder::UiAccount) -> Option<Account> {
    let data = match &ui.data {
        UiAccountData::Binary(blob, UiAccountEncoding::Base64) => {
            base64::engine::general_purpose::STANDARD
                .decode(blob)
                .ok()?
        }
        _ => ui.decode::<Account>().map(|a| a.data)?,
    };
    Some(Account {
        lamports: ui.lamports,
        data,
        owner: ui.owner.parse().ok()?,
        executable: ui.executable,
        rent_epoch: ui.rent_epoch,
    })
}

#[async_trait]
impl ChainSource for RpcSource {
    async fn get_multiple_accounts(&self, pubkeys: &[Pubkey]) -> Result<Vec<Option<Account>>> {
        // RPC rejects more than 100 keys per call, and a turner tracking a
        // real registry passes that on every tick. `buffered` preserves
        // input order, so concatenating the chunk replies keeps each slot
        // aligned with the key that asked for it.
        let chunks: Vec<Vec<Pubkey>> = pubkeys
            .chunks(MAX_ACCOUNTS_PER_CALL)
            .map(<[Pubkey]>::to_vec)
            .collect();
        let replies: Vec<_> = futures_util::stream::iter(chunks)
            .map(|chunk| async move { self.client.get_multiple_accounts(&chunk).await })
            .buffered(MAX_CONCURRENT_ACCOUNT_CALLS)
            .collect()
            .await;
        replies
            .into_iter()
            .collect::<std::result::Result<Vec<_>, _>>()
            .context("get_multiple_accounts")
            .map(|chunks| chunks.into_iter().flatten().collect())
    }

    async fn get_program_accounts(
        &self,
        program: &Pubkey,
        filter_sets: &[Vec<AccountFilter>],
    ) -> Result<Vec<(Pubkey, Account)>> {
        let empty: Vec<Vec<AccountFilter>> = vec![Vec::new()];
        let queries = if filter_sets.is_empty() {
            &empty[..]
        } else {
            filter_sets
        };
        let mut out = Vec::new();
        for set in queries {
            let filters: Vec<RpcFilterType> = set.iter().map(AccountFilter::to_rpc).collect();
            // Deprecated in favor of the ui-accounts variant, but this one
            // returns `Account` directly, which is what the trait wants.
            #[allow(deprecated)]
            let accounts = self
                .client
                .get_program_accounts_with_config(
                    program,
                    RpcProgramAccountsConfig {
                        filters: Some(filters),
                        account_config: RpcAccountInfoConfig {
                            encoding: Some(UiAccountEncoding::Base64),
                            ..Default::default()
                        },
                        ..Default::default()
                    },
                )
                .await
                .context("get_program_accounts")?;
            out.extend(accounts);
        }
        Ok(out)
    }

    async fn clock(&self) -> Result<ClockSnapshot> {
        let account = self
            .client
            .get_account(&sysvar::clock::id())
            .await
            .context("get clock sysvar")?;
        let clock: Clock = bincode::deserialize(&account.data).context("decode clock sysvar")?;
        Ok(ClockSnapshot {
            slot: clock.slot,
            unix_timestamp: clock.unix_timestamp,
        })
    }

    async fn latest_blockhash(&self) -> Result<BlockhashInfo> {
        let (hash, last_valid_block_height) = self
            .client
            .get_latest_blockhash_with_commitment(CommitmentConfig::confirmed())
            .await
            .context("get_latest_blockhash")?;
        Ok(BlockhashInfo {
            hash,
            last_valid_block_height,
        })
    }

    async fn block_height(&self) -> Result<u64> {
        self.client.get_block_height().await.context("block_height")
    }

    async fn signature_statuses(
        &self,
        signatures: &[Signature],
    ) -> Result<Vec<Option<SignatureOutcome>>> {
        // One call per 256 signatures is the RPC limit; batches this large
        // are already unusual for a single turner.
        let mut out = Vec::with_capacity(signatures.len());
        for chunk in signatures.chunks(256) {
            let statuses = self
                .client
                .get_signature_statuses(chunk)
                .await
                .context("get_signature_statuses")?
                .value;
            out.extend(statuses.into_iter().map(|status| {
                // A `processed`-only status is not an outcome yet: the fork
                // carrying it can still be abandoned, and the caller acts on
                // these — booking payment, or ramping a contention delay off
                // a revert. Leave it pending and let it settle. If the fork
                // does get dropped it never confirms, the blockhash expires,
                // and it comes back as a retryable `Expired` rather than a
                // landed crank that never happened.
                status
                    .filter(|status| {
                        matches!(
                            status.confirmation_status,
                            Some(TransactionConfirmationStatus::Confirmed)
                                | Some(TransactionConfirmationStatus::Finalized)
                        )
                    })
                    .map(|status| match status.err {
                        Some(err) => SignatureOutcome::Failed(err.to_string()),
                        None => SignatureOutcome::Landed,
                    })
            }));
        }
        Ok(out)
    }

    async fn simulate_transaction(
        &self,
        tx: &Transaction,
        return_accounts: &[Pubkey],
    ) -> Result<SimOutcome> {
        let accounts_config =
            (!return_accounts.is_empty()).then(|| RpcSimulateTransactionAccountsConfig {
                encoding: Some(UiAccountEncoding::Base64),
                addresses: return_accounts.iter().map(|pk| pk.to_string()).collect(),
            });
        let response = self
            .client
            .simulate_transaction_with_config(
                tx,
                RpcSimulateTransactionConfig {
                    sig_verify: false,
                    replace_recent_blockhash: true,
                    commitment: Some(CommitmentConfig::processed()),
                    accounts: accounts_config,
                    ..Default::default()
                },
            )
            .await
            .context("simulate_transaction")?;
        let value = response.value;
        let return_data = value
            .return_data
            .map(|rd| {
                base64::engine::general_purpose::STANDARD
                    .decode(rd.data.0)
                    .map_err(|e| anyhow!("bad return data base64: {e}"))
            })
            .transpose()?;
        Ok(SimOutcome {
            err: value.err.map(|e| e.to_string()),
            logs: value.logs.unwrap_or_default(),
            return_data,
            units_consumed: value.units_consumed.unwrap_or_default(),
            accounts: value
                .accounts
                .unwrap_or_default()
                .into_iter()
                .map(|maybe| maybe.and_then(decode_ui_account))
                .collect(),
        })
    }

    async fn recent_priority_fee(&self, accounts: &[Pubkey]) -> Result<u64> {
        let fees = self
            .client
            .get_recent_prioritization_fees(accounts)
            .await
            .context("get_recent_prioritization_fees")?;
        // Median over the recent window: the max is one outlier slot, the
        // mean chases it.
        let mut recent: Vec<u64> = fees
            .iter()
            .rev()
            .take(20)
            .map(|fee| fee.prioritization_fee)
            .collect();
        if recent.is_empty() {
            return Ok(0);
        }
        recent.sort_unstable();
        Ok(recent[recent.len() / 2])
    }

    async fn send_transaction(&self, tx: &Transaction) -> Result<Signature> {
        // Preflight is redundant with our own simulation and would run at a
        // stricter commitment than the state the decision was made on.
        self.client
            .send_transaction_with_config(
                tx,
                RpcSendTransactionConfig {
                    skip_preflight: true,
                    ..Default::default()
                },
            )
            .await
            .context("send_transaction")
    }
}
