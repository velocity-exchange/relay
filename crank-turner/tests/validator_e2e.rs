//! End-to-end against a real validator.
//!
//! Everything below the turner is real here: a `solana-test-validator`
//! with both programs deployed, transactions that actually land, a real
//! RPC endpoint, and a turner running its normal loop against it. What the
//! litesvm suites cannot cover is exactly what this catches — RPC
//! encodings, commitment lag, blockhash expiry, confirmation timing, and
//! whether local simulation against cached accounts agrees with a chain
//! that is genuinely moving underneath it.
//!
//! Ignored by default because it needs the validator binary and takes
//! tens of seconds. Run it with:
//!
//! ```text
//! ./scripts/e2e.sh
//! ```

#![cfg(not(target_os = "windows"))]

use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use relay_crank_turner::{
    ChainSource, LocalSimConfig, LocalSimSource, Outcome, RpcSource, Turner, TurnerConfig,
    WatchFilter,
};
use relay_spec as spec;
use sha2::{Digest, Sha256};
use solana_client::nonblocking::rpc_client::RpcClient;
use solana_commitment_config::CommitmentConfig;
use solana_sdk::instruction::{AccountMeta, Instruction};
use solana_sdk::pubkey::Pubkey;
use solana_sdk::signature::{Keypair, Signer};
use solana_sdk::transaction::Transaction;

const RPC_PORT: u16 = 8999;
const FAUCET_PORT: u16 = 9900;

// Layout mirrors, hand-pinned like the other turner tests so this suite
// doubles as an ABI check on demo-book.
const BOOK_ACCOUNT_LEN: usize = 2792;
const CONDITIONS_OFFSET: u32 = 912;
const ENTRY_COUNT_OFFSET: usize = 72;
const LIVE_OFFSET: usize = 848;
const SIDES_OFFSET: usize = 880;
const MAX_ENTRIES: usize = 32;
const SIDE_BID: u8 = 0;
const SIDE_ASK: u8 = 1;
const PAYMENT: u64 = 100_000;
const WATCH_V0_LEN: usize = spec::WATCH_V0_LEN;

fn relay_id() -> Pubkey {
    "4D5tPhw9sqkdkR5CpmP427TH6y9p9AMuKUukUEHn3Mpu"
        .parse()
        .unwrap()
}

fn demo_id() -> Pubkey {
    "6PqZZeykcFwncPxs5LjjxzQshdRV29mpsFtmT3QS9jRZ"
        .parse()
        .unwrap()
}

fn disc(name: &str) -> Vec<u8> {
    Sha256::digest(format!("global:{name}").as_bytes())[..8].to_vec()
}

fn so_path(name: &str) -> String {
    format!(
        "{}/../programs/target/deploy/{name}.so",
        env!("CARGO_MANIFEST_DIR")
    )
}

/// The validator, killed on drop so a failing assertion never leaks one.
struct Validator {
    child: Child,
    ledger: String,
}

impl Drop for Validator {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_dir_all(&self.ledger);
    }
}

impl Validator {
    async fn start() -> Self {
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_millis();
        let ledger = std::env::temp_dir()
            .join(format!("relay-e2e-{stamp}"))
            .to_string_lossy()
            .into_owned();
        let child = Command::new("solana-test-validator")
            .args([
                "--reset",
                "--quiet",
                "--ledger",
                &ledger,
                "--rpc-port",
                &RPC_PORT.to_string(),
                "--faucet-port",
                &FAUCET_PORT.to_string(),
                "--bpf-program",
                &relay_id().to_string(),
                &so_path("relay"),
                "--bpf-program",
                &demo_id().to_string(),
                &so_path("demo_book"),
            ])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect(
                "solana-test-validator must be on PATH; run scripts/e2e.sh, and build the \
                 programs first with scripts/build-programs.sh",
            );
        let validator = Self { child, ledger };

        // Wait for it to serve requests.
        let rpc = RpcClient::new_with_commitment(validator.url(), CommitmentConfig::confirmed());
        let deadline = Instant::now() + Duration::from_secs(60);
        loop {
            assert!(Instant::now() < deadline, "validator did not come up");
            if rpc.get_latest_blockhash().await.is_ok() && rpc.get_slot().await.unwrap_or(0) > 1 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(400)).await;
        }
        validator
    }

    fn url(&self) -> String {
        format!("http://127.0.0.1:{RPC_PORT}")
    }
}

