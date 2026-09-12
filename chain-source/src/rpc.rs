//! The polling reference implementation of [`ChainSource`].
//!
//! Reads at `processed` commitment — fast, possibly provisional state is
//! fine because every action is simulated before it is sent, and sent
//! transactions are re-validated on chain. The blockhash and signature
//! outcomes are the exception and read at `confirmed`: a `processed`
//! blockhash can be abandoned, and a `processed` signature status is not yet
//! an outcome.

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
use solana_rpc_client_api::filter::RpcFilterType;
use solana_sdk::account::Account;
use solana_sdk::clock::Clock;
use solana_sdk::pubkey::Pubkey;
use solana_sdk::signature::Signature;
use solana_sdk::sysvar;
use solana_sdk::transaction::Transaction;
use solana_transaction_status_client_types::TransactionConfirmationStatus;

use crate::metrics;
use crate::source::{
    AccountFilter, BlockhashInfo, ChainSource, ClockSnapshot, SignatureOutcome, SimOutcome,
};

/// RPC-polling source. Reads at `processed` commitment — fast, possibly
/// provisional state is fine because every action is simulated before it is
/// sent, and sent transactions are re-validated on chain.
/// RPC's per-call ceiling for `getMultipleAccounts`.
const MAX_ACCOUNTS_PER_CALL: usize = 100;
/// How many of those chunk calls to have in flight at once.
const MAX_CONCURRENT_ACCOUNT_CALLS: usize = 8;

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

/// Time one RPC call and record it under `method`.
///
/// Wrapping rather than instrumenting each body inline keeps the label
/// spelling in one place: a metric whose method names drift between call
/// sites is worse than no metric, because a dashboard silently loses a
/// series instead of failing.
async fn timed<T>(
    method: &'static str,
    accounts: usize,
    call: impl std::future::Future<Output = Result<T>>,
) -> Result<T> {
    let started = std::time::Instant::now();
    let result = call.await;
    metrics::RPC_SECONDS
        .with_label_values(&[method])
        .observe(started.elapsed().as_secs_f64());
    if accounts > 0 {
        metrics::RPC_ACCOUNTS
            .with_label_values(&[method])
            .inc_by(accounts as u64);
    }
    if result.is_err() {
        metrics::RPC_ERRORS.with_label_values(&[method]).inc();
    }
    result
}

#[async_trait]
impl ChainSource for RpcSource {
    async fn get_multiple_accounts(&self, pubkeys: &[Pubkey]) -> Result<Vec<Option<Account>>> {
        timed("get_multiple_accounts", pubkeys.len(), async {
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
        })
        .await
    }

    async fn get_program_accounts(
        &self,
        program: &Pubkey,
        filter_sets: &[Vec<AccountFilter>],
    ) -> Result<Vec<(Pubkey, Account)>> {
        timed("get_program_accounts", 0, async {
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
            metrics::RPC_ACCOUNTS
                .with_label_values(&["get_program_accounts"])
                .inc_by(out.len() as u64);
            Ok(out)
        })
        .await
    }

    async fn clock(&self) -> Result<ClockSnapshot> {
        timed("clock", 1, async {
            let account = self
                .client
                .get_account(&sysvar::clock::id())
                .await
                .context("get clock sysvar")?;
            let clock: Clock =
                bincode::deserialize(&account.data).context("decode clock sysvar")?;
            Ok(ClockSnapshot {
                slot: clock.slot,
                unix_timestamp: clock.unix_timestamp,
            })
        })
        .await
    }

    async fn latest_blockhash(&self) -> Result<BlockhashInfo> {
        timed("latest_blockhash", 0, async {
            let (hash, last_valid_block_height) = self
                .client
                .get_latest_blockhash_with_commitment(CommitmentConfig::confirmed())
                .await
                .context("get_latest_blockhash")?;
            Ok(BlockhashInfo {
                hash,
                last_valid_block_height,
            })
        })
        .await
    }

    async fn block_height(&self) -> Result<u64> {
        self.client.get_block_height().await.context("block_height")
    }

    async fn signature_statuses(
        &self,
        signatures: &[Signature],
    ) -> Result<Vec<Option<SignatureOutcome>>> {
        timed("signature_statuses", signatures.len(), async {
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
        })
        .await
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
