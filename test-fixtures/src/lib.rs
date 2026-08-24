//! Paths to the SBF program binaries the litesvm test suites load.
//!
//! Depending on this crate is what builds them: its build script runs
//! `scripts/build-programs.sh`, so a test that names a path here finds a
//! binary there. Naming the paths in one place also keeps a program rename
//! from being a hunt through five test files.

/// The relay program.
pub const RELAY_SO: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../programs/target/deploy/relay.so"
);

/// demo-book, the reference target program the suites crank against.
pub const DEMO_BOOK_SO: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../programs/target/deploy/demo_book.so"
);

/// Read one of the binaries above.
///
/// Panics rather than returning a result: the build script already guaranteed
/// the file, so its absence is a broken build rather than a condition a test
/// can do anything about.
pub fn elf(path: &str) -> Vec<u8> {
    std::fs::read(path)
        .unwrap_or_else(|err| panic!("{path} missing after the fixture build: {err}"))
}
