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

use harness::{ALL_SCENARIOS, BenchScenario, DEFAULT_SAMPLES, DEFAULT_WARMUP, Lang, ceiling_ms};
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
    shm_used: bool,
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
        let selected_scenarios: Vec<&BenchScenario> = ALL_SCENARIOS
            .iter()
            .filter(|s| s.lang == lang)
            .filter(|s| selected(&filter, s))
            .collect();
        if selected_scenarios.is_empty() {
            continue;
        }

        let scenario = runtime.block_on(harness::start_scenario(lang));
        for bench in selected_scenarios {
            let stats = runtime.block_on(scenario.run(bench, DEFAULT_WARMUP, DEFAULT_SAMPLES));
            let name = bench.label.to_string();
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
                ceiling_ms: ceiling_ms(bench),
                prev_p50_ns,
                shm_used: stats.shm_used(),
            });
        }
        runtime.block_on(scenario.shutdown());
    }

    print_table(&rows);
    baseline::save(&baseline_path, &current);
}

/// Print the CPU/scheduling environment that governs run-to-run variance, so a
/// slow run is self-explanatory. On a shared host with turbo enabled, per-core
/// frequency and noisy-neighbor load — not the code — dominate absolute numbers.
fn print_environment() {
    let env = CpuEnvironment::detect();
    println!("\nenv: {}", env.summary_line());
    // The spawned nodes run with the default runtime config, so the line below
    // reflects exactly what they were configured with (the per-scenario `shm`
    // table column then reports whether shared memory was actually used on the
    // wire, as observed by the driver on its received payloads).
    let discovery = config::runtime::DiscoveryConfig::default();
    let routing = if discovery.gossip {
        "peer (gossip discovery)"
    } else {
        "router (relay)"
    };
    let shm = if discovery.shm { "on" } else { "off" };
    println!("transport: {routing}, shm={shm}");
    if env.is_noisy() {
        println!(
            "note: turbo and/or shared-host load make absolute latency swing run-to-run regardless \
             of the code. For stable comparisons, measure on a quiet host with turbo disabled and \
             the measured processes pinned to isolated cores — see .github/workflows/latency.yml."
        );
    }
}

/// Whether a scenario matches the optional `cargo bench -- <filter>` substring,
/// tested against the scenario label.
fn selected(filter: &Option<String>, scenario: &BenchScenario) -> bool {
    match filter {
        None => true,
        Some(needle) => scenario.label.contains(needle.as_str()),
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
        "scenario", "p50", "p90", "mean", "ceiling", "prev p50", "Δp50", "shm", "status",
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
            if row.shm_used { "yes" } else { "no" }.to_string(),
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
         shm = the driver observed its received payloads as shared-memory backed \
         (sub-threshold payloads are always 'no' by design). \
         Ceilings via PEPPY_LATENCY_MAX_MS_<SCENARIO>."
    );
}
