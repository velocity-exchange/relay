//! The CLI, run as an operator would run it, against a real validator.
//!
//! What this covers is the wiring the unit tests structurally cannot: clap
//! parsing, the RPC connection, the registry scan with its memcmp filters,
//! and whether the rendered output actually says the thing. The *verdicts*
//! are covered where the logic lives — `explain_*` in the turner's litesvm
//! suite — because the CLI is a presentation layer over
//! `Turner::explain` and duplicating those cases here would only add a
//! second set of hand-pinned layout mirrors to keep in sync.
//!
//! Deliberately no demo-book: a watch can be registered against any
//! account, and one whose data is not a condition block exercises the
//! unreadable-block diagnosis, which is a real failure operators hit when a
//! target's layout changes without re-registering.
//!
//! Ignored by default (needs the validator binary). Run with:
//!
//! ```text
//! ./scripts/cli-e2e.sh
//! ```

#![cfg(not(target_os = "windows"))]

use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use sha2::{Digest, Sha256};
use solana_client::nonblocking::rpc_client::RpcClient;
use solana_commitment_config::CommitmentConfig;
use solana_sdk::instruction::{AccountMeta, Instruction};
use solana_sdk::pubkey::Pubkey;
use solana_sdk::signature::{Keypair, Signer};
use solana_sdk::transaction::Transaction;

/// Distinct from the turner suite's ports so the two can run at once.
const RPC_PORT: u16 = 8997;
const FAUCET_PORT: u16 = 9897;
const WATCH_V0_LEN: usize = relay_spec::WATCH_V0_LEN;
const CONDITIONS_OFFSET: u32 = 912;
/// The system program owns any plain funded account, so it is what
/// `register_watch_v0` records as the target program here.
const SYSTEM_PROGRAM_ID: Pubkey = Pubkey::new_from_array([0u8; 32]);

fn relay_id() -> Pubkey {
    "4D5tPhw9sqkdkR5CpmP427TH6y9p9AMuKUukUEHn3Mpu"
        .parse()
        .unwrap()
}

fn disc(name: &str) -> Vec<u8> {
    Sha256::digest(format!("global:{name}").as_bytes())[..8].to_vec()
}

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
            .join(format!("relay-cli-e2e-{stamp}"))
            .to_string_lossy()
            .into_owned();
        let so = format!(
            "{}/../programs/target/deploy/relay.so",
            env!("CARGO_MANIFEST_DIR")
        );
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
                &so,
            ])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("solana-test-validator on PATH; build programs first");
        let validator = Self { child, ledger };
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

/// Run the shipped `relay` binary and return (stdout, stderr, success).
fn relay(args: &[&str]) -> (String, String, bool) {
    let output = Command::new(env!("CARGO_BIN_EXE_relay"))
        .args(args)
        // Env vars for this tool are global-arg fallbacks; a developer's
        // shell must not change what the test asserts.
        .env_remove("RELAY_RPC_URL")
        .env_remove("RELAY_PROGRAM_ID")
        .env_remove("RELAY_KEEPER_KEYPAIR")
        .env_remove("RELAY_METRICS_URL")
        .env_remove("RELAY_MIN_CRANK_PAYMENT")
        .output()
        .expect("run relay");
    (
        String::from_utf8_lossy(&output.stdout).into_owned(),
        String::from_utf8_lossy(&output.stderr).into_owned(),
        output.status.success(),
    )
}

