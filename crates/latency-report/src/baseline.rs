//! Same-machine baseline persistence.
//!
//! Baselines are keyed by a stable machine id and stored as a small TSV under the
//! machine-local `target/` directory, so latency numbers are never compared
//! across machines. Each line is `name<TAB>p50_ns<TAB>p90_ns<TAB>mean_ns`.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

/// Stored percentiles for one scenario / interface, all in nanoseconds.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct StoredStats {
    pub p50_ns: u64,
    pub p90_ns: u64,
    pub mean_ns: u64,
}

/// Stable per-machine id (so a baseline is never compared across machines).
/// Falls back through `/etc/machine-id`, `/var/lib/dbus/machine-id`, `$HOSTNAME`,
/// then `"unknown"`. The result is sanitized to be filesystem-safe.
pub fn machine_id() -> String {
    for path in ["/etc/machine-id", "/var/lib/dbus/machine-id"] {
        if let Ok(contents) = fs::read_to_string(path) {
            let trimmed = contents.trim();
            if !trimmed.is_empty() {
                return sanitize(trimmed);
            }
        }
    }
    let host = std::env::var("HOSTNAME").unwrap_or_default();
    if host.is_empty() {
        "unknown".to_string()
    } else {
        sanitize(&host)
    }
}

fn sanitize(value: &str) -> String {
    value
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .collect()
}

/// `<target>/<subdir>/baseline-<machine_id>.tsv`. `target/` is already
/// machine-local and git-ignored; the machine-id in the filename makes
/// same-machine reuse explicit. `subdir` separates unrelated baselines (e.g.
/// `latency-bench` vs `stack-benchmark`).
pub fn baseline_path(subdir: &str) -> PathBuf {
    let target = std::env::var_os("CARGO_TARGET_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            // CARGO_MANIFEST_DIR is `<repo>/crates/latency-report`; the workspace
            // `target/` is two levels up.
            Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("..")
                .join("..")
                .join("target")
        });
    target
        .join(subdir)
        .join(format!("baseline-{}.tsv", machine_id()))
}

/// Load a baseline TSV. A missing or malformed file yields an empty map;
/// individual malformed lines are skipped.
pub fn load(path: &Path) -> BTreeMap<String, StoredStats> {
    let mut map = BTreeMap::new();
    let Ok(contents) = fs::read_to_string(path) else {
        return map;
    };
    for line in contents.lines() {
        let fields: Vec<&str> = line.split('\t').collect();
        if fields.len() != 4 {
            continue;
        }
        let (Ok(p50), Ok(p90), Ok(mean)) =
            (fields[1].parse(), fields[2].parse(), fields[3].parse())
        else {
            continue;
        };
        map.insert(
            fields[0].to_string(),
            StoredStats {
                p50_ns: p50,
                p90_ns: p90,
                mean_ns: mean,
            },
        );
    }
    map
}

/// Write a baseline TSV, creating the parent directory if needed. Failures are
/// reported on stderr and otherwise ignored (a baseline is best-effort).
pub fn save(path: &Path, stats: &BTreeMap<String, StoredStats>) {
    if let Some(parent) = path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    let mut out = String::new();
    for (name, s) in stats {
        out.push_str(&format!(
            "{}\t{}\t{}\t{}\n",
            name, s.p50_ns, s.p90_ns, s.mean_ns
        ));
    }
    if let Err(err) = fs::write(path, out) {
        eprintln!(
            "warning: failed to save latency baseline to {}: {err}",
            path.display()
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn machine_id_is_filesystem_safe() {
        let id = machine_id();
        assert!(!id.is_empty());
        assert!(id.chars().all(|c| c.is_ascii_alphanumeric() || c == '_'));
    }

    #[test]
    fn round_trips_through_disk() {
        let dir = std::env::temp_dir().join(format!("latency-report-test-{}", machine_id()));
        let _ = fs::create_dir_all(&dir);
        let path = dir.join("baseline-test.tsv");

        let mut stats = BTreeMap::new();
        stats.insert(
            "rust/topic".to_string(),
            StoredStats {
                p50_ns: 5_120_000,
                p90_ns: 7_890_000,
                mean_ns: 5_234_567,
            },
        );
        save(&path, &stats);
        let loaded = load(&path);
        assert_eq!(loaded, stats);
        let _ = fs::remove_file(&path);
    }

    #[test]
    fn missing_file_is_empty() {
        let path = Path::new("/nonexistent/latency-report/baseline.tsv");
        assert!(load(path).is_empty());
    }
}
