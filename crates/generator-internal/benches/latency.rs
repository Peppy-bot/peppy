//! Roundtrip latency benchmark between two real peppy nodes.
//!
//! Spins up a Rust **driver** node and a **responder** node (Rust or Python),
//! both launched the way peppy launches nodes (NodeBuilder + PEPPY_RUNTIME_CONFIG
//! with a generated peppygen library present). The driver times each topic /
//! service roundtrip on a single clock and reports the distribution back over a
//! raw control service.
//!
//! Prints a self-explanatory summary table — p50 / p90 / mean per scenario, the
//! p90 ceiling, and the change in p90 versus the previous run **on this
//! machine** (baselines are keyed by /etc/machine-id and stored under the
//! machine-local `target/`, so numbers are never compared across machines).
//!
//! Run with:
//!     cargo bench -p generator --bench latency
//!     cargo bench -p generator --bench latency -- rust    # filter scenarios
//! Set PEPPY_LATENCY_SKIP_PYTHON=1 to skip the Python responder (no `uv`).

#[path = "../tests/latency/harness.rs"]
mod harness;
#[path = "../tests/helpers.rs"]
mod helpers;

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use harness::{ALL_SCENARIOS, DEFAULT_SAMPLES, DEFAULT_WARMUP, Lang, Transport, ceiling_ms};

/// One measured scenario, ready to render.
struct Row {
    name: String,
    p50: Duration,
    p90: Duration,
    mean: Duration,
    ceiling_ms: u64,
    prev_p90_ns: Option<u64>,
}

fn main() {
    // `cargo bench -- <filter>` passes a substring; ignore flag-like args
    // (e.g. the `--bench` libtest harnesses inject).
    let filter: Option<String> = std::env::args().skip(1).find(|arg| !arg.starts_with('-'));
    let skip_python = std::env::var("PEPPY_LATENCY_SKIP_PYTHON").is_ok();

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(4)
        .enable_all()
        .build()
        .expect("bench tokio runtime");

    let previous = load_baseline();
    // Start from the previous baseline so scenarios we skip this run keep their
    // recorded numbers instead of being dropped.
    let mut current = previous.clone();
    let mut rows: Vec<Row> = Vec::new();

    // Group by language so each responder is spawned once and reused for both
    // transports.
    for &lang in &[Lang::Rust, Lang::Python] {
        if lang == Lang::Python && skip_python {
            continue;
        }
        let transports: Vec<Transport> = ALL_SCENARIOS
            .iter()
            .filter(|(l, _)| *l == lang)
            .map(|(_, t)| *t)
            .filter(|t| selected(&filter, lang, *t))
            .collect();
        if transports.is_empty() {
            continue;
        }

        let scenario = runtime.block_on(harness::start_scenario(lang));
        for transport in transports {
            let stats = runtime.block_on(scenario.run(transport, DEFAULT_WARMUP, DEFAULT_SAMPLES));
            let name = format!("{}/{}", lang.as_str(), transport.as_str());
            let prev_p90_ns = previous.get(&name).map(|s| s.p90_ns);
            current.insert(
                name.clone(),
                StoredStats {
                    p50_ns: stats.p50().as_nanos() as u64,
                    p90_ns: stats.p90().as_nanos() as u64,
                    mean_ns: stats.mean().as_nanos() as u64,
                },
            );
            rows.push(Row {
                name,
                p50: stats.p50(),
                p90: stats.p90(),
                mean: stats.mean(),
                ceiling_ms: ceiling_ms(lang, transport),
                prev_p90_ns,
            });
        }
        runtime.block_on(scenario.shutdown());
    }

    print_table(&rows);
    save_baseline(&current);
}

/// Whether a scenario matches the optional `cargo bench -- <filter>` substring,
/// tested against the `lang/transport` id.
fn selected(filter: &Option<String>, lang: Lang, transport: Transport) -> bool {
    match filter {
        None => true,
        Some(needle) => {
            let id = format!("{}/{}", lang.as_str(), transport.as_str());
            id.contains(needle.as_str())
        }
    }
}

// ---------------------------------------------------------------------------
// Rendering
// ---------------------------------------------------------------------------

