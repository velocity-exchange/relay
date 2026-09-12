//! The binding gate on what the turner will put its signature behind.
//!
//! A malicious executor is harmless as long as it never receives an account
//! that signs the transaction. Rather than reason about where an account
//! list came from, the check searches the finished instruction list at
//! signing time.

use anyhow::Result;
use solana_sdk::instruction::Instruction;
use solana_sdk::message::Message;
use solana_sdk::pubkey::Pubkey;
use solana_sdk::signature::Signer;
use solana_sdk::transaction::Transaction;

use relay_chain_source::ChainSource;
use relay_spec as spec;

use super::config::COMPUTE_BUDGET_PROGRAM_ID;
use super::{metrics, Turner};

impl<S: ChainSource> Turner<S> {
    /// The authoritative safety gate, run on the exact instruction list
    /// about to be signed and sent.
    ///
    /// A malicious executor is harmless as long as it never receives an
    /// account that signs the transaction — so rather than reason about
    /// where the account list came from, search the finished list. Returns
    /// the offending program if any instruction that is not ours, and not
    /// trusted, names the fee payer. Public so an operator embedding the
    /// turner can pre-flight their own instruction lists with the same
    /// rule the turner enforces.
    ///
    /// This sits at signing time on purpose. The same check runs earlier
    /// for a clean skip reason, but instructions are rebuilt, re-priced
    /// and concatenated into packs after that point; putting the binding
    /// check here means no later transformation can reintroduce a leak.
    pub fn signer_leak(&self, ixs: &[Instruction]) -> Option<Pubkey> {
        // Compile to learn which accounts this transaction will actually
        // present as signers, rather than assuming it is just the payer.
        let message = Message::new(ixs, Some(&self.keeper.pubkey()));
        let signers = &message.account_keys[..message.header.num_required_signatures as usize];
        ixs.iter()
            .filter(|ix| {
                !is_own_guard(ix, &self.config.relay_program)
                    && ix.program_id != *COMPUTE_BUDGET_PROGRAM_ID
                    && !self.trusts(&ix.program_id)
            })
            .find(|ix| {
                // Two ways an executor can end up holding a signature.
                // First, asking for one outright — nothing legitimate
                // needs it, since executors are permissionless.
                ix.accounts.iter().any(|meta| meta.is_signer)
                    // Second, and the one that actually bites: naming an
                    // account that signs the transaction for another
                    // reason. `is_signer: false` on the meta does not help
                    // there — see `hostile_drain_succeeds_with_is_signer_false`.
                    || names_transaction_signer(ix, signers)
            })
            .map(|ix| ix.program_id)
    }

    /// Sign a transaction that is about to be submitted, refusing to sign
    /// one that would hand a signing account to an untrusted program.
    pub(super) async fn sign_for_submission(
        &self,
        ixs: &[Instruction],
    ) -> Result<(Transaction, u64)> {
        if let Some(program) = self.signer_leak(ixs) {
            metrics::FAILURES
                .with_label_values(&["signer_leak", &metrics::program_label(&program)])
                .inc();
            anyhow::bail!(
                "refusing to sign: untrusted program {program} would receive the fee payer, \
                 which signs this transaction"
            );
        }
        self.signed_tx(ixs).await
    }

    /// Sign against the shared blockhash the submitter keeps refreshed,
    /// falling back to a direct fetch when running without one. Returns
    /// the block height past which the transaction can no longer land.
    pub(super) async fn signed_tx(&self, ixs: &[Instruction]) -> Result<(Transaction, u64)> {
        let info = match self.submitter.as_ref().and_then(|s| s.cached_blockhash()) {
            Some(info) => info,
            None => self.source.latest_blockhash().await?,
        };
        Ok((
            Transaction::new_signed_with_payer(
                ixs,
                Some(&self.keeper.pubkey()),
                &[&self.keeper],
                info.hash,
            ),
            info.last_valid_block_height,
        ))
    }
}

/// Is this one of the turner's own payment guards?
///
/// The guards are the one place the fee payer legitimately appears in an
/// instruction the turner did not write itself — `begin_guard_v0` funds the
/// guard account out of it — so they are exempt from the signer check. The
/// exemption is by *instruction*, keyed on the discriminator, not by
/// program: an executor's identity comes from a resolver, which is free to
/// name the relay program like any other, and a blanket
/// `program_id == relay_program` exemption would wave that straight through
/// to signing with the fee payer in its account list.
pub(super) fn is_own_guard(ix: &Instruction, relay_program: &Pubkey) -> bool {
    ix.program_id == *relay_program
        && ix.data.first_chunk::<8>().is_some_and(|disc| {
            *disc == spec::BEGIN_GUARD_V0_DISCRIMINATOR
                || *disc == spec::ASSERT_PAID_V0_DISCRIMINATOR
        })
}

/// Does this instruction name any account from `signers`?
///
/// The distinction that matters: an executor **must** name the account it
/// pays, and that is fine — a non-signing account can only be credited.
/// What it must never name is an account that signs the transaction,
/// because signer status on Solana is transaction-global. A compiled
/// message has no per-instruction signer flag at all: `is_signer` is
/// decided by an account's position in the message's signer section, so
/// setting `AccountMeta { is_signer: false }` on an account that signs
/// elsewhere in the transaction demotes nothing. The executor receives it
/// as a signer and can CPI a System transfer to drain it.
///
/// So the rule is not "don't name the payee", it is "don't name a
/// signer" — which, since the turner's only signer is its fee payer, is
/// why payment goes to a separate non-signing payout account.
pub fn names_transaction_signer(ix: &Instruction, signers: &[Pubkey]) -> bool {
    ix.accounts
        .iter()
        .any(|meta| signers.contains(&meta.pubkey))
}