/// Test-side client: builds and lands transactions the way an operator's
/// tooling would.
struct Client {
    rpc: RpcClient,
    payer: Keypair,
}

impl Client {
    async fn new(url: String) -> Self {
        let rpc = RpcClient::new_with_commitment(url, CommitmentConfig::confirmed());
        let payer = Keypair::new();
        let client = Self { rpc, payer };
        client.fund(&client.payer.pubkey(), 500_000_000_000).await;
        client
    }

    async fn fund(&self, to: &Pubkey, lamports: u64) {
        let signature = self.rpc.request_airdrop(to, lamports).await.unwrap();
        let deadline = Instant::now() + Duration::from_secs(30);
        while Instant::now() < deadline {
            if self
                .rpc
                .confirm_transaction(&signature)
                .await
                .unwrap_or(false)
            {
                return;
            }
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
        panic!("airdrop did not confirm");
    }

    async fn send(&self, ixs: &[Instruction], extra: &[&Keypair]) {
        let blockhash = self.rpc.get_latest_blockhash().await.unwrap();
        let mut signers: Vec<&Keypair> = vec![&self.payer];
        signers.extend(extra);
        let tx = Transaction::new_signed_with_payer(
            ixs,
            Some(&self.payer.pubkey()),
            &signers,
            blockhash,
        );
        self.rpc
            .send_and_confirm_transaction(&tx)
            .await
            .unwrap_or_else(|err| panic!("transaction failed: {err}"));
    }

    async fn account(&self, pubkey: &Pubkey) -> Vec<u8> {
        self.rpc.get_account(pubkey).await.unwrap().data
    }

    async fn lamports(&self, pubkey: &Pubkey) -> u64 {
        self.rpc.get_balance(pubkey).await.unwrap()
    }

    /// Live entries on one side, read straight from the account.
    async fn side_count(&self, book: &Pubkey, side: u8) -> usize {
        let data = self.account(book).await;
        (0..MAX_ENTRIES)
            .filter(|&i| data[LIVE_OFFSET + i] == 1 && data[SIDES_OFFSET + i] == side)
            .count()
    }

    async fn entry_count(&self, book: &Pubkey) -> u32 {
        let data = self.account(book).await;
        u32::from_le_bytes(
            data[ENTRY_COUNT_OFFSET..ENTRY_COUNT_OFFSET + 4]
                .try_into()
                .unwrap(),
        )
    }

    async fn unix_timestamp(&self) -> i64 {
        self.rpc
            .get_block_time(self.rpc.get_slot().await.unwrap())
            .await
            .unwrap()
    }

    /// Create the book account, initialize it, and register its watch.
    async fn create_book(&self, evict_threshold: u32) -> Pubkey {
        let book = Keypair::new();
        let rent = self
            .rpc
            .get_minimum_balance_for_rent_exemption(BOOK_ACCOUNT_LEN)
            .await
            .unwrap();
        // Extra lamports beyond rent are the crank-payment treasury.
        let create = solana_system_interface::instruction::create_account(
            &self.payer.pubkey(),
            &book.pubkey(),
            rent + 200_000_000,
            BOOK_ACCOUNT_LEN as u64,
            &demo_id(),
        );
        let init = Instruction {
            program_id: demo_id(),
            accounts: vec![
                AccountMeta::new_readonly(self.payer.pubkey(), true),
                AccountMeta::new(book.pubkey(), false),
            ],
            data: disc("initialize_book_v0")
                .into_iter()
                .chain(PAYMENT.to_le_bytes())
                .chain(evict_threshold.to_le_bytes())
                .collect(),
        };
        self.send(&[create, init], &[&book]).await;

        // Register the watch so the turner discovers the book.
        let watch = Keypair::new();
        let watch_rent = self
            .rpc
            .get_minimum_balance_for_rent_exemption(WATCH_V0_LEN)
            .await
            .unwrap();
        let create_watch = solana_system_interface::instruction::create_account(
            &self.payer.pubkey(),
            &watch.pubkey(),
            watch_rent,
            WATCH_V0_LEN as u64,
            &relay_id(),
        );
        let register = Instruction {
            program_id: relay_id(),
            accounts: vec![
                AccountMeta::new_readonly(self.payer.pubkey(), true),
                AccountMeta::new_readonly(book.pubkey(), false),
                AccountMeta::new(watch.pubkey(), false),
            ],
            data: disc("register_watch_v0")
                .into_iter()
                .chain(CONDITIONS_OFFSET.to_le_bytes())
                .collect(),
        };
        self.send(&[create_watch, register], &[&watch]).await;
        book.pubkey()
    }

    /// The order-posting bot: one quote per call.
    async fn post_quote(&self, book: &Pubkey, expiry_ts: i64, price: u64, side: u8) {
        let ix = Instruction {
            program_id: demo_id(),
            accounts: vec![
                AccountMeta::new(*book, false),
                AccountMeta::new_readonly(self.payer.pubkey(), true),
            ],
            data: disc("add_entry_v0")
                .into_iter()
                .chain(expiry_ts.to_le_bytes())
                .chain(price.to_le_bytes())
                .chain([side])
                .collect(),
        };
        self.send(&[ix], &[]).await;
    }
}

/// Build the turner exactly as `main.rs` does for the RPC transport, with
/// local simulation on.
fn build_turner(url: String, keeper: Keypair) -> Turner<Arc<dyn ChainSource>> {
    let source: Arc<dyn ChainSource> = Arc::new(LocalSimSource::new(
        RpcSource::new(url),
        LocalSimConfig { pool_size: 4 },
    ));
    Turner::new(
        source,
        keeper,
        TurnerConfig {
            filter: WatchFilter::for_programs([demo_id()]),
            // The operator wrote demo-book, so run it the way they would:
            // trusted, meaning no guard instructions and payment straight
            // to the fee payer.
            trusted_programs: [demo_id()].into_iter().collect(),
            // The validator's clock moves in real time, so let wakes
            // re-fire promptly rather than sitting in backoff.
            no_work_backoff_slots: 1,
            ..TurnerConfig::default()
        },
    )
}

/// Run ticks until `done` reports true, or fail with what the turner saw.
async fn turn_until<F>(
    turner: &mut Turner<Arc<dyn ChainSource>>,
    timeout: Duration,
    mut done: F,
) -> Vec<Outcome>
where
    F: AsyncFnMut() -> bool,
{
    let deadline = Instant::now() + timeout;
    let mut seen = Vec::new();
    while Instant::now() < deadline {
        turner.refresh_watches().await.expect("refresh");
        match turner.tick().await {
            Ok(outcomes) => seen.extend(outcomes),
            Err(err) => panic!("tick failed: {err:#}"),
        }
        if done().await {
            return seen;
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    panic!("condition never became true; turner saw: {seen:?}");
}

/// The full loop against a real chain: a bot posts orders, and the turner
/// expires them, evicts at the soft cap, and matches crosses — with no
/// test-side knowledge of any of those cranks beyond what the on-chain
/// conditions declare.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs solana-test-validator; run scripts/e2e.sh"]
async fn crank_turner_drives_a_real_book() {
    let validator = Validator::start().await;
    let client = Client::new(validator.url()).await;
    let keeper = Keypair::new();
    client.fund(&keeper.pubkey(), 10_000_000_000).await;
    let keeper_pubkey = keeper.pubkey();
    let mut turner = build_turner(validator.url(), keeper);

    // A soft cap of 6 so eviction is reachable without filling the book.
    let book = client.create_book(6).await;
    assert_eq!(
        turner.refresh_watches().await.unwrap().admitted,
        1,
        "the turner should discover the registered book"
    );

    // --- expiry ---------------------------------------------------------
    // The bot posts quotes that expire almost immediately, plus one that
    // outlives the test.
    let now = client.unix_timestamp().await;
    client.post_quote(&book, now + 2, 100, SIDE_BID).await;
    client.post_quote(&book, now + 2, 101, SIDE_BID).await;
    client.post_quote(&book, now + 3600, 90, SIDE_BID).await;
    assert_eq!(client.entry_count(&book).await, 3);

    let outcomes = turn_until(&mut turner, Duration::from_secs(60), async || {
        client.entry_count(&book).await == 1
    })
    .await;
    assert!(
        outcomes.iter().any(|o| matches!(o, Outcome::Sent { .. })),
        "the sweep should have been cranked: {outcomes:?}"
    );
    assert!(
        client.rpc.get_balance(&keeper_pubkey).await.unwrap() > 0,
        "keeper still funded"
    );

    // --- eviction -------------------------------------------------------
    // Push the book to its soft cap with long-lived quotes; the evict
    // condition's change-wake fires on entry_count and trims it back.
    let far = now + 3600;
    for price in 0..5u64 {
        client.post_quote(&book, far, 80 + price, SIDE_BID).await;
    }
    assert!(client.entry_count(&book).await >= 6);
    turn_until(&mut turner, Duration::from_secs(60), async || {
        client.entry_count(&book).await < 6
    })
    .await;

    // --- cross ----------------------------------------------------------
    // On its own book, with the soft cap out of reach and nothing
    // expiring, so only a cross can change the count. The turner has no
    // idea this book exists until it re-scans the registry.
    let crossing = client.create_book(100).await;
    let treasury_before = client.lamports(&crossing).await;
    client.post_quote(&crossing, far, 100, SIDE_BID).await;
    client.post_quote(&crossing, far, 101, SIDE_BID).await; // best bid
    client.post_quote(&crossing, far, 200, SIDE_ASK).await; // does not cross
    assert_eq!(client.entry_count(&crossing).await, 3);
    // Guard the hand-pinned layout mirrors: if LIVE_OFFSET/SIDES_OFFSET
    // had drifted, the per-side counts would not add up to the total and
    // the assertions below would be meaningless.
    assert_eq!(
        client.side_count(&crossing, SIDE_BID).await + client.side_count(&crossing, SIDE_ASK).await,
        3,
        "layout mirrors are stale — re-read demo-book's state.rs offsets"
    );

    // This ask crosses the resting bid at 101.
    client.post_quote(&crossing, far, 50, SIDE_ASK).await;
    turn_until(&mut turner, Duration::from_secs(60), async || {
        client.entry_count(&crossing).await == 2
    })
    .await;

    // Exactly the crossed pair went: the 101 bid and the 50 ask, leaving
    // one bid and the uncrossed 200 ask.
    assert_eq!(client.side_count(&crossing, SIDE_BID).await, 1);
    assert_eq!(client.side_count(&crossing, SIDE_ASK).await, 1);

    // The book paid for it, on chain. Rent is untouched, so the drop is
    // crank payments (one cross here, at minimum).
    let treasury_after = client.lamports(&crossing).await;
    assert!(
        treasury_before - treasury_after >= PAYMENT,
        "book treasury should have paid a crank: {treasury_before} -> {treasury_after}"
    );
    assert!(
        client.lamports(&keeper_pubkey).await > 0,
        "keeper still solvent after paying fees"
    );
    drop(validator);
}
