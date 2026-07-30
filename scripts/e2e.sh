#!/usr/bin/env bash
# End-to-end test against a real solana-test-validator: deploys both
# programs, runs a crank turner, posts orders, and checks that expiry,
# eviction, and crossing all get cranked.
#
# Needs `solana-test-validator` on PATH (ships with the Solana CLI).
set -euo pipefail
cd "$(dirname "$0")/.."

if ! command -v solana-test-validator >/dev/null; then
  echo "solana-test-validator not found on PATH (install the Solana CLI)" >&2
  exit 1
fi

./scripts/build-programs.sh >/dev/null
# The CLI is exercised by one scenario here, by path from the shared target
# directory, so it has to exist before the tests run.
cargo build -p relay-cli >/dev/null

# --nocapture so the validator's startup wait and turner progress are
# visible; the test owns the validator's lifetime and kills it on drop.
cargo test -p relay-crank-turner --test validator_e2e -- --ignored --nocapture --test-threads=1
