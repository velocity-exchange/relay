//! Build the SBF program binaries the test suites load.
//!
//! The litesvm suites run the real programs, so they need real ELFs, and the
//! ELFs are build output rather than checked-in fixtures. This crate is a
//! dev-dependency, so its build script runs when a test target is compiled and
//! not when anything else is — someone building the turner binary needs no SBF
//! toolchain, and someone running the tests has the programs waiting.
//!
//! The recipe lives in `scripts/build-programs.sh` and stays there: the tools
//! version is load-bearing (v1.52 miscompiles both programs) and one copy of
//! that fact is enough.

use std::{path::PathBuf, process::Command};

fn main() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("test-fixtures sits one level under the repo root")
        .to_path_buf();
    let script = root.join("scripts/build-programs.sh");

    // Re-run when a program's source moves, not on every test build. The
    // script is cargo underneath and would no-op, but only after paying for a
    // process launch and a dependency scan on each of them.
    println!("cargo:rerun-if-changed={}", script.display());
    for program in ["relay", "demo-book"] {
        let dir = root.join("programs").join(program);
        println!("cargo:rerun-if-changed={}", dir.join("src").display());
        println!(
            "cargo:rerun-if-changed={}",
            dir.join("Cargo.toml").display()
        );
    }

    // A build script inherits the outer cargo's environment, and the programs
    // are a separate workspace with its own lockfile, target dir and toolchain
    // — so every one of these is either ignored or actively wrong there. The
    // wrappers are the ones that bite: under `cargo clippy` they point at
    // clippy-driver, which cannot load the SBF target specification, so the
    // nested build fails for a reason that has nothing to do with the build.
    let mut build = Command::new("bash");
    build.arg(&script);
    for leaked in [
        "CARGO",
        "CARGO_BUILD_RUSTFLAGS",
        "CARGO_BUILD_TARGET",
        "CARGO_ENCODED_RUSTFLAGS",
        "CARGO_MANIFEST_DIR",
        "CARGO_TARGET_DIR",
        "RUSTC",
        "RUSTC_WORKSPACE_WRAPPER",
        "RUSTC_WRAPPER",
        "RUSTFLAGS",
        "RUSTUP_TOOLCHAIN",
    ] {
        build.env_remove(leaked);
    }

    let status = build.status();

    match status {
        Ok(status) if status.success() => {}
        Ok(status) => panic!(
            "{} failed ({status}). The test suites load real SBF binaries, so they \
             cannot run without it.",
            script.display()
        ),
        Err(err) => panic!(
            "could not run {}: {err}. It needs `cargo-build-sbf` with \
             platform-tools v1.54 — see the README.",
            script.display()
        ),
    }
}
