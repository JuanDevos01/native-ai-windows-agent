//! Compile-time build metadata so the running binary can report exactly which
//! build it is (semver + git hash + build time).

/// Crate semver version (from Cargo.toml).
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Short git commit hash at build time (may be `"unknown"` or carry a `-dirty` suffix).
pub const GIT_HASH: &str = env!("METIS_GIT_HASH");

/// UTC timestamp of when the binary was built.
pub const BUILD_TIME: &str = env!("METIS_BUILD_TIME");

/// One-line human-readable version string (`&'static str`),
/// e.g. `v0.1.0 (a1b2c3d, built 2026-06-02 23:59:00 UTC)`.
pub const VERSION_LINE: &str = concat!(
    "v",
    env!("CARGO_PKG_VERSION"),
    " (",
    env!("METIS_GIT_HASH"),
    ", built ",
    env!("METIS_BUILD_TIME"),
    ")"
);

/// One-line human-readable version string.
pub fn version_line() -> String {
    VERSION_LINE.to_string()
}
