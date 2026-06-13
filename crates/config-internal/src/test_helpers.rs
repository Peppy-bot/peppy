mod git;
mod templates;

pub use git::*;
pub use templates::*;

/// Asserts that all given patterns are present in the rendered output.
pub fn assert_contains_all(rendered: &str, patterns: &[&str]) {
    for pattern in patterns {
        if !rendered.contains(pattern) {
            eprintln!("rendered output:\n{}", rendered);
            panic!("expected to find: {:?}", pattern);
        }
    }
}

use std::path::{Path, PathBuf};
use std::time::Duration;

/// Root for per-test scratch directories: `<base>/test-tmp`, where base is
/// `PEPPY_HOME` when set and non-empty (the CI per-run redirect), otherwise
/// `$HOME/.peppy`.
///
/// The `$HOME` fallback is deliberate so two constraints documented at the call
/// sites still hold for local dev:
/// 1. On macOS, Lima 2.0+ only mounts `~` into the guest VM, so node paths must
///    live under `$HOME` to be visible inside the VM (system temp such as
///    `/var/folders/...` is inaccessible).
/// 2. On Linux dev/CI machines `/tmp` is frequently a size-quota'd `tmpfs`;
///    building a node there (the cargo `target/` alone is ~2 GB) trips the
///    per-user quota. `$HOME` lives on the roomy backing disk instead.
///
/// This is intentionally NOT keyed off `app_env()`/`temp_dir()`, so it never
/// lands on `/tmp` tmpfs. Only CI, which sets `PEPPY_HOME` on a roomy disk,
/// redirects it.
///
/// Every scratch dir handed out from here should be a `TempDir`, so it is
/// removed when its guard drops; normal completion and panics both clean up, and
/// nothing is carried over to the next run. As a backstop for runs that were
/// hard-killed before their guards could run, the first call per test binary
/// reclaims leftovers older than [`STALE_TEST_TMP_AGE`]; that age floor keeps
/// concurrently-running test binaries from deleting each other's live dirs.
pub fn test_tmp_root() -> PathBuf {
    let base =
        match std::env::var_os(crate::internal::consts::PEPPY_HOME_ENV).filter(|v| !v.is_empty()) {
            Some(home) => PathBuf::from(home),
            None => PathBuf::from(std::env::var("HOME").expect("HOME must be set")).join(".peppy"),
        };
    let root = base.join("test-tmp");
    std::fs::create_dir_all(&root).expect("create test-tmp root");

    static RECLAIM: std::sync::Once = std::sync::Once::new();
    RECLAIM.call_once(|| reclaim_stale_test_tmp(&root));

    root
}

/// Scratch older than this is treated as abandoned by an earlier run and is
/// safe to delete. Far longer than any real test run (which finishes in
/// minutes), so an in-flight run is never affected.
const STALE_TEST_TMP_AGE: Duration = Duration::from_secs(60 * 60);

/// Best-effort removal of stale leftovers directly under `root`. Errors are
/// ignored on purpose: reclaiming scratch must never fail a test.
fn reclaim_stale_test_tmp(root: &Path) {
    let Ok(entries) = std::fs::read_dir(root) else {
        return;
    };
    let now = std::time::SystemTime::now();
    for entry in entries.flatten() {
        let Ok(metadata) = entry.metadata() else {
            continue;
        };
        let too_old = metadata
            .modified()
            .ok()
            .and_then(|modified| now.duration_since(modified).ok())
            .is_some_and(|age| age >= STALE_TEST_TMP_AGE);
        if !too_old {
            continue;
        }
        if metadata.is_dir() {
            let _ = std::fs::remove_dir_all(entry.path());
        } else {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}
