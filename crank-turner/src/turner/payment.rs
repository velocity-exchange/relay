//! What a crank pays, who it pays, and the guards that hold it to that.

use anyhow::Result;
use solana_sdk::instruction::{AccountMeta, Instruction};
use solana_sdk::pubkey::Pubkey;
use solana_sdk::signature::{Signature, Signer};

use relay_chain_source::ChainSource;
use relay_spec as spec;

use super::config::{SYNC_NATIVE_TAG, SYSTEM_PROGRAM_ID, TOKEN_PROGRAM_ID};
use super::Turner;

impl<S: ChainSource> Turner<S> {
    /// Bracket an executor with the payment guards (or pass it through
    /// untouched when guarding is disabled).
    pub(super) fn guarded(
        &self,
        executor: Instruction,
        payout: Pubkey,
        program: &Pubkey,
        min_payment: u64,
    ) -> Vec<Instruction> {
        // Trusted programs skip the guards entirely: two fewer
        // instructions, their compute, and their bytes.
        if !self.config.guard_payments || self.trusts(program) {
            return vec![executor];
        }
        let payer = self.keeper.pubkey();
        let nonce = self.config.guard_nonce;
        let guard = self.guard_address(payout, nonce);
        vec![
            Instruction {
                program_id: self.config.relay_program,
                accounts: vec![
                    AccountMeta::new(payer, true),
                    AccountMeta::new_readonly(payout, false),
                    AccountMeta::new(guard, false),
                    AccountMeta::new_readonly(SYSTEM_PROGRAM_ID, false),
                ],
                data: spec::encode_begin_guard_v0_data(nonce).to_vec(),
            },
            executor,
            Instruction {
                program_id: self.config.relay_program,
                accounts: vec![
                    AccountMeta::new_readonly(payout, false),
                    AccountMeta::new(guard, false),
                ],
                data: spec::encode_assert_paid_v0_data(min_payment, nonce).to_vec(),
            },
        ]
    }

    /// Roll a wrapped-SOL payout's accumulated lamports into its token
    /// balance, as a standalone transaction.
    ///
    /// Deliberately *not* part of a crank. Payment arrives as lamports and
    /// the guard measures lamports, so nothing about correctness or safety
    /// waits on this — it only decides when the proceeds become spendable
    /// as tokens. Bundling it into every crank would buy an instruction,
    /// its compute, and its bytes on every single transaction to keep a
    /// number fresh that nobody reads in between. Call it on a timer.
    pub async fn sync_payout(&self) -> Result<Option<Signature>> {
        let Some(payout) = self.config.payout else {
            return Ok(None);
        };
        if !self.config.sync_native_payout {
            return Ok(None);
        }
        let sync = Instruction {
            program_id: *TOKEN_PROGRAM_ID,
            accounts: vec![AccountMeta::new(payout, false)],
            data: vec![SYNC_NATIVE_TAG],
        };
        let units = self
            .measure(std::slice::from_ref(&sync))
            .await
            .unwrap_or(10_000);
        let config = self.tx_config(units, self.priority_fee());
        let (tx, expiry) = self
            .sign_for_submission(std::slice::from_ref(&sync), config)
            .await?;
        let signature = self.submit(tx, *TOKEN_PROGRAM_ID, 0, expiry).await?;
        Ok(Some(signature))
    }

    /// A payout account's guard PDA for a given nonce.
    pub fn guard_address(&self, payout: Pubkey, nonce: u8) -> Pubkey {
        Pubkey::find_program_address(
            &[spec::GUARD_SEED, payout.as_ref(), &[nonce]],
            &self.config.relay_program,
        )
        .0
    }

    /// Slots to hold a program's cranks back by. Zero without a submitter,
    /// which is also the no-feedback case: nothing has observed a lost race.
    pub(super) fn contention_delay(&self, program: &Pubkey) -> u64 {
        self.submitter
            .as_ref()
            .map_or(0, |submitter| submitter.lag_for(program))
    }

    /// Is this program exempt from guards and payout separation?
    pub(super) fn trusts(&self, program: &Pubkey) -> bool {
        self.config.trusted_programs.contains(program)
    }

    /// Where a given program's executor should pay.
    ///
    /// Trusted programs may pay the fee payer directly. Untrusted ones
    /// must pay a configured account that never signs — otherwise there is
    /// no safe answer and the condition is skipped.
    pub(super) fn payout_for(&self, program: &Pubkey) -> Option<Pubkey> {
        match self.config.payout {
            Some(payout) => Some(payout),
            None => self.trusts(program).then(|| self.keeper.pubkey()),
        }
    }

    /// The least this turner will accept for landing a crank of `units`:
    /// what the transaction costs it, plus whatever margin the operator set.
    ///
    /// The guard exists to stop a target program paying less than it said it
    /// would. Held to the program's own advertised figure it never protected
    /// the turner at all — a crank could advertise a lamport, land, and leave
    /// the fee unpaid. Held to this, an underpaying crank fails in simulation
    /// and is skipped before it costs anything.
    ///
    /// The base fee is per signature and a crank carries one: the turner's.
    pub(super) fn required_payment(&self, units: u32) -> u64 {
        required_payment(
            self.priority_fee(),
            units,
            self.config.crank_margin_lamports,
        )
    }

    /// Priority fee the submitter observed, clamped to the configured
    /// ceiling. Zero when no submitter is attached.
    pub(super) fn priority_fee(&self) -> u64 {
        self.submitter
            .as_ref()
            .map(|s| s.priority_fee())
            .unwrap_or(0)
            .min(self.config.max_priority_fee)
    }
}

/// Lamports a crank of `units` must pay for a turner bidding `price_per_unit`
/// micro-lamports to be whole, plus `margin`.
///
/// Free of the turner so it can be reasoned about — and tested — on its own.
/// Rounded up: a payment short by a lamport buys nothing.
pub(super) fn required_payment(price_per_unit: u64, units: u32, margin: u64) -> u64 {
    /// Lamports the runtime charges per signature; a crank carries one, the
    /// turner's own fee payer.
    const BASE_FEE_LAMPORTS: u64 = 5_000;
    BASE_FEE_LAMPORTS
        .saturating_add(priority_fee_lamports(price_per_unit, units))
        .saturating_add(margin)
}

/// The priority fee in lamports for `units` compute units bid at
/// `price_per_unit` micro-lamports each.
///
/// A v1 message states the priority fee as a total in lamports, while the
/// RPC reports a rate per compute unit. One function does the conversion so
/// the fee the turner bids and the fee it charges the crank cannot drift
/// apart. Rounded up, because the runtime charges whole lamports.
pub(super) fn priority_fee_lamports(price_per_unit: u64, units: u32) -> u64 {
    let fee = (price_per_unit as u128)
        .saturating_mul(u128::from(units))
        .div_ceil(1_000_000);
    u64::try_from(fee).unwrap_or(u64::MAX)
}
