//! The running daemon's own state, which a fresh process cannot infer:
//! failure backoff, post-send suppression, adaptive contention delay, and the
//! rolling profitability window. All of it is scraped from the daemon's
//! metrics endpoint rather than guessed at.

use anyhow::{anyhow, Context, Result};
use serde_json::json;
use solana_sdk::pubkey::Pubkey;

use crate::session::Common;

pub async fn print_daemon_state(common: &Common, program: &Pubkey) {
    let Some(url) = &common.metrics_url else {
        println!(
            "\nPass --metrics-url http://host:9899/metrics to also report the \
             running daemon's backoff, contention delay and profitability."
        );
        return;
    };
    match scrape(url).await {
        Ok(metrics) => {
            println!("\ndaemon ({url})");
            let label = short_program(program);
            let show = |name: &str, needle: &str| {
                let value: Vec<&str> = metrics.lines().filter(|l| l.starts_with(needle)).collect();
                if value.is_empty() {
                    println!("  {name}: (no samples)");
                } else {
                    value.iter().for_each(|line| println!("  {line}"));
                }
            };
            show("contention delay", "relay_contention_delay_slots");
            show("cranks", "relay_cranks_total");
            show("failures", "relay_crank_failures_total");
            show("transactions", "relay_transactions_total");
            show("cache", "chain_cache_reads_total");
            show("reorgs", "chain_reorgs_total");
            if !metrics.contains(&label) {
                println!(
                    "  note: no samples labelled {label} — this daemon has never \
                     cranked the program you asked about, which usually means its \
                     filters exclude it."
                );
            }
        }
        Err(err) => println!("\ndaemon ({url}): unreachable ({err:#})"),
    }
}

/// Prometheus labels programs by a short prefix, matching the turner.
pub fn short_program(program: &Pubkey) -> String {
    program.to_string().chars().take(8).collect()
}

pub async fn daemon_state(common: &Common, program: &Pubkey) -> serde_json::Value {
    let Some(url) = &common.metrics_url else {
        return json!(null);
    };
    match scrape(url).await {
        Ok(metrics) => json!({
            "url": url,
            "program_label": short_program(program),
            "metrics": metrics
                .lines()
                .filter(|l| l.starts_with("relay_") || l.starts_with("chain_"))
                .collect::<Vec<_>>(),
        }),
        Err(err) => json!({"url": url, "error": format!("{err:#}")}),
    }
}

/// A one-shot HTTP GET. Deliberately hand-rolled: a debugging CLI should not
/// drag a TLS stack in to read a plaintext metrics endpoint on a private
/// network.
async fn scrape(url: &str) -> Result<String> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let rest = url
        .strip_prefix("http://")
        .ok_or_else(|| anyhow!("metrics url must be http://host:port[/metrics]"))?;
    let (authority, path) = rest.split_once('/').unwrap_or((rest, "metrics"));
    let mut socket = tokio::net::TcpStream::connect(authority)
        .await
        .with_context(|| format!("connect {authority}"))?;
    socket
        .write_all(
            format!("GET /{path} HTTP/1.1\r\nHost: {authority}\r\nConnection: close\r\n\r\n")
                .as_bytes(),
        )
        .await?;
    let mut body = String::new();
    socket.read_to_string(&mut body).await?;
    Ok(body)
}
