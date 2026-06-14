//! Roundtrip latency benchmark between two real peppy nodes.
//!
//! Spins up a Rust **driver** node and a **responder** node (Rust or Python),
//! both launched the way peppy launches nodes (NodeBuilder + PEPPY_RUNTIME_CONFIG
//! with a generated peppygen library present). The driver times each topic /
//! service roundtrip on a single clock and reports the distribution back over a
//! raw control service.
//!
//! Prints a self-explanatory summary table — p50 / p90 / mean per scenario, the
//! ceiling, and the change in the **median (p50)** versus the previous run **on
//! this machine** (baselines are keyed by /etc/machine-id and stored under the
//! machine-local `target/`, so numbers are never compared across machines). The
//! median is the gated, run-to-run-stable statistic; p90 is shown only as a
//! diagnostic.
//!
//! The stats / table / baseline / environment plumbing is shared with
//! `peppy stack benchmark` via the `latency-report` crate.
//!
//! Run with:
//!     cargo bench -p core-node --bench latency
//!     cargo bench -p core-node --bench latency -- rust    # filter scenarios
//! Set PEPPY_LATENCY_SKIP_PYTHON=1 to skip the Python responder (no `uv`).

#[path = "../tests/latency/harness.rs"]
mod harness;
// Codegen + build + spawn scaffolding stays with the generator's codegen tests
// (see tests/latency.rs for the rationale); reached cross-crate.
#[path = "../../generator-internal/tests/helpers.rs"]
mod helpers;

use std::time::Duration;

use harness::{ALL_SCENARIOS, DEFAULT_SAMPLES, DEFAULT_WARMUP, Lang, Transport, ceiling_ms};
use latency_report::baseline::{self, StoredStats};
use latency_report::environment::CpuEnvironment;
use latency_report::format;

/// Subdirectory under `target/` for this bench's same-machine baseline.
const BASELINE_SUBDIR: &str = "latency-bench";

/// One measured scenario, ready to render.
struct Row {
    name: String,
    p50: Duration,
    p90: Duration,
    mean: Duration,
    ceiling_ms: u64,
    prev_p50_ns: Option<u64>,
}

fn main() {
    // `cargo bench -- <filter>` passes a substring; ignore flag-like args
    // (e.g. the `--bench` libtest harnesses inject).
    let filter: Option<String> = std::env::args().skip(1).find(|arg| !arg.starts_with('-'));
    let skip_python = std::env::var("PEPPY_LATENCY_SKIP_PYTHON").is_ok();

    print_environment();

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .expect("bench tokio runtime");

    let baseline_path = baseline::baseline_path(BASELINE_SUBDIR);
    let previous = baseline::load(&baseline_path);
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
            let prev_p50_ns = previous.get(&name).map(|s| s.p50_ns);
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
                prev_p50_ns,
            });
        }
        runtime.block_on(scenario.shutdown());
    }

    print_table(&rows);
    if let Err(err) = baseline::save(&baseline_path, &current) {
        eprintln!(
            "warning: failed to save latency baseline to {}: {err}",
            baseline_path.display()
        );
    }
}

/// Print the CPU/scheduling environment that governs run-to-run variance, so a
/// slow run is self-explanatory. On a shared host with turbo enabled, per-core
/// frequency and noisy-neighbor load — not the code — dominate absolute numbers.
fn print_environment() {
    let env = CpuEnvironment::detect();
    println!("\nenv: {}", env.summary_line());
    // Nodes run as Zenoh peers: after gossip via the seed router they exchange
    // data over direct peer-to-peer links rather than relaying through the
    // router. Shared memory is not yet wired into the publish path.
    println!("transport: peer (gossip discovery, shm=off)");
    if env.is_noisy() {
        println!(
            "note: turbo and/or shared-host load make absolute latency swing run-to-run regardless \
             of the code. For stable comparisons, measure on a quiet host with turbo disabled and \
             the measured processes pinned to isolated cores — see .github/workflows/latency.yml."
        );
    }
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
// Rendering (column layout shared via latency_report::format)
// ---------------------------------------------------------------------------

fn print_table(rows: &[Row]) {
    if rows.is_empty() {
        println!("\nno scenarios matched the filter");
        return;
    }

    let headers = [
        "scenario", "p50", "p90", "mean", "ceiling", "prev p50", "Δp50", "status",
    ];
    let mut cells: Vec<Vec<String>> = Vec::with_capacity(rows.len());
    for row in rows {
        let (prev, delta) = match row.prev_p50_ns {
            Some(prev_ns) => (
                format::fmt_duration(Duration::from_nanos(prev_ns)),
                format::fmt_delta(row.p50.as_nanos() as u64, prev_ns),
            ),
            None => ("—".to_string(), "—".to_string()),
        };
        let status = if row.p50 <= Duration::from_millis(row.ceiling_ms) {
            "✓"
        } else {
            "✗ OVER"
        };
        cells.push(vec![
            row.name.clone(),
            format::fmt_duration(row.p50),
            format::fmt_duration(row.p90),
            format::fmt_duration(row.mean),
            format!("{}ms", row.ceiling_ms),
            prev,
            delta,
            status.to_string(),
        ]);
    }

    println!(
        "\nRoundtrip latency (two real peppy nodes, {} samples/scenario)",
        DEFAULT_SAMPLES
    );
    println!("{}", format::render_table(&headers, &cells));
    println!(
        "\nΔp50 = median vs the previous run on this machine (+ = slower, - = faster). \
         status = median within ceiling (the gated metric; p90 is a diagnostic). \
         Ceilings via PEPPY_LATENCY_MAX_MS_<LANG>_<TRANSPORT>."
    );
}