fn print_table(rows: &[Row]) {
    if rows.is_empty() {
        println!("\nno scenarios matched the filter");
        return;
    }

    let headers = [
        "scenario", "p50", "p90", "mean", "ceiling", "prev p90", "Δp90", "status",
    ];
    let mut cells: Vec<[String; 8]> = Vec::with_capacity(rows.len());
    for row in rows {
        let (prev, delta) = match row.prev_p90_ns {
            Some(prev_ns) => (
                fmt_duration(Duration::from_nanos(prev_ns)),
                fmt_delta(row.p90.as_nanos() as u64, prev_ns),
            ),
            None => ("—".to_string(), "—".to_string()),
        };
        let status = if row.p90 <= Duration::from_millis(row.ceiling_ms) {
            "✓"
        } else {
            "✗ OVER"
        };
        cells.push([
            row.name.clone(),
            fmt_duration(row.p50),
            fmt_duration(row.p90),
            fmt_duration(row.mean),
            format!("{}ms", row.ceiling_ms),
            prev,
            delta,
            status.to_string(),
        ]);
    }

    let mut widths = [0usize; 8];
    for (i, header) in headers.iter().enumerate() {
        widths[i] = header.chars().count();
    }
    for cell in &cells {
        for (i, value) in cell.iter().enumerate() {
            widths[i] = widths[i].max(value.chars().count());
        }
    }

    let render = |cell: &[String; 8]| {
        cell.iter()
            .enumerate()
            .map(|(i, value)| pad(value, widths[i]))
            .collect::<Vec<_>>()
            .join("  ")
    };

    println!(
        "\nRoundtrip latency (two real peppy nodes, {} samples/scenario)",
        DEFAULT_SAMPLES
    );
    let header_row: [String; 8] = headers.map(|h| h.to_string());
    println!("{}", render(&header_row));
    println!(
        "{}",
        "-".repeat(widths.iter().sum::<usize>() + 2 * (widths.len() - 1))
    );
    for cell in &cells {
        println!("{}", render(cell));
    }
    println!(
        "\nΔp90 vs the previous run on this machine (+ = slower, - = faster). \
         status = p90 within ceiling. Ceilings via PEPPY_LATENCY_MAX_MS_<LANG>_<TRANSPORT>."
    );
}

/// Pad to width, accounting for the wide `µ`/`Δ`/`✓` glyphs (counted as one).
fn pad(value: &str, width: usize) -> String {
    let len = value.chars().count();
    if len >= width {
        value.to_string()
    } else {
        format!("{value}{}", " ".repeat(width - len))
    }
}

fn fmt_duration(duration: Duration) -> String {
    let nanos = duration.as_nanos();
    if nanos < 1_000 {
        format!("{nanos}ns")
    } else if nanos < 1_000_000 {
        format!("{:.0}µs", nanos as f64 / 1_000.0)
    } else {
        format!("{:.2}ms", nanos as f64 / 1_000_000.0)
    }
}

fn fmt_delta(now_ns: u64, prev_ns: u64) -> String {
    if prev_ns == 0 {
        return "—".to_string();
    }
    let pct = (now_ns as f64 - prev_ns as f64) / prev_ns as f64 * 100.0;
    format!("{pct:+.1}%")
}

// ---------------------------------------------------------------------------
// Same-machine baseline persistence
// ---------------------------------------------------------------------------

#[derive(Clone, Copy)]
struct StoredStats {
    p50_ns: u64,
    p90_ns: u64,
    mean_ns: u64,
}

/// Stable per-machine id so a baseline is never compared across machines.
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

/// `<workspace>/target/latency-bench/` — `target/` is already machine-local and
/// git-ignored; the machine-id in the filename makes same-machine reuse explicit.
fn baseline_path() -> PathBuf {
    let target = std::env::var_os("CARGO_TARGET_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("..")
                .join("..")
                .join("target")
        });
    target
        .join("latency-bench")
        .join(format!("baseline-{}.tsv", machine_id()))
}

fn load_baseline() -> BTreeMap<String, StoredStats> {
    let mut map = BTreeMap::new();
    let Ok(contents) = fs::read_to_string(baseline_path()) else {
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

fn save_baseline(stats: &BTreeMap<String, StoredStats>) {
    let path = baseline_path();
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
    if let Err(err) = fs::write(&path, out) {
        eprintln!(
            "warning: failed to save latency baseline to {}: {err}",
            path.display()
        );
    }
}
