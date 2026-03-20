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

/// Acquires a cross-process file lock to serialize container image pulls
/// from ECR Public. Prevents concurrent requests that trigger the 1 req/sec
/// burst rate limit.
///
/// Hold the returned guard until the container build completes.
/// The lock is released when the [`std::fs::File`] is dropped.
pub fn container_build_lock() -> std::fs::File {
    use fs2::FileExt;

    let lock_path = std::env::temp_dir().join("peppy-container-build.lock");
    let file =
        std::fs::File::create(&lock_path).expect("failed to create container build lock file");
    file.lock_exclusive()
        .expect("failed to acquire container build lock");
    file
}
