//! Build-environment readers.

/// Returns the Rust target triple for the current build.
///
/// Must be called from a build script. It reads the `TARGET` environment
/// variable, which cargo only sets while running `build.rs`. The read
/// `expect()`s on purpose: the variable's absence means the function was
/// called outside that context, which is a programming error rather than a
/// recoverable runtime condition.
pub fn build_target_triple() -> String {
    std::env::var("TARGET")
        .expect("TARGET not set; build_target_triple must be called from a build script")
}