/// Register a watch against an account that is not a condition block.
async fn register_dummy_watch(url: String) -> (Pubkey, Keypair) {
    let rpc = RpcClient::new_with_commitment(url, CommitmentConfig::confirmed());
    let payer = Keypair::new();
    let signature = rpc
        .request_airdrop(&payer.pubkey(), 100_000_000_000)
        .await
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(30);
    while Instant::now() < deadline {
        if rpc.confirm_transaction(&signature).await.unwrap_or(false) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }

    // A plain funded account: system-owned, and its data is zeroes rather
    // than a condition block.
    let target = Keypair::new();
    let watch = Keypair::new();
    let target_len = 2000usize;
    let create_target = solana_system_interface::instruction::create_account(
        &payer.pubkey(),
        &target.pubkey(),
        rpc.get_minimum_balance_for_rent_exemption(target_len)
            .await
            .unwrap(),
        target_len as u64,
        &SYSTEM_PROGRAM_ID,
    );
    let create_watch = solana_system_interface::instruction::create_account(
        &payer.pubkey(),
        &watch.pubkey(),
        rpc.get_minimum_balance_for_rent_exemption(WATCH_V0_LEN)
            .await
            .unwrap(),
        WATCH_V0_LEN as u64,
        &relay_id(),
    );
    let register = Instruction {
        program_id: relay_id(),
        accounts: vec![
            AccountMeta::new_readonly(payer.pubkey(), true),
            AccountMeta::new_readonly(target.pubkey(), false),
            AccountMeta::new(watch.pubkey(), false),
        ],
        data: disc("register_watch_v0")
            .into_iter()
            .chain(CONDITIONS_OFFSET.to_le_bytes())
            .collect(),
    };
    let blockhash = rpc.get_latest_blockhash().await.unwrap();
    let tx = Transaction::new_signed_with_payer(
        &[create_target, create_watch, register],
        Some(&payer.pubkey()),
        &[&payer, &target, &watch],
        blockhash,
    );
    rpc.send_and_confirm_transaction(&tx)
        .await
        .unwrap_or_else(|err| panic!("register watch: {err}"));
    (target.pubkey(), payer)
}

/// Argument handling, which needs no chain at all. The point is that the
/// failures are legible: a tool reached for while something is already
/// broken must not add a confusing error of its own.
#[test]
fn missing_arguments_fail_with_an_actionable_message() {
    let (_, stderr, ok) = relay(&["watch", "list"]);
    assert!(!ok, "no rpc url should fail");
    assert!(
        stderr.contains("--rpc-url"),
        "error should name the missing flag: {stderr}"
    );

    // Sending needs a real key, and saying so beats a signature error.
    let (_, stderr, ok) = relay(&[
        "--rpc-url",
        "http://127.0.0.1:1",
        "condition",
        "run",
        &Pubkey::new_unique().to_string(),
        "--send",
    ]);
    assert!(!ok);
    assert!(
        stderr.contains("--keypair") || stderr.contains("rpc") || stderr.contains("connect"),
        "unhelpful failure: {stderr}"
    );

    // Help is the front door for a debugging tool: it must list the
    // commands an operator is looking for.
    let (stdout, _, ok) = relay(&["--help"]);
    assert!(ok);
    ["watch", "condition", "guard", "clock", "doctor"]
        .iter()
        .for_each(|command| {
            assert!(
                stdout.contains(command),
                "--help omits {command}:\n{stdout}"
            );
        });
}

/// An empty registry is a real state — a fresh deployment, or a wrong
/// `--program-id` — and the answer has to distinguish those rather than
/// printing an empty table.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs solana-test-validator; run scripts/cli-e2e.sh"]
async fn an_empty_registry_says_so_and_says_what_to_check() {
    let validator = Validator::start().await;

    let (stdout, _, ok) = relay(&["--rpc-url", &validator.url(), "watch", "list"]);
    assert!(ok, "{stdout}");
    assert!(stdout.contains("0 watch"), "{stdout}");

    let (stdout, _, ok) = relay(&["--rpc-url", &validator.url(), "doctor"]);
    assert!(ok, "{stdout}");
    assert!(
        stdout.contains("No watches tracked") && stdout.contains("--program-id"),
        "doctor should point at the likely cause:\n{stdout}"
    );

    // The clock is what timestamp and slot wakes are compared against, so
    // reading it has to work on its own.
    let (stdout, _, ok) = relay(&["--rpc-url", &validator.url(), "clock", "--json"]);
    assert!(ok, "{stdout}");
    let value: serde_json::Value = serde_json::from_str(&stdout).expect("clock emits json");
    assert!(value["slot"].as_u64().unwrap() > 0, "{stdout}");
    assert!(value["unix_timestamp"].as_i64().unwrap() > 0, "{stdout}");
}

