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
use solana_sdk::signature::{Keypair, Signature, Signer};
use solana_sdk::signer::EncodableKey;
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
const TOKEN_ACCOUNT_LEN: usize = 165;
/// SPL token account layout: mint(32) owner(32) amount(8) ...
const TOKEN_AMOUNT_OFFSET: usize = 64;

fn token_program() -> Pubkey {
    "TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA"
        .parse()
        .unwrap()
}

fn native_mint() -> Pubkey {
    "So11111111111111111111111111111111111111112"
        .parse()
        .unwrap()
}
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

    /// Put one already-expired quote on each of `books`, batched.
    async fn post_expired_quotes(&self, books: &[Pubkey]) {
        // Twelve per transaction: each contributes a 32-byte book key plus
        // its instruction, and twenty overflows the packet limit.
        const PER_TX: usize = 12;
        let now = self.unix_timestamp().await;
        for chunk in books.chunks(PER_TX) {
            let ixs: Vec<Instruction> = chunk
                .iter()
                .map(|book| Instruction {
                    program_id: demo_id(),
                    accounts: vec![
                        AccountMeta::new(*book, false),
                        AccountMeta::new_readonly(self.payer.pubkey(), true),
                    ],
                    data: disc("add_entry_v0")
                        .into_iter()
                        .chain((now - 1).to_le_bytes())
                        .chain(100u64.to_le_bytes())
                        .chain([SIDE_BID])
                        .collect(),
                })
                .collect();
            self.send(&ixs, &[]).await;
        }
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

    /// Create a wrapped-SOL account owned by `authority`.
    ///
    /// This is the payout shape the turner wants for untrusted programs:
    /// the SPL Token program owns the account, so only it can debit the
    /// lamports, and only for operations that need `authority` to sign —
    /// which a hostile executor never gets, because the authority is not
    /// in its account list. Anyone may credit it, and `sync_native` (no
    /// signer) turns those lamports into token balance.
    async fn create_wsol(&self, authority: &Pubkey) -> Pubkey {
        let wsol = Keypair::new();
        let rent = self
            .rpc
            .get_minimum_balance_for_rent_exemption(TOKEN_ACCOUNT_LEN)
            .await
            .unwrap();
        let create = solana_system_interface::instruction::create_account(
            &self.payer.pubkey(),
            &wsol.pubkey(),
            rent,
            TOKEN_ACCOUNT_LEN as u64,
            &token_program(),
        );
        // InitializeAccount3 { owner }: tag 18, no rent sysvar, no signer.
        let init = Instruction {
            program_id: token_program(),
            accounts: vec![
                AccountMeta::new(wsol.pubkey(), false),
                AccountMeta::new_readonly(native_mint(), false),
            ],
            data: [18u8].into_iter().chain(authority.to_bytes()).collect(),
        };
        self.send(&[create, init], &[&wsol]).await;
        wsol.pubkey()
    }

    /// The SPL token `amount` field, which only tracks lamports once
    /// `sync_native` has run.
    async fn token_amount(&self, account: &Pubkey) -> u64 {
        let data = self.account(account).await;
        u64::from_le_bytes(
            data[TOKEN_AMOUNT_OFFSET..TOKEN_AMOUNT_OFFSET + 8]
                .try_into()
                .unwrap(),
        )
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
        LocalSimConfig {
            pool_size: 4,
            ..LocalSimConfig::default()
        },
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
///
/// On timeout this looks up what actually happened to the transactions the
/// turner sent. Without a submitter attached, sends are fire-and-forget,
/// so `Sent` means "handed to the RPC", not "landed" — and a crank that
/// fails on chain otherwise shows up as an infinite stream of successful
/// sends with nothing changing.
async fn turn_until<F>(
    turner: &mut Turner<Arc<dyn ChainSource>>,
    client: &Client,
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

    let signatures: Vec<Signature> = seen
        .iter()
        .filter_map(|o| match o {
            Outcome::Sent { signature, .. } => Some(*signature),
            _ => None,
        })
        .collect();
    let mut on_chain = Vec::new();
    for signature in signatures.iter().rev().take(3) {
        let status = client
            .rpc
            .get_signature_statuses(&[*signature])
            .await
            .ok()
            .and_then(|r| r.value.into_iter().next().flatten());
        on_chain.push(format!("{signature}: {status:?}"));
    }
    let failures: Vec<&Outcome> = seen
        .iter()
        .filter(|o| matches!(o, Outcome::Failed { .. }))
        .collect();
    panic!(
        "condition never became true.\n  sent: {}\n  last on-chain: {on_chain:#?}\n  \
         turner failures: {failures:#?}",
        signatures.len()
    );
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

    let outcomes = turn_until(&mut turner, &client, Duration::from_secs(60), async || {
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
    turn_until(&mut turner, &client, Duration::from_secs(60), async || {
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
    turn_until(&mut turner, &client, Duration::from_secs(60), async || {
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

/// The wrapped-SOL payout, end to end against the real SPL Token program.
///
/// demo-book is run **untrusted** here, which is the whole point: the
/// turner guards the crank, pays a wSOL account it controls, and never
/// hands the executor its signing key. Payment lands as lamports (which
/// the guard measures), and the appended `sync_native` — an instruction
/// with no signer — turns them into spendable token balance.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs solana-test-validator; run scripts/e2e.sh"]
async fn wrapped_sol_payout_is_paid_and_synced() {
    let validator = Validator::start().await;
    let client = Client::new(validator.url()).await;
    let keeper = Keypair::new();
    client.fund(&keeper.pubkey(), 10_000_000_000).await;
    let keeper_pubkey = keeper.pubkey();

    // The payout: wSOL owned by the keeper, which never signs the cranks.
    let payout = client.create_wsol(&keeper_pubkey).await;
    let payout_rent = client.lamports(&payout).await;
    assert_eq!(client.token_amount(&payout).await, 0);

    let source: Arc<dyn ChainSource> = Arc::new(LocalSimSource::new(
        RpcSource::new(validator.url()),
        LocalSimConfig {
            pool_size: 4,
            ..LocalSimConfig::default()
        },
    ));
    let mut turner = Turner::new(
        source,
        keeper,
        TurnerConfig {
            filter: WatchFilter::for_programs([demo_id()]),
            // Untrusted: guards on, and the fee payer must stay out of the
            // executor's account list entirely.
            payout: Some(payout),
            sync_native_payout: true,
            no_work_backoff_slots: 1,
            ..TurnerConfig::default()
        },
    );

    let book = client.create_book(100).await;
    assert_eq!(turner.refresh_watches().await.unwrap().admitted, 1);

    let payer_before = client.lamports(&keeper_pubkey).await;
    let now = client.unix_timestamp().await;
    client.post_quote(&book, now + 2, 100, SIDE_BID).await;
    turn_until(&mut turner, &client, Duration::from_secs(60), async || {
        client.entry_count(&book).await == 0
    })
    .await;

    // Paid in lamports, which is what the guard measures — and the token
    // `amount` has NOT moved, because cranks deliberately do not carry a
    // sync_native instruction.
    let paid = client.lamports(&payout).await - payout_rent;
    assert!(paid >= PAYMENT, "payout received {paid} lamports");
    assert_eq!(
        client.token_amount(&payout).await,
        0,
        "cranks should not pay for a sync every time"
    );

    // The turner rolls it into spendable token balance later, on its own
    // schedule, in one standalone transaction.
    let signature = turner
        .sync_payout()
        .await
        .expect("sync submitted")
        .expect("sync enabled");
    let deadline = Instant::now() + Duration::from_secs(30);
    while client.token_amount(&payout).await == 0 && Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(300)).await;
    }
    assert_eq!(
        client.token_amount(&payout).await,
        paid,
        "sync_native should mirror lamports above the rent reserve ({signature})"
    );

    // The fee payer only ever spent fees — it was never handed to the
    // executor, so it could not be rugged.
    let payer_after = client.lamports(&keeper_pubkey).await;
    assert!(
        payer_before - payer_after < 10_000_000,
        "fee payer should only have paid fees: {payer_before} -> {payer_after}"
    );
    drop(validator);
}

// --- the shipped binary, running as a daemon ---

/// The real `relay-crank-turner` process, killed on drop.
struct Daemon {
    child: Child,
    log: String,
}

impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Daemon {
    /// Spawn the binary the way an operator would: websocket transport,
    /// its own timer, its own submitter, its own metrics server.
    fn start(rpc_url: &str, keypair_path: &str, metrics_port: u16, log: String) -> Self {
        Self::start_with_ws(
            rpc_url,
            &format!("ws://127.0.0.1:{}", RPC_PORT + 1),
            keypair_path,
            metrics_port,
            log,
        )
    }

    fn start_with_ws(
        rpc_url: &str,
        ws_url: &str,
        keypair_path: &str,
        metrics_port: u16,
        log: String,
    ) -> Self {
        let out = std::fs::File::create(&log).expect("log file");
        let err = out.try_clone().expect("log file");
        let child = Command::new(env!("CARGO_BIN_EXE_relay-crank-turner"))
            .args([
                "--rpc-url",
                rpc_url,
                "--ws-url",
                ws_url,
                "--transport",
                "ws",
                "--keypair",
                keypair_path,
                "--program-id",
                &relay_id().to_string(),
                "--target-program",
                &demo_id().to_string(),
                "--trusted-program",
                &demo_id().to_string(),
                // Everything owned by demo-book is streamed, so local
                // simulation runs off the subscription.
                "--watch-program",
                &demo_id().to_string(),
                "--tick-ms",
                "300",
                "--refresh-ticks",
                "3",
                "--metrics-port",
                &metrics_port.to_string(),
            ])
            .env("RUST_LOG", "info")
            .stdout(Stdio::from(out))
            .stderr(Stdio::from(err))
            .spawn()
            .expect("crank turner binary");
        Self { child, log }
    }

    fn logs(&self) -> String {
        std::fs::read_to_string(&self.log).unwrap_or_default()
    }
}

/// Scrape the daemon's own metrics endpoint.
async fn scrape(port: u16) -> Option<String> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let mut socket = tokio::net::TcpStream::connect(("127.0.0.1", port))
        .await
        .ok()?;
    socket
        .write_all(b"GET /metrics HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
        .await
        .ok()?;
    let mut body = String::new();
    socket.read_to_string(&mut body).await.ok()?;
    Some(body)
}

/// The turner as it actually ships: a separate process, subscribed over
/// websocket, running its own loop.
///
/// The other scenarios drive `tick()` by hand over RPC, which exercises
/// the library but bypasses everything the binary adds — the timer, the
/// subscription feed and its cache, the submitter, the metrics server, and
/// CLI wiring. Here the test only creates a book and posts orders; if the
/// cranks happen, the shipped daemon works.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs solana-test-validator; run scripts/e2e.sh"]
async fn shipped_daemon_cranks_over_websocket() {
    let validator = Validator::start().await;
    let client = Client::new(validator.url()).await;

    // The keeper the daemon will load from disk.
    let keeper = Keypair::new();
    client.fund(&keeper.pubkey(), 10_000_000_000).await;
    let dir = std::env::temp_dir().join(format!("relay-daemon-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let keypair_path = dir.join("keeper.json").to_string_lossy().into_owned();
    keeper
        .write_to_file(&keypair_path)
        .expect("write keeper keypair");

    // Book and watch exist before the daemon starts, so its first
    // registry scan finds them.
    let book = client.create_book(6).await;
    let metrics_port = 9977;
    let daemon = Daemon::start(
        &validator.url(),
        &keypair_path,
        metrics_port,
        dir.join("turner.log").to_string_lossy().into_owned(),
    );

    // From here the test only acts as the order-posting bot. Nothing
    // drives the turner.
    let now = client.unix_timestamp().await;
    let far = now + 3600;
    client.post_quote(&book, now + 2, 100, SIDE_BID).await;
    client.post_quote(&book, now + 2, 101, SIDE_BID).await;
    client.post_quote(&book, far, 90, SIDE_BID).await;

    let expired_swept = wait_for(Duration::from_secs(90), || async {
        client.entry_count(&book).await == 1
    })
    .await;
    assert!(
        expired_swept,
        "daemon never swept the expired quotes.\nlogs:\n{}",
        daemon.logs()
    );

    // A crossing ask: proves the change-wake reached it over the
    // subscription, not just the timestamp wake.
    client.post_quote(&book, far, 1, SIDE_ASK).await;
    let crossed = wait_for(Duration::from_secs(90), || async {
        client.entry_count(&book).await == 0
    })
    .await;
    assert!(
        crossed,
        "daemon never matched the cross.\nlogs:\n{}",
        daemon.logs()
    );

    // Its own metrics endpoint confirms the daemon, not the test, did it.
    // It really used the websocket path, rather than quietly falling back.
    let logs = daemon.logs();
    assert!(
        logs.contains("websocket subscriptions enabled"),
        "daemon did not enable websocket subscriptions:\n{logs}"
    );

    let metrics = scrape(metrics_port).await.unwrap_or_default();
    // And served its reads from the subscription rather than polling.
    assert!(
        metrics.contains(r#"chain_cache_reads_total{outcome="covered"}"#),
        "no covered cache reads, so the feed was not doing the work:\n{metrics}"
    );

    // The series the Grafana dashboard is built on, asserted present with
    // their labels. Metric names are an API: a rename that compiles fine
    // silently empties a panel, and nobody notices until they are staring
    // at the dashboard during an incident. `grafana/relay-dashboard.json`
    // is the consumer.
    [
        r#"relay_evaluations_total{program="#,
        r#"relay_skips_total{program="#,
        r#"relay_stage_seconds_count{program="#,
        r#"relay_compute_units_count{program="#,
        r#"relay_due_per_tick_count{program="#,
        r#"relay_refresh_seconds_count{phase="total"}"#,
        r#"relay_confirm_seconds_count{result="#,
        r#"relay_tick_seconds_count{phase="total"}"#,
        r#"relay_wake_lag_seconds_count{program="#,
        r#"chain_rpc_seconds_count{method="get_multiple_accounts"}"#,
        r#"chain_rpc_accounts_total{method="get_multiple_accounts"}"#,
        r#"chain_cached_accounts{kind="accounts"}"#,
        r#"chain_cache_reads_total{outcome="#,
        r#"chain_update_source_total{source="#,
    ]
    .iter()
    .for_each(|series| {
        assert!(
            metrics.contains(series),
            "dashboard series missing: {series}\n{metrics}"
        );
    });
    // And the two label values that carry the load breakdown, which is the
    // whole point of splitting evaluations by wake kind.
    assert!(
        metrics.contains(r#"wake="at_timestamp""#)
            && metrics.contains(r#"wake="on_account_change""#),
        "wake-kind breakdown missing:\n{metrics}"
    );
    // Skips must be broken out by reason, not lumped under one outcome.
    assert!(
        metrics.contains(r#"reason="not_due""#),
        "skip reasons missing:\n{metrics}"
    );

    // Fork detection is live. Reads run at `processed`, so a write can be
    // taken back with no correcting notification ever arriving; the slot
    // stream is the only thing that would tell us. Failing to subscribe is
    // deliberately non-fatal, which means it could go missing silently.
    assert!(
        !logs.contains("slot_subscribe failed"),
        "slot subscription failed, so fork detection was off:\n{logs}"
    );
    // And it does not cry wolf. One validator cannot fork, so any detection
    // here is a bug in the predicate — the shape to watch for is confirmed
    // and finalized statuses, which repeat slots the tip has already passed
    // and would otherwise read as a switch on every single slot, throwing
    // the cache away continuously.
    assert_eq!(
        metric_sum(&metrics, "chain_reorgs_total", r#"kind="detected""#),
        0,
        "fork switches reported against a single-node validator:\n{metrics}"
    );
    assert!(
        metrics.contains("relay_cranks_total"),
        "no metrics served on {metrics_port}; logs:\n{}",
        daemon.logs()
    );
    assert!(
        metrics
            .lines()
            .any(|line| line.starts_with("relay_cranks_total") && line.contains("sent")),
        "metrics show no sent cranks:\n{metrics}"
    );
    drop(daemon);
    drop(validator);
    let _ = std::fs::remove_dir_all(&dir);
}

/// Poll a condition until it holds or the deadline passes.
async fn wait_for<F, Fut>(timeout: Duration, mut check: F) -> bool
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = bool>,
{
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if check().await {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    false
}

/// Real RPC caps `getMultipleAccounts` at 100 keys per call. A turner
/// tracking more than a handful of watches blows straight past that on
/// every tick, so the source has to chunk.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs solana-test-validator; run scripts/e2e.sh"]
async fn account_reads_chunk_past_the_rpc_limit() {
    let validator = Validator::start().await;
    let source = RpcSource::new(validator.url());

    // Well past the limit, and deliberately a mix of real and missing
    // accounts so the ordering of the reply matters.
    let mut keys: Vec<Pubkey> = (0..250).map(|_| Pubkey::new_unique()).collect();
    let known = Pubkey::new_from_array([0u8; 32]); // system program
    keys[137] = known;

    let accounts = source
        .get_multiple_accounts(&keys)
        .await
        .expect("a turner with many watches must not fail its account reads");
    assert_eq!(accounts.len(), keys.len(), "one slot per requested key");
    assert!(
        accounts[137].is_some(),
        "results must stay aligned with the request across chunks"
    );
    assert!(accounts[0].is_none());
    drop(validator);
}

/// The guard's whole purpose, proven on chain rather than in simulation.
///
/// Every other underpayment test rejects the crank at simulation time,
/// which is the easy case. This one hand-builds a guarded transaction that
/// *does* reach the cluster while the book pays less than asserted, and
/// checks the runtime reverts all of it — including the executor's work.
/// That atomicity is what makes a sim-to-land race survivable.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs solana-test-validator; run scripts/e2e.sh"]
async fn guard_reverts_a_landed_underpaying_crank() {
    let validator = Validator::start().await;
    let client = Client::new(validator.url()).await;
    let payer = client.payer.pubkey();
    // Payout is a separate non-signing account, as an untrusted crank
    // requires.
    let payout = Pubkey::new_unique();
    client.fund(&payout, 1_000_000).await;

    let book = client.create_book(100).await;
    let now = client.unix_timestamp().await;
    client.post_quote(&book, now - 1, 100, SIDE_BID).await; // already expired
    assert_eq!(client.entry_count(&book).await, 1);

    // The book now pays half of what the crank will assert.
    client
        .send(
            &[Instruction {
                program_id: demo_id(),
                accounts: vec![
                    AccountMeta::new(book, false),
                    AccountMeta::new_readonly(payer, true),
                ],
                data: disc("set_payment_v0")
                    .into_iter()
                    .chain((PAYMENT / 2).to_le_bytes())
                    .collect(),
            }],
            &[],
        )
        .await;

    let nonce = 0u8;
    let guard = Pubkey::find_program_address(
        &[relay_spec::GUARD_SEED, payout.as_ref(), &[nonce]],
        &relay_id(),
    )
    .0;
    let system = Pubkey::new_from_array([0u8; 32]);
    let begin = Instruction {
        program_id: relay_id(),
        accounts: vec![
            AccountMeta::new(payer, true),
            AccountMeta::new_readonly(payout, false),
            AccountMeta::new(guard, false),
            AccountMeta::new_readonly(system, false),
        ],
        data: relay_spec::encode_begin_guard_v0_data(nonce).to_vec(),
    };
    // sweep_v0 { ids: [1] }
    let sweep = Instruction {
        program_id: demo_id(),
        accounts: vec![
            AccountMeta::new(payout, false),
            AccountMeta::new(book, false),
        ],
        data: disc("sweep_v0")
            .into_iter()
            .chain(1u32.to_le_bytes())
            .chain(1u64.to_le_bytes())
            .collect(),
    };
    let assert_paid = Instruction {
        program_id: relay_id(),
        accounts: vec![
            AccountMeta::new_readonly(payout, false),
            AccountMeta::new(guard, false),
        ],
        // Assert the full price, which the book no longer pays.
        data: relay_spec::encode_assert_paid_v0_data(PAYMENT, nonce).to_vec(),
    };

    let payout_before = client.lamports(&payout).await;
    let blockhash = client.rpc.get_latest_blockhash().await.unwrap();
    let tx = Transaction::new_signed_with_payer(
        &[begin.clone(), sweep.clone(), assert_paid],
        Some(&payer),
        &[&client.payer],
        blockhash,
    );
    let landed = client.rpc.send_and_confirm_transaction(&tx).await;
    assert!(
        landed.is_err(),
        "the guard should have failed the transaction"
    );

    // Everything reverted: the entry is still there and nothing was paid.
    assert_eq!(
        client.entry_count(&book).await,
        1,
        "the executor's work must revert with the guard"
    );
    assert_eq!(client.lamports(&payout).await, payout_before);

    // The same crank at the price the book actually pays goes through,
    // proving the failure was the assertion and not the setup.
    let assert_half = Instruction {
        program_id: relay_id(),
        accounts: vec![
            AccountMeta::new_readonly(payout, false),
            AccountMeta::new(guard, false),
        ],
        data: relay_spec::encode_assert_paid_v0_data(PAYMENT / 2, nonce).to_vec(),
    };
    let tx = Transaction::new_signed_with_payer(
        &[begin, sweep, assert_half],
        Some(&payer),
        &[&client.payer],
        client.rpc.get_latest_blockhash().await.unwrap(),
    );
    client.rpc.send_and_confirm_transaction(&tx).await.unwrap();
    assert_eq!(client.entry_count(&book).await, 0, "the honest crank lands");
    assert_eq!(client.lamports(&payout).await - payout_before, PAYMENT / 2);
    drop(validator);
}

// --- chaos: the subscription dies underneath a running daemon ---

/// A TCP proxy the test can sever, sitting between the daemon and the
/// validator's pubsub port. Killing the validator would take RPC down
/// too; this severs only the websocket, which is the failure that
/// actually happens in production — a provider drops the stream while
/// RPC keeps answering.
struct WsProxy {
    port: u16,
    severed: tokio::sync::watch::Sender<bool>,
}

impl WsProxy {
    async fn start(upstream_port: u16, port: u16) -> Self {
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", port))
            .await
            .expect("bind proxy");
        let (severed, _) = tokio::sync::watch::channel(false);
        let flag = severed.clone();
        tokio::spawn(async move {
            loop {
                let Ok((mut downstream, _)) = listener.accept().await else {
                    continue;
                };
                if *flag.borrow() {
                    // Refuse while severed, the way a dead provider does.
                    continue;
                }
                let mut watch = flag.subscribe();
                tokio::spawn(async move {
                    let Ok(mut upstream) =
                        tokio::net::TcpStream::connect(("127.0.0.1", upstream_port)).await
                    else {
                        return;
                    };
                    tokio::select! {
                        _ = tokio::io::copy_bidirectional(&mut downstream, &mut upstream) => {}
                        // Sever: drop both halves mid-stream.
                        _ = async { while watch.changed().await.is_ok() {
                            if *watch.borrow() { break; }
                        } } => {}
                    }
                });
            }
        });
        Self { port, severed }
    }

    fn sever(&self) {
        let _ = self.severed.send(true);
    }

    fn restore(&self) {
        let _ = self.severed.send(false);
    }
}

/// Count of cache reads served without a live subscription, straight from
/// the daemon's own metrics.
fn uncovered_reads(metrics: &str) -> u64 {
    metrics
        .lines()
        .find(|l| l.starts_with(r#"chain_cache_reads_total{outcome="uncovered"}"#))
        .and_then(|l| l.rsplit(' ').next())
        .and_then(|n| n.parse().ok())
        .unwrap_or(0)
}

/// A daemon whose websocket dies must keep cranking, not go blind.
///
/// This is the freshness design under fire: when the backend loses its
/// session it publishes empty coverage, so the cache stops trusting
/// silence and revalidates against RPC. Work continues, more expensively,
/// until the subscription comes back. Nothing else in the suite exercises
/// that path — the cache tests fake coverage, and the happy-path daemon
/// test never loses its stream.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs solana-test-validator; run scripts/e2e.sh"]
async fn daemon_survives_losing_its_subscription() {
    let validator = Validator::start().await;
    let client = Client::new(validator.url()).await;
    let proxy = WsProxy::start(RPC_PORT + 1, 9101).await;

    let keeper = Keypair::new();
    client.fund(&keeper.pubkey(), 10_000_000_000).await;
    let dir = std::env::temp_dir().join(format!("relay-chaos-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let keypair_path = dir.join("keeper.json").to_string_lossy().into_owned();
    keeper.write_to_file(&keypair_path).expect("write keypair");

    let book = client.create_book(100).await;
    let metrics_port = 9978;
    let daemon = Daemon::start_with_ws(
        &validator.url(),
        &format!("ws://127.0.0.1:{}", proxy.port),
        &keypair_path,
        metrics_port,
        dir.join("turner.log").to_string_lossy().into_owned(),
    );

    // Baseline: cranking with a healthy subscription.
    let now = client.unix_timestamp().await;
    client.post_quote(&book, now + 2, 100, SIDE_BID).await;
    assert!(
        wait_for(Duration::from_secs(60), || async {
            client.entry_count(&book).await == 0
        })
        .await,
        "baseline crank never happened.\nlogs:\n{}",
        daemon.logs()
    );

    // Sever the stream. RPC stays up, exactly as when a provider drops.
    proxy.sever();
    tokio::time::sleep(Duration::from_secs(2)).await;
    let during_start = uncovered_reads(&scrape(metrics_port).await.unwrap_or_default());

    // Work posted while blind must still get cranked.
    let now = client.unix_timestamp().await;
    client.post_quote(&book, now + 2, 101, SIDE_BID).await;
    assert!(
        wait_for(Duration::from_secs(60), || async {
            client.entry_count(&book).await == 0
        })
        .await,
        "daemon stopped cranking when its subscription died.\nlogs:\n{}",
        daemon.logs()
    );

    // And it noticed: with coverage revoked, reads fall back to RPC.
    let during_end = uncovered_reads(&scrape(metrics_port).await.unwrap_or_default());
    assert!(
        during_end > during_start,
        "expected uncovered reads to climb while the feed was down \
         ({during_start} -> {during_end})"
    );

    // Restore, and confirm it recovers rather than limping on RPC forever.
    proxy.restore();
    let now = client.unix_timestamp().await;
    client.post_quote(&book, now + 2, 102, SIDE_BID).await;
    assert!(
        wait_for(Duration::from_secs(90), || async {
            client.entry_count(&book).await == 0
        })
        .await,
        "daemon never recovered after the subscription returned.\nlogs:\n{}",
        daemon.logs()
    );
    let logs = daemon.logs();
    assert!(
        logs.matches("ws session failed").count() >= 1,
        "expected the daemon to report the dropped session:\n{logs}"
    );
    drop(daemon);
    drop(validator);
    let _ = std::fs::remove_dir_all(&dir);
}

// --- scale ---

impl Client {
    /// Create `count` books with watches, batching several per transaction
    /// and sending batches concurrently — otherwise the setup dominates
    /// the test.
    async fn create_books(&self, count: usize, evict_threshold: u32) -> Vec<Pubkey> {
        // Two books per transaction: three overflows the 1232-byte packet
        // limit once every new account's signature is counted.
        const PER_TX: usize = 2;
        let book_rent = self
            .rpc
            .get_minimum_balance_for_rent_exemption(BOOK_ACCOUNT_LEN)
            .await
            .unwrap();
        let watch_rent = self
            .rpc
            .get_minimum_balance_for_rent_exemption(WATCH_V0_LEN)
            .await
            .unwrap();

        let batches: Vec<Vec<(Keypair, Keypair)>> = (0..count)
            .map(|_| (Keypair::new(), Keypair::new()))
            .collect::<Vec<_>>()
            .chunks(PER_TX)
            .map(|chunk| {
                chunk
                    .iter()
                    .map(|(b, w)| (b.insecure_clone(), w.insecure_clone()))
                    .collect()
            })
            .collect();

        let mut created = Vec::new();
        let mut pending = Vec::new();
        for batch in &batches {
            let mut ixs = Vec::new();
            for (book, watch) in batch {
                ixs.push(solana_system_interface::instruction::create_account(
                    &self.payer.pubkey(),
                    &book.pubkey(),
                    book_rent + 50_000_000,
                    BOOK_ACCOUNT_LEN as u64,
                    &demo_id(),
                ));
                ixs.push(Instruction {
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
                });
                ixs.push(solana_system_interface::instruction::create_account(
                    &self.payer.pubkey(),
                    &watch.pubkey(),
                    watch_rent,
                    WATCH_V0_LEN as u64,
                    &relay_id(),
                ));
                ixs.push(Instruction {
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
                });
                created.push(book.pubkey());
            }
            pending.push((ixs, batch));
        }
        // Send batches concurrently; sequential setup dominates the test.
        let prepared: Vec<(Vec<Instruction>, Vec<&Keypair>)> = pending
            .into_iter()
            .map(|(ixs, batch)| (ixs, batch.iter().flat_map(|(b, w)| [b, w]).collect()))
            .collect();
        for group in prepared.chunks(8) {
            futures_util::future::join_all(
                group.iter().map(|(ixs, signers)| self.send(ixs, signers)),
            )
            .await;
        }
        created
    }
}

/// Average seconds per tick, from the daemon's own histogram.
/// Sum every sample of a labelled metric whose label set contains
/// `contains`. Prometheus text format, one sample per line.
fn metric_sum(metrics: &str, name: &str, contains: &str) -> u64 {
    metrics
        .lines()
        .filter(|line| line.starts_with(name) && line.contains(contains))
        .filter_map(|line| line.rsplit_once(' '))
        .filter_map(|(_, value)| value.trim().parse::<f64>().ok())
        .map(|value| value as u64)
        .sum()
}

fn mean_tick_seconds(metrics: &str, phase: &str) -> Option<f64> {
    let sum = metrics
        .lines()
        .find(|l| l.starts_with(&format!(r#"relay_tick_seconds_sum{{phase="{phase}"}}"#)))?
        .rsplit(' ')
        .next()?
        .parse::<f64>()
        .ok()?;
    let count = metrics
        .lines()
        .find(|l| l.starts_with(&format!(r#"relay_tick_seconds_count{{phase="{phase}"}}"#)))?
        .rsplit(' ')
        .next()?
        .parse::<f64>()
        .ok()?;
    (count > 0.0).then_some(sum / count)
}

/// A registry far larger than one `getMultipleAccounts` call, cranked by
/// the shipped daemon.
///
/// The unchunked-read bug lived exactly here: everything worked at three
/// books and broke at a hundred. This is the regression net for that whole
/// class — anything that silently assumes a small registry.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs solana-test-validator; run scripts/e2e.sh"]
async fn daemon_handles_a_registry_larger_than_one_rpc_call() {
    const BOOKS: usize = 120;

    let validator = Validator::start().await;
    let client = Client::new(validator.url()).await;
    let keeper = Keypair::new();
    client.fund(&keeper.pubkey(), 20_000_000_000).await;
    let dir = std::env::temp_dir().join(format!("relay-scale-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let keypair_path = dir.join("keeper.json").to_string_lossy().into_owned();
    keeper.write_to_file(&keypair_path).expect("write keypair");

    // Well past the 100-key ceiling on a single account read.
    let books = client.create_books(BOOKS, 100).await;
    assert_eq!(books.len(), BOOKS);

    let metrics_port = 9979;
    let daemon = Daemon::start(
        &validator.url(),
        &keypair_path,
        metrics_port,
        dir.join("turner.log").to_string_lossy().into_owned(),
    );

    // One already-expired quote on every book: BOOKS cranks to get through.
    client.post_expired_quotes(&books).await;

    let swept = wait_for(Duration::from_secs(180), || async {
        let mut remaining = 0;
        for book in books.iter() {
            remaining += client.entry_count(book).await;
            if remaining > 0 {
                return false;
            }
        }
        true
    })
    .await;
    let metrics = scrape(metrics_port).await.unwrap_or_default();
    assert!(
        swept,
        "not every book was swept.\nmetrics:\n{}\nlogs (tail):\n{}",
        metrics
            .lines()
            .filter(|l| l.starts_with("relay_cranks") || l.starts_with("relay_crank_failures"))
            .collect::<Vec<_>>()
            .join("\n"),
        daemon
            .logs()
            .lines()
            .rev()
            .take(20)
            .collect::<Vec<_>>()
            .join("\n")
    );

    // Nothing should be failing at this scale, at either stage: a read
    // that blew a limit shows up as a crank-stage failure, and a
    // transaction that was built too large or went stale shows up as an
    // on-chain one. With a single turner there is no contention to excuse
    // either.
    assert!(
        !metrics
            .lines()
            .any(|l| l.starts_with("relay_crank_failures_total")),
        "cranks failed at scale:\n{metrics}"
    );
    assert_eq!(
        metric_sum(&metrics, "relay_transactions_total", "result=\"failed\""),
        0,
        "transactions failed on chain at scale:\n{metrics}"
    );

    // And a tick should still complete in a sane time with 120 watches
    // and 360 conditions to evaluate.
    let mean = mean_tick_seconds(&metrics, "total").unwrap_or(f64::MAX);
    assert!(
        mean < 10.0,
        "mean tick took {mean:.2}s with {BOOKS} books; something is superlinear"
    );
    println!("scale: {BOOKS} books, mean tick {mean:.3}s");
    drop(daemon);
    drop(validator);
    let _ = std::fs::remove_dir_all(&dir);
}

/// Two turners, one registry — the realistic keeper-fleet shape.
///
/// Nothing coordinates them: both see the same watches, both decide the
/// same conditions are ready, and both race to land the same crank. Only
/// one can win, because the executor's own state check fails once the work
/// is done. The question this answers is what the loser does with that: a
/// clean per-condition failure and a retry on the next tick is fine, but a
/// turner that treats a lost race as a fatal error, or that backs off
/// without bound, would stall a fleet.
///
/// Proven by a second round of work after the collisions: if either daemon
/// had wedged, the round would not finish, and if both had wedged it would
/// not finish at all.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs solana-test-validator; run scripts/e2e.sh"]
async fn two_turners_share_one_registry_without_wedging() {
    const BOOKS: usize = 24;

    let validator = Validator::start().await;
    let client = Client::new(validator.url()).await;
    let dir = std::env::temp_dir().join(format!("relay-fleet-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();

    let books = client.create_books(BOOKS, 100).await;

    // Two independent keepers, each with its own keypair, submitter, and
    // metrics endpoint — as separate as two hosts would be.
    let ports = [9977u16, 9978];
    let mut daemons = Vec::new();
    for (i, port) in ports.iter().enumerate() {
        let keeper = Keypair::new();
        let path = dir.join(format!("keeper-{i}.json"));
        keeper.write_to_file(&path).expect("write keypair");
        // Funding has to land before the daemon starts, or its first ticks
        // fail on an empty fee payer.
        client.fund(&keeper.pubkey(), 10_000_000_000).await;
        daemons.push(Daemon::start(
            &validator.url(),
            &path.to_string_lossy(),
            *port,
            dir.join(format!("turner-{port}.log"))
                .to_string_lossy()
                .into_owned(),
        ));
    }

    // Round one: BOOKS cranks, two turners contending for every one.
    client.post_expired_quotes(&books).await;
    assert!(
        wait_for(Duration::from_secs(120), || async {
            all_books_swept(&client, &books).await
        })
        .await,
        "round one never finished\n{}",
        fleet_report(&ports, &daemons).await
    );

    // Both turners must get work in the first round: the contention delay
    // is driven by observed reverts, and it takes a confirm cycle or two to
    // ramp, so neither has had time to stand down this early.
    let sent: Vec<u64> = futures_util::future::join_all(ports.iter().map(|port| async move {
        metric_sum(
            &scrape(*port).await.unwrap_or_default(),
            "relay_cranks_total",
            "outcome=\"sent\"",
        )
    }))
    .await;
    assert!(
        sent.iter().all(|&n| n > 0),
        "one turner landed nothing: {sent:?}\n{}",
        fleet_report(&ports, &daemons).await
    );

    // Round two is the real assertion: whatever the collisions did to the
    // losers, both are still turning.
    client.post_expired_quotes(&books).await;
    assert!(
        wait_for(Duration::from_secs(120), || async {
            all_books_swept(&client, &books).await
        })
        .await,
        "round two never finished — a turner wedged on lost races\n{}",
        fleet_report(&ports, &daemons).await
    );

    let after: Vec<u64> = futures_util::future::join_all(ports.iter().map(|port| async move {
        metric_sum(
            &scrape(*port).await.unwrap_or_default(),
            "relay_cranks_total",
            "outcome=\"sent\"",
        )
    }))
    .await;
    // The fleet as a whole kept working. Deliberately *not* asserted
    // per-turner: a turner that keeps losing races ramps its contention
    // delay and stops submitting, which is the correct response, not a
    // stall — see `a_losing_turner_delays_itself_and_recovers_when_the_rival_dies`
    // for that behaviour on its own. What must hold here is that going
    // quiet is a choice one turner makes, not a state the fleet gets stuck
    // in, which the completed second round already establishes.
    let (before, now): (u64, u64) = (sent.iter().sum(), after.iter().sum());
    assert!(
        now > before,
        "the fleet stopped landing cranks after the first round: {sent:?} -> {after:?}\n{}",
        fleet_report(&ports, &daemons).await
    );
    // Finally, prove the test exercised what it claims. Two uncoordinated
    // turners both crank everything, so roughly half of all submissions
    // should be losses — and a loss lands on chain and reverts, which is
    // `relay_transactions_total{outcome="failed"}`, not a crank-stage
    // failure. Zero here would mean the two never actually contended and
    // the rest of this test proved nothing.
    let (mut lost, mut landed) = (0, 0);
    for port in ports {
        let metrics = scrape(port).await.unwrap_or_default();
        lost += metric_sum(&metrics, "relay_transactions_total", "result=\"failed\"");
        landed += metric_sum(&metrics, "relay_transactions_total", "result=\"landed\"");
    }
    assert!(
        lost > 0,
        "the two turners never collided, so nothing about contention was tested\n{}",
        fleet_report(&ports, &daemons).await
    );
    // And the losses are bounded by the work itself. Losing is supposed to
    // be self-limiting: the next tick re-reads the book, finds the winner
    // already swept it, and resolves to no-work instead of resubmitting. So
    // across both rounds there can be at most one lost transaction per unit
    // of work — and rather fewer, since each transaction packs several
    // cranks. A turner that answered a lost race by retrying blind would
    // sail past this.
    let work = (BOOKS * 2) as u64;
    assert!(
        lost <= work,
        "{lost} lost transactions for {work} units of work ({landed} landed) — \
         losing is not self-limiting\n{}",
        fleet_report(&ports, &daemons).await
    );
}

/// Metrics text from every port, in order.
async fn scrape_all(ports: &[u16]) -> Vec<String> {
    futures_util::future::join_all(
        ports
            .iter()
            .map(|port| async move { scrape(*port).await.unwrap_or_default() }),
    )
    .await
}

async fn all_books_swept(client: &Client, books: &[Pubkey]) -> bool {
    for book in books {
        if client.entry_count(book).await > 0 {
            return false;
        }
    }
    true
}

/// Metrics and log tails from every daemon, for failure messages.
async fn fleet_report(ports: &[u16], daemons: &[Daemon]) -> String {
    let mut report = String::new();
    for (port, daemon) in ports.iter().zip(daemons) {
        let metrics = scrape(*port).await.unwrap_or_default();
        let counters: Vec<&str> = metrics
            .lines()
            .filter(|l| {
                l.starts_with("relay_cranks_total")
                    || l.starts_with("relay_crank_failures_total")
                    || l.starts_with("relay_transactions_total")
            })
            .collect();
        report.push_str(&format!(
            "--- turner :{port} ---\n{}\nlogs (tail):\n{}\n",
            counters.join("\n"),
            daemon
                .logs()
                .lines()
                .rev()
                .take(15)
                .collect::<Vec<_>>()
                .join("\n")
        ));
    }
    report
}

/// The adaptive contention delay, end to end: a turner that keeps losing
/// races stops paying for them, and starts again when the rival dies.
///
/// This is tuktuk's lesson. Two turners racing means the slower one does
/// the work, arrives second, and pays a fee for a reverted transaction —
/// indefinitely, because nothing about losing tells it to stop trying. The
/// fix is to lose on purpose: hold back a few seconds, and the rival's
/// crank lands before the delayed turner even resolves, so its simulation
/// reports nothing to do and no transaction is built. A loss that costs
/// nothing is sustainable.
///
/// The half that matters more is the recovery. A turner that backed off
/// permanently would leave the protocol uncranked the moment its rival
/// died, so the delay has to decay: cranks start landing again, the delay
/// walks back down, and the only lasting cost is running a few seconds
/// late. Both halves are asserted here — the ramp under contention, and
/// the recovery after the rival is killed.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs solana-test-validator; run scripts/e2e.sh"]
async fn a_losing_turner_delays_itself_and_recovers_when_the_rival_dies() {
    const BOOKS: usize = 24;
    const DELAY_METRIC: &str = "relay_contention_delay_slots";

    let validator = Validator::start().await;
    let client = Client::new(validator.url()).await;
    let dir = std::env::temp_dir().join(format!("relay-contend-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();

    let books = client.create_books(BOOKS, 100).await;

    let ports = [9975u16, 9976];
    let mut daemons = Vec::new();
    for (i, port) in ports.iter().enumerate() {
        let keeper = Keypair::new();
        let path = dir.join(format!("keeper-{i}.json"));
        keeper.write_to_file(&path).expect("write keypair");
        client.fund(&keeper.pubkey(), 10_000_000_000).await;
        daemons.push(Daemon::start(
            &validator.url(),
            &path.to_string_lossy(),
            *port,
            dir.join(format!("turner-{port}.log"))
                .to_string_lossy()
                .into_owned(),
        ));
    }

    // Contended rounds. The delay is a gauge that decays, so sampling it
    // only at the end would miss the ramp — poll while the work drains and
    // keep the peak per turner.
    //
    // Several rounds, and a settle window after them, because the feedback
    // is not instant: the delay only moves once the submitter's confirm
    // pass observes a reverted transaction, which is a couple of seconds
    // behind the crank that lost. Stopping the moment the books went empty
    // measured the delay before the losses had been accounted for.
    const CONTENDED_ROUNDS: usize = 3;
    let mut peak = [0u64; 2];
    let sample = |peak: &mut [u64; 2], metrics: &[String]| {
        (0..2).for_each(|i| {
            peak[i] = peak[i].max(metric_sum(&metrics[i], DELAY_METRIC, "program="));
        });
    };
    for round in 0..CONTENDED_ROUNDS {
        client.post_expired_quotes(&books).await;
        let mut swept = false;
        let deadline = Instant::now() + Duration::from_secs(60);
        while Instant::now() < deadline {
            let metrics = scrape_all(&ports).await;
            sample(&mut peak, &metrics);
            if all_books_swept(&client, &books).await {
                swept = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
        assert!(
            swept,
            "contended round {round} never finished\n{}",
            fleet_report(&ports, &daemons).await
        );
    }
    // Settle: let the confirm pass account for the last round's losses.
    let settle = Instant::now() + Duration::from_secs(10);
    while Instant::now() < settle {
        let metrics = scrape_all(&ports).await;
        sample(&mut peak, &metrics);
        tokio::time::sleep(Duration::from_millis(250)).await;
    }

    // Kill the winner: the one that held back least.
    let loser = if peak[0] >= peak[1] { 0 } else { 1 };
    let winner = 1 - loser;
    drop(daemons.remove(winner));
    let (loser_port, loser_peak) = (ports[loser], peak[loser]);

    // Round two, uncontested. Two things have to be true now, and they pull
    // in opposite directions: the delayed turner must still do the work
    // (late is fine, never is not), and its delay must come back down.
    //
    // Recovery is driven by cranks landing, so it needs work to land — with
    // an idle registry the delay just freezes wherever contention left it.
    // That is correct behaviour, not a bug, so this keeps feeding batches
    // the way a live protocol would, and waits for the delay to reach zero.
    let mut swept_alone = false;
    let mut delay_now = loser_peak;
    let deadline = Instant::now() + Duration::from_secs(120);
    while Instant::now() < deadline && delay_now > 0 {
        client.post_expired_quotes(&books).await;
        let batch = Instant::now() + Duration::from_secs(45);
        while Instant::now() < batch {
            if all_books_swept(&client, &books).await {
                swept_alone = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
        delay_now = metric_sum(
            &scrape(loser_port).await.unwrap_or_default(),
            DELAY_METRIC,
            "program=",
        );
    }
    let metrics = scrape(loser_port).await.unwrap_or_default();
    assert!(
        swept_alone,
        "the delayed turner never picked the work up after its rival died \
         (delay {delay_now} slots)\nmetrics:\n{}\nlogs (tail):\n{}",
        metrics
            .lines()
            .filter(|l| l.starts_with("relay_"))
            .collect::<Vec<_>>()
            .join("\n"),
        daemons[0]
            .logs()
            .lines()
            .rev()
            .take(20)
            .collect::<Vec<_>>()
            .join("\n")
    );

    // Fully recovered: back to real time, not merely less handicapped. A
    // turner still holding back after its rival is gone is pure latency.
    println!(
        "contention: peaks {peak:?} slots, loser :{loser_port} recovered {loser_peak} -> {delay_now}"
    );
    assert_eq!(
        delay_now,
        0,
        "delay never decayed after the rival died: peak {loser_peak}\n{}",
        metrics
            .lines()
            .filter(|l| l.starts_with("relay_"))
            .collect::<Vec<_>>()
            .join("\n")
    );
}

/// The `relay` CLI against a live book: list, explain, and actually crank.
///
/// This lives here rather than in the CLI's own suite because it needs a real
/// target program with real conditions, and demo-book plus its layout mirrors
/// already live in this file. What it proves is the part string assertions on
/// a synthetic registry cannot: that `explain` reaches READY on a genuinely
/// due condition, and that `run --send` does the work — the CLI is not just a
/// viewer.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs solana-test-validator; run scripts/e2e.sh"]
async fn the_cli_explains_and_cranks_a_real_condition() {
    let validator = Validator::start().await;
    let client = Client::new(validator.url()).await;
    let keeper = Keypair::new();
    client.fund(&keeper.pubkey(), 10_000_000_000).await;
    let dir = std::env::temp_dir().join(format!("relay-cli-live-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let keypair_path = dir.join("keeper.json").to_string_lossy().into_owned();
    keeper.write_to_file(&keypair_path).expect("write keypair");

    let book = client.create_book(100).await;
    let now = client.unix_timestamp().await;
    client.post_quote(&book, now - 1, 100, SIDE_BID).await; // already expired
    assert_eq!(client.entry_count(&book).await, 1);

    let url = validator.url();
    let target = book.to_string();
    let demo = demo_id().to_string();

    // Listed, with the sweep condition showing as ready to crank.
    let (stdout, stderr, ok) = cli(&[
        "--rpc-url",
        &url,
        "--trusted-program",
        &demo,
        "condition",
        "list",
    ]);
    assert!(ok, "condition list failed: {stderr}{stdout}");
    assert!(stdout.contains("at-timestamp"), "{stdout}");
    assert!(
        stdout.contains("READY"),
        "an expired quote should leave the sweep ready:\n{stdout}"
    );

    // Explained: the gates, then the verdict, then the transaction.
    let (stdout, stderr, ok) = cli(&[
        "--rpc-url",
        &url,
        "--trusted-program",
        &demo,
        "condition",
        "explain",
        &target,
    ]);
    assert!(ok, "explain failed: {stderr}{stdout}");
    ["gates", "wake due", "VERDICT", "ready to crank"]
        .iter()
        .for_each(|needle| {
            assert!(stdout.contains(needle), "explain omits {needle}:\n{stdout}");
        });
    // Explaining must never do the work.
    assert_eq!(
        client.entry_count(&book).await,
        1,
        "explain cranked something"
    );

    // Dry run is the default, and says so.
    let (stdout, _, ok) = cli(&[
        "--rpc-url",
        &url,
        "--trusted-program",
        &demo,
        "condition",
        "run",
        &target,
    ]);
    assert!(ok, "{stdout}");
    assert!(stdout.contains("dry run"), "{stdout}");
    assert_eq!(client.entry_count(&book).await, 1, "dry run sent");

    // And --send actually cranks it.
    let (stdout, stderr, ok) = cli(&[
        "--rpc-url",
        &url,
        "--keypair",
        &keypair_path,
        "--trusted-program",
        &demo,
        "condition",
        "run",
        &target,
        "--send",
    ]);
    assert!(ok, "run --send failed: {stderr}{stdout}");
    assert!(stdout.contains("sent "), "{stdout}");
    assert!(
        wait_for(Duration::from_secs(30), || async {
            client.entry_count(&book).await == 0
        })
        .await,
        "the CLI's crank never landed:\n{stdout}"
    );

    // With the work gone the same condition reads as not due, because
    // demo-book pushes its wake out to i64::MAX once no entry is live. That
    // is the shape of a healthy idle condition, and worth pinning: an
    // operator looking at a quiet system needs "nothing to do yet" to be
    // distinguishable from "stuck".
    let (stdout, _, ok) = cli(&[
        "--rpc-url",
        &url,
        "--trusted-program",
        &demo,
        "condition",
        "explain",
        &target,
    ]);
    assert!(ok, "{stdout}");
    assert!(
        stdout.contains("not due"),
        "expected an idle verdict after sweeping:\n{stdout}"
    );
    assert!(
        !stdout.contains("ready to crank"),
        "the swept condition should not still read as ready:\n{stdout}"
    );
    assert!(
        stdout.contains("9223372036854775807"),
        "the wake should have been pushed out to i64::MAX:\n{stdout}"
    );
}

/// Run the `relay` binary from the shared workspace target directory.
///
/// `CARGO_BIN_EXE_` only covers the current package, so this resolves the
/// sibling crate's binary by path and fails loudly if it is missing rather
/// than skipping — a test that quietly does nothing is worse than none.
fn cli(args: &[&str]) -> (String, String, bool) {
    let path = format!("{}/../target/debug/relay", env!("CARGO_MANIFEST_DIR"));
    assert!(
        std::path::Path::new(&path).exists(),
        "{path} is missing — build it first (`cargo build -p relay-cli`); \
         scripts/e2e.sh does this for you"
    );
    let output = Command::new(&path)
        .args(args)
        .env_remove("RELAY_RPC_URL")
        .env_remove("RELAY_PROGRAM_ID")
        .env_remove("RELAY_KEEPER_KEYPAIR")
        .env_remove("RELAY_METRICS_URL")
        .output()
        .expect("run relay cli");
    (
        String::from_utf8_lossy(&output.stdout).into_owned(),
        String::from_utf8_lossy(&output.stderr).into_owned(),
        output.status.success(),
    )
}
