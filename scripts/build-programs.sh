#!/usr/bin/env bash
# SBF-build both programs into programs/target/deploy/ (the fixture path the
# litesvm test suites load from).
set -euo pipefail
cd "$(dirname "$0")/../programs"

if [[ "$(uname)" == "Darwin" && -z "${SDKROOT:-}" ]]; then
  export SDKROOT="$(xcrun --show-sdk-path)"
fi

cargo-build-sbf --tools-version v1.52 --manifest-path relay/Cargo.toml
cargo-build-sbf --tools-version v1.52 --manifest-path demo-book/Cargo.toml
ls -la target/deploy/*.so
