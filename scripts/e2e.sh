#!/usr/bin/env bash
# End-to-end test against a real solana-test-validator: deploys both
# programs, runs a crank turner, posts orders, and checks that expiry,
# eviction, and crossing all get cranked.
#
# Needs `solana-test-validator` 4.2 or later on PATH: the turner signs
# transaction v1, which the `txv1` feature gates.
set -euo pipefail
cd "$(dirname "$0")/.."

SOLANA_TEST_VALIDATOR="${SOLANA_TEST_VALIDATOR:-solana-test-validator}"

if ! command -v "$SOLANA_TEST_VALIDATOR" >/dev/null; then
  echo "$SOLANA_TEST_VALIDATOR not found on PATH (install the Solana CLI)" >&2
  exit 1
fi

# A validator without `txv1` cannot parse a v1 transaction. It reports that
# as a deserialize error on send, three hops from the cause, and only after
# a simulation that looked fine. Say it here instead.
validator_version="$("$SOLANA_TEST_VALIDATOR" --version | awk '{print $2}')"
validator_major="${validator_version%%.*}"
validator_minor="${validator_version#*.}"
validator_minor="${validator_minor%%.*}"
if [ "$validator_major" -lt 4 ] ||
  { [ "$validator_major" -eq 4 ] && [ "$validator_minor" -lt 2 ]; }; then
  echo "solana-test-validator $validator_version is too old: the turner signs" >&2
  echo "transaction v1, which needs agave 4.2 or later. Run:" >&2
  echo "  agave-install init 4.2.2" >&2
  echo "or set SOLANA_TEST_VALIDATOR to a 4.2+ binary." >&2
  exit 1
fi

./scripts/build-programs.sh >/dev/null
# The CLI is exercised by one scenario here, by path from the shared target
# directory, so it has to exist before the tests run.
cargo build -p relay-cli >/dev/null

# --nocapture so the validator's startup wait and turner progress are
# visible; the test owns the validator's lifetime and kills it on drop.
cargo test -p relay-crank-turner --test validator_e2e -- --ignored --nocapture --test-threads=1