/// A registered watch whose target is not a condition block.
///
/// This is the dead end the CLI has to eliminate. The filter rejects an
/// unreadable block at refresh, so the watch never enters the tracked set:
/// it is absent from `watch list`, has no conditions, and is never cranked —
/// while being plainly present on chain, which is why an operator is right
/// to insist it exists. Every command must therefore be able to say "it is
/// registered, and here is why this turner threw it away".
#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs solana-test-validator; run scripts/cli-e2e.sh"]
async fn a_rejected_watch_is_reported_rather_than_vanishing() {
    let validator = Validator::start().await;
    let (target, payer) = register_dummy_watch(validator.url()).await;
    let url = validator.url();

    // Not tracked — but the count of rejects is volunteered, unprompted, so
    // an operator is never left staring at an empty table.
    let (stdout, _, ok) = relay(&["--rpc-url", &url, "watch", "list"]);
    assert!(ok, "{stdout}");
    assert!(stdout.contains("0 watch(es) tracked"), "{stdout}");
    assert!(
        stdout.contains("--rejected"),
        "an empty list must point at the rejected set:\n{stdout}"
    );

    // And --rejected names the watch, the reason, and the fix.
    let (stdout, _, ok) = relay(&["--rpc-url", &url, "watch", "list", "--rejected"]);
    assert!(ok, "{stdout}");
    assert!(stdout.contains(&target.to_string()), "{stdout}");
    assert!(stdout.contains("unparseable block"), "{stdout}");
    assert!(
        stdout.contains("offset") && stdout.contains("layout"),
        "the advice must say what to check:\n{stdout}"
    );
    assert!(
        stdout.contains(&SYSTEM_PROGRAM_ID.to_string()),
        "the recorded target program should be shown:\n{stdout}"
    );

    // Asking about it directly gets the same diagnosis, not "not found".
    let (_, stderr, ok) = relay(&[
        "--rpc-url",
        &url,
        "condition",
        "explain",
        &target.to_string(),
    ]);
    assert!(!ok);
    assert!(
        stderr.contains("IS registered") && stderr.contains("unparseable block"),
        "explain must distinguish rejected from absent:\n{stderr}"
    );

    // Doctor finds it without being told where to look.
    let (stdout, _, ok) = relay(&["--rpc-url", &url, "doctor"]);
    assert!(ok, "{stdout}");
    assert!(
        stdout.contains("unparseable block") && stdout.contains(&target.to_string()),
        "doctor should surface rejects:\n{stdout}"
    );

    // JSON carries it too, for a runbook.
    let (stdout, _, ok) = relay(&["--rpc-url", &url, "doctor", "--json"]);
    assert!(ok, "{stdout}");
    let value: serde_json::Value = serde_json::from_str(&stdout).expect("json");
    assert_eq!(value["rejected"][0]["target"], target.to_string());
    assert_eq!(value["rejected"][0]["reason"], "unparseable block");
    assert_eq!(value["rejected"][0]["offset"], CONDITIONS_OFFSET);

    // An address with nothing registered against it is a different answer
    // from one that was registered and thrown away, and must read as one.
    let (_, stderr, ok) = relay(&[
        "--rpc-url",
        &url,
        "condition",
        "explain",
        &payer.pubkey().to_string(),
    ]);
    assert!(!ok);
    assert!(
        stderr.contains("nothing is registered") || stderr.contains("no watch"),
        "unhelpful error for an unregistered target: {stderr}"
    );
    assert!(
        !stderr.contains("IS registered"),
        "must not claim an unregistered target is registered: {stderr}"
    );
}
