//! Chain access behind a trait so transports are pluggable. The production
//! implementation is RPC polling; a geyser/websocket source implements the
//! same trait with a local cache; tests use a litesvm-backed source.

use anyhow::{anyhow, Context, Result};
use async_trait::async_trait;
use solana_client::nonblocking::rpc_client::RpcClient;
use solana_client::rpc_config::{
    RpcAccountInfoConfig, RpcProgramAccountsConfig, RpcSimulateTransactionConfig,
};
use solana_client::rpc_filter::{Memcmp, RpcFilterType};
use solana_commitment_config::CommitmentConfig;
use solana_sdk::account::Account;
use solana_sdk::clock::Clock;
use solana_sdk::hash::Hash;
use solana_sdk::pubkey::Pubkey;
use solana_sdk::signature::Signature;
use solana_sdk::sysvar;
use solana_sdk::transaction::Transaction;

/// Chain time, as the scheduler's single notion of "now" — always the
/// on-chain clock, never wall time, so wakes can't fire early relative to
/// what the executor's `Clock::get()` will see.
#[derive(Debug, Clone, Copy)]
pub struct ClockSnapshot {
    pub slot: u64,
    pub unix_timestamp: i64,
}

/// Simulation result, reduced to what the turner decides on.
#[derive(Debug, Clone)]
pub struct SimOutcome {
    /// None = simulation succeeded.
    pub err: Option<String>,
    pub logs: Vec<String>,
    pub return_data: Option<Vec<u8>>,
}

#[async_trait]
pub trait ChainSource: Send + Sync {
    async fn get_multiple_accounts(&self, pubkeys: &[Pubkey]) -> Result<Vec<Option<Account>>>;

    /// relay watch accounts (pre-filtered to plausible `WatchV0`s by
    /// size/discriminator where the transport allows).
    async fn get_watch_accounts(&self, program: &Pubkey) -> Result<Vec<(Pubkey, Account)>>;

    async fn clock(&self) -> Result<ClockSnapshot>;

    async fn latest_blockhash(&self) -> Result<Hash>;

    async fn simulate_transaction(&self, tx: &Transaction) -> Result<SimOutcome>;

    async fn send_transaction(&self, tx: &Transaction) -> Result<Signature>;
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

#[async_trait]
impl ChainSource for RpcSource {
    async fn get_multiple_accounts(&self, pubkeys: &[Pubkey]) -> Result<Vec<Option<Account>>> {
        self.client
            .get_multiple_accounts(pubkeys)
            .await
            .context("get_multiple_accounts")
    }

    async fn get_watch_accounts(&self, program: &Pubkey) -> Result<Vec<(Pubkey, Account)>> {
        // Deprecated in favor of the ui-accounts variant, but this one
        // returns `Account` directly, which is what the trait wants.
        #[allow(deprecated)]
        self.client
            .get_program_accounts_with_config(
                program,
                RpcProgramAccountsConfig {
                    filters: Some(vec![
                        RpcFilterType::DataSize(relay_spec::WATCH_V0_LEN as u64),
                        RpcFilterType::Memcmp(Memcmp::new_raw_bytes(
                            0,
                            relay_spec::WATCH_V0_DISCRIMINATOR.to_vec(),
                        )),
                    ]),
                    account_config: RpcAccountInfoConfig {
                        encoding: Some(solana_client::rpc_config::UiAccountEncoding::Base64),
                        ..Default::default()
                    },
                    ..Default::default()
                },
            )
            .await
            .context("get_program_accounts(watches)")
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

    async fn latest_blockhash(&self) -> Result<Hash> {
        self.client
            .get_latest_blockhash()
            .await
            .context("get_latest_blockhash")
    }

    async fn simulate_transaction(&self, tx: &Transaction) -> Result<SimOutcome> {
        let response = self
            .client
            .simulate_transaction_with_config(
                tx,
                RpcSimulateTransactionConfig {
                    sig_verify: false,
                    replace_recent_blockhash: true,
                    commitment: Some(CommitmentConfig::processed()),
                    ..Default::default()
                },
            )
            .await
            .context("simulate_transaction")?;
        let value = response.value;
        let return_data = value
            .return_data
            .map(|rd| {
                use base64::Engine as _;
                base64::engine::general_purpose::STANDARD
                    .decode(rd.data.0)
                    .map_err(|e| anyhow!("bad return data base64: {e}"))
            })
            .transpose()?;
        Ok(SimOutcome {
            err: value.err.map(|e| e.to_string()),
            logs: value.logs.unwrap_or_default(),
            return_data,
        })
    }

    async fn send_transaction(&self, tx: &Transaction) -> Result<Signature> {
        // Preflight is redundant with our own simulation and would run at a
        // stricter commitment than the state the decision was made on.
        self.client
            .send_transaction_with_config(
                tx,
                solana_client::rpc_config::RpcSendTransactionConfig {
                    skip_preflight: true,
                    ..Default::default()
                },
            )
            .await
            .context("send_transaction")
    }
}
