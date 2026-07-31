//! Output formatting. Every command can emit either a human table or JSON
//! (`--json`), so the same tool serves an operator at a terminal and a
//! script in a runbook.

use relay_crank_turner::{SkipReason, Stage, Verdict};
use serde_json::{json, Value};
use solana_sdk::pubkey::Pubkey;

/// A wake, described in terms of the two values whose comparison decides
/// whether it is due. This is the single most useful thing to print when a
/// condition is not firing: it turns "NotDue" into "waiting for 1794, chain
/// is at 1750, so 44 to go".
pub struct WakeDetail {
    pub kind: &'static str,
    /// What it is waiting for, rendered.
    pub waiting_for: String,
    /// What the chain currently reads.
    pub chain_reads: String,
    /// Populated when the two are numerically comparable.
    pub remaining: Option<i128>,
}

pub fn wake_detail(
    condition: &relay_spec::ConditionV0,
    clock: &relay_chain_source::ClockSnapshot,
    watched_now: Option<&[u8]>,
) -> WakeDetail {
    match condition.wake() {
        Ok(relay_spec::WakeView::AtTimestamp { unix_ts }) => WakeDetail {
            kind: "at-timestamp",
            waiting_for: format!("unix_ts {unix_ts}"),
            chain_reads: format!("clock {}", clock.unix_timestamp),
            remaining: Some(unix_ts as i128 - clock.unix_timestamp as i128),
        },
        Ok(relay_spec::WakeView::AtSlot { slot }) => WakeDetail {
            kind: "at-slot",
            waiting_for: format!("slot {slot}"),
            chain_reads: format!("slot {}", clock.slot),
            remaining: Some(slot as i128 - clock.slot as i128),
        },
        Ok(relay_spec::WakeView::EverySlots { slots }) => WakeDetail {
            kind: "every-slots",
            waiting_for: format!("every {slots} slots"),
            chain_reads: format!("slot {}", clock.slot),
            remaining: None,
        },
        Ok(relay_spec::WakeView::OnAccountChange {
            address,
            offset,
            len,
        }) => WakeDetail {
            kind: "on-account-change",
            waiting_for: format!(
                "{}[{offset}..{}]",
                Pubkey::from(address),
                offset as u64 + len as u64
            ),
            chain_reads: watched_now.map_or_else(
                || "unreadable".to_string(),
                |bytes| format!("0x{}", hex(bytes)),
            ),
            remaining: None,
        },
        Ok(relay_spec::WakeView::OnValueCross {
            address,
            offset,
            len,
            threshold,
            cmp,
        }) => {
            let value = watched_now.and_then(relay_spec::read_watched_value);
            WakeDetail {
                kind: "on-value-cross",
                waiting_for: format!(
                    "{}[{offset}..{}] {} {threshold}",
                    Pubkey::from(address),
                    offset as u64 + len as u64,
                    if cmp == 0 { ">=" } else { "<=" },
                ),
                chain_reads: value.map_or_else(
                    || "unreadable".to_string(),
                    |value| format!("value {value}"),
                ),
                remaining: value.map(|value| {
                    if cmp == 0 {
                        threshold as i128 - value as i128
                    } else {
                        value as i128 - threshold as i128
                    }
                }),
            }
        }
        Err(_) => WakeDetail {
            kind: "unknown",
            waiting_for: format!("wake kind {}", condition.wake_kind),
            chain_reads: String::new(),
            remaining: None,
        },
    }
}

pub fn hex(bytes: &[u8]) -> String {
    // Long ranges are rare and unhelpful in full; the head is what
    // identifies a change.
    let head: String = bytes
        .iter()
        .take(16)
        .map(|b| format!("{b:02x}"))
        .collect::<Vec<_>>()
        .join("");
    if bytes.len() > 16 {
        format!("{head}… ({} bytes)", bytes.len())
    } else {
        head
    }
}

/// One line describing where a condition got to, for list output.
pub fn verdict_line(verdict: &Verdict) -> String {
    match verdict {
        Verdict::Skipped(reason) => format!("skipped: {}", skip_reason(reason)),
        Verdict::NoWork => "no work".to_string(),
        Verdict::WouldSend {
            min_payment, units, ..
        } => format!("READY (pays {min_payment} lamports, {units} CU)"),
        Verdict::Failed { stage, error } => {
            format!("FAILED at {}: {}", stage_name(stage), first_line(error))
        }
    }
}

pub fn skip_reason(reason: &SkipReason) -> &'static str {
    match reason {
        SkipReason::NotDue => "not due",
        SkipReason::Backoff => "backoff",
        SkipReason::Inactive => "inactive",
        SkipReason::BelowMinPayment => "below min payment",
        SkipReason::ParseFailed => "parse failed",
        SkipReason::Unprofitable => "unprofitable",
        SkipReason::ContentionDelay => "contention delay",
        SkipReason::NoSafePayout => "no safe payout",
        SkipReason::ExecutorNamedSigner => "executor named a signer",
    }
}

pub fn stage_name(stage: &Stage) -> &'static str {
    match stage {
        Stage::ResolveSim => "resolve simulation",
        Stage::ExecuteSim => "execute simulation",
        Stage::Send => "send",
    }
}

fn first_line(text: &str) -> String {
    text.lines().next().unwrap_or_default().to_string()
}

pub fn verdict_json(verdict: &Verdict) -> Value {
    match verdict {
        Verdict::Skipped(reason) => json!({"state": "skipped", "reason": skip_reason(reason)}),
        Verdict::NoWork => json!({"state": "no_work"}),
        Verdict::WouldSend {
            min_payment,
            units,
            instructions,
        } => json!({
            "state": "ready",
            "min_payment": min_payment,
            "compute_units": units,
            "instructions": instructions.iter().map(|ix| json!({
                "program": ix.program_id.to_string(),
                "accounts": ix.accounts.iter().map(|a| json!({
                    "pubkey": a.pubkey.to_string(),
                    "signer": a.is_signer,
                    "writable": a.is_writable,
                })).collect::<Vec<_>>(),
                "data_len": ix.data.len(),
            })).collect::<Vec<_>>(),
        }),
        Verdict::Failed { stage, error } => {
            json!({"state": "failed", "stage": stage_name(stage), "error": error})
        }
    }
}

/// Print rows as an aligned table.
pub fn table(headers: &[&str], rows: &[Vec<String>]) {
    let widths: Vec<usize> = headers
        .iter()
        .enumerate()
        .map(|(i, header)| {
            rows.iter()
                .map(|row| row.get(i).map_or(0, |cell| cell.chars().count()))
                .chain([header.chars().count()])
                .max()
                .unwrap_or(0)
        })
        .collect();
    let line = |cells: &[String]| {
        let rendered: Vec<String> = cells
            .iter()
            .enumerate()
            .map(|(i, cell)| format!("{:<width$}", cell, width = widths[i]))
            .collect();
        println!("{}", rendered.join("  ").trim_end());
    };
    line(&headers.iter().map(|h| h.to_string()).collect::<Vec<_>>());
    println!(
        "{}",
        "-".repeat(widths.iter().sum::<usize>() + 2 * widths.len())
    );
    rows.iter().for_each(|row| line(row));
}
