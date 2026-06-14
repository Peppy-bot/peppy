//! Same-machine baseline persistence.
//!
//! Baselines are keyed by a stable machine id and stored as a small TSV under the
//! machine-local `target/` directory, so latency numbers are never compared
//! across machines. Each line is `name<TAB>p50_ns<TAB>p90_ns<TAB>mean_ns`.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::fs;
use std::io;
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
/// then `"unknown"`. The result is sanitized to be filesystem-safe. Internal: it
/// only ever names the baseline file built by [`baseline_path`].
fn machine_id() -> String {
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
    path_in(&target_dir(), subdir, &machine_id())
}

/// The machine-local `target/` directory baselines live under: `$CARGO_TARGET_DIR`
/// when set, else the workspace `target/` resolved relative to this crate.
fn target_dir() -> PathBuf {
    std::env::var_os("CARGO_TARGET_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            // CARGO_MANIFEST_DIR is `<repo>/crates/latency-report`; the workspace
            // `target/` is two levels up.
            Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("..")
                .join("..")
                .join("target")
        })
}

/// Pure path join, split out so the layout is testable without reading the
/// environment: `<target>/<subdir>/baseline-<machine_id>.tsv`.
fn path_in(target: &Path, subdir: &str, machine_id: &str) -> PathBuf {
    target
        .join(subdir)
        .join(format!("baseline-{machine_id}.tsv"))
}

/// Load a baseline TSV. A missing or malformed file yields an empty map;
/// individual malformed lines are skipped.
pub fn load(path: &Path) -> BTreeMap<String, StoredStats> {
    match fs::read_to_string(path) {
        Ok(contents) => parse_baseline(&contents),
        Err(_) => BTreeMap::new(),
    }
}

/// Write a baseline TSV, creating the parent directory if needed. Returns the
/// I/O error so the caller decides how to surface it; a baseline is best-effort,
/// so callers typically log and continue.
pub fn save(path: &Path, stats: &BTreeMap<String, StoredStats>) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        // Best-effort: if the directory genuinely can't be created, the write
        // below surfaces the real error.
        let _ = fs::create_dir_all(parent);
    }
    fs::write(path, render_baseline(stats))
}

/// Parse a baseline TSV body into stats, skipping malformed lines (a field count
/// other than 4, or an unparseable number). Pure, so the skip rules are
/// unit-testable without the filesystem. Each line is
/// `name<TAB>p50_ns<TAB>p90_ns<TAB>mean_ns`.
fn parse_baseline(contents: &str) -> BTreeMap<String, StoredStats> {
    let mut map = BTreeMap::new();
    for line in contents.lines() {
        let fields: Vec<&str> = line.split('\t').collect();
        if fields.len() != 4 {
            continue;
        }
        let (Ok(p50_ns), Ok(p90_ns), Ok(mean_ns)) =
            (fields[1].parse(), fields[2].parse(), fields[3].parse())
        else {
            continue;
        };
        map.insert(
            fields[0].to_string(),
            StoredStats {
                p50_ns,
                p90_ns,
                mean_ns,
            },
        );
    }
    map
}

/// Render stats as a baseline TSV body, the inverse of [`parse_baseline`].
fn render_baseline(stats: &BTreeMap<String, StoredStats>) -> String {
    let mut out = String::new();
    for (name, s) in stats {
        // Writing into the buffer avoids a temporary String per line; writing to
        // a String never fails, so the result is safe to discard.
        let _ = writeln!(out, "{name}\t{}\t{}\t{}", s.p50_ns, s.p90_ns, s.mean_ns);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stat(p50_ns: u64, p90_ns: u64, mean_ns: u64) -> StoredStats {
        StoredStats {
            p50_ns,
            p90_ns,
            mean_ns,
        }
    }

    fn one(name: &str, s: StoredStats) -> BTreeMap<String, StoredStats> {
        let mut map = BTreeMap::new();
        map.insert(name.to_string(), s);
        map
    }

    #[test]
    fn machine_id_is_filesystem_safe() {
        let id = machine_id();
        assert!(!id.is_empty());
        assert!(id.chars().all(|c| c.is_ascii_alphanumeric() || c == '_'));
    }

    #[test]
    fn parse_render_round_trip_in_memory() {
        let stats = one("rust/topic", stat(5_120_000, 7_890_000, 5_234_567));
        assert_eq!(parse_baseline(&render_baseline(&stats)), stats);
    }

    #[test]
    fn parse_empty_body_is_empty() {
        assert!(parse_baseline("").is_empty());
    }

    #[test]
    fn parse_skips_malformed_lines() {
        let body = concat!(
            "good\t1\t2\t3\n",
            "too\tfew\n",         // 2 fields: skipped
            "five\t1\t2\t3\t4\n", // 5 fields: skipped
            "bad\tx\t2\t3\n",     // p50 unparseable: skipped
            "also_good\t10\t20\t30\n",
        );
        let parsed = parse_baseline(body);
        assert_eq!(parsed.len(), 2);
        assert_eq!(parsed["good"], stat(1, 2, 3));
        assert_eq!(parsed["also_good"], stat(10, 20, 30));
    }

    #[test]
    fn round_trips_through_disk() {
        // Unique per process so concurrent `cargo test` invocations never collide
        // on the path.
        let dir = std::env::temp_dir().join(format!(
            "latency-report-test-{}-{}",
            machine_id(),
            std::process::id()
        ));
        let _ = fs::create_dir_all(&dir);
        let path = dir.join("baseline-test.tsv");

        let stats = one("rust/topic", stat(5_120_000, 7_890_000, 5_234_567));
        save(&path, &stats).expect("save baseline");
        let loaded = load(&path);
        assert_eq!(loaded, stats);

        let _ = fs::remove_file(&path);
        let _ = fs::remove_dir(&dir);
    }

    #[test]
    fn missing_file_is_empty() {
        let path = Path::new("/nonexistent/latency-report/baseline.tsv");
        assert!(load(path).is_empty());
    }

    #[test]
    fn path_in_lays_out_target_subdir_and_machine_file() {
        assert_eq!(
            path_in(Path::new("/tmp/target"), "stack-benchmark", "abc123"),
            Path::new("/tmp/target/stack-benchmark/baseline-abc123.tsv")
        );
    }

    #[test]
    fn baseline_path_uses_subdir_and_machine_keyed_file() {
        let path = baseline_path("latency-bench");
        assert!(path.to_string_lossy().contains("latency-bench"));
        let file = path.file_name().unwrap().to_string_lossy();
        assert!(file.starts_with("baseline-") && file.ends_with(".tsv"));
    }
}
