#!/usr/bin/env bash
# The `relay` CLI against a real solana-test-validator: registry scan,
# rendering, and the diagnoses an operator relies on.
set -euo pipefail
cd "$(dirname "$0")/.."

if ! command -v solana-test-validator >/dev/null; then
  echo "solana-test-validator not found on PATH (install the Solana CLI)" >&2
  exit 1
fi

./scripts/build-programs.sh >/dev/null
cargo test -p relay-cli -- --ignored --nocapture --test-threads=1
