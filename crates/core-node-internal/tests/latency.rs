//! Roundtrip latency threshold guard.
//!
//! Spins up two real peppy nodes (a Rust driver + a Rust or Python responder)
//! and measures topic and service roundtrip latency. The pass/fail assertion is
//! on the **median (p50)**, not a tail percentile: the median is a robust,
//! run-to-run-stable statistic, whereas p90/p99 are estimated from a handful of
//! tail samples and swing with any scheduler preemption or CPU down-clock —
//! gating on them is the dominant flakiness source for sub-millisecond IPC. p90
//! and mean are still printed as diagnostics.
//!
//! One scenario per language is started once and reused for both transports
//! (matching the bench), so process-spawn churn doesn't perturb the numbers.
//!
//! These tests are `#[ignore]` by default (they spawn node processes and build
//! Rust/Python projects); they run only in the dedicated `dev` -> `main`
//! `latency.yml` workflow. Run locally with:
//!     cargo test -p core-node --test latency -- --ignored --test-threads 1
//!
//! Ceilings are env-overridable (`PEPPY_LATENCY_MAX_MS_<LABEL>` with the
//! scenario label uppercased and `/`/`-` mapped to `_`) and
//! the Python scenarios can be skipped where `uv` is unavailable
//! (`PEPPY_LATENCY_SKIP_PYTHON=1`).

#[path = "latency/harness.rs"]
mod harness;
// The node codegen + build + spawn scaffolding lives with the generator's own
// codegen tests (`rust.rs` / `python.rs` include the same file), so it has to
// stay in generator-internal. The latency harness reaches it cross-crate
// instead of duplicating ~500 lines; `#[allow(dead_code)]` at its top tolerates
// each consumer using only a subset.
#[path = "../../generator-internal/tests/helpers.rs"]
mod helpers;

use std::time::Duration;

use harness::{ALL_SCENARIOS, DEFAULT_SAMPLES, DEFAULT_WARMUP, Lang, ceiling_ms};

fn python_skipped() -> bool {
    std::env::var("PEPPY_LATENCY_SKIP_PYTHON").is_ok()
}

/// Start one scenario for `lang` and assert the median roundtrip of every
/// transport stays under its ceiling. Both transports are measured (and logged)
/// before any assertion fails, so a failure reports the full picture.
async fn check_lang(lang: Lang) {
    if lang == Lang::Python && python_skipped() {
        eprintln!("skipping {}: PEPPY_LATENCY_SKIP_PYTHON set", lang.as_str());
        return;
    }

    let scenario = harness::start_scenario(lang).await;
    let mut failures: Vec<String> = Vec::new();

    for bench in ALL_SCENARIOS.iter().filter(|s| s.lang == lang) {
        let stats = scenario.run(bench, DEFAULT_WARMUP, DEFAULT_SAMPLES).await;
        let median = stats.p50();
        let ceiling = Duration::from_millis(ceiling_ms(bench));
        eprintln!(
            "{}: p50={:?} p90={:?} mean={:?} n={} shm={} (median ceiling {:?})",
            bench.label,
            median,
            stats.p90(),
            stats.mean(),
            stats.count(),
            stats.shm_used(),
            ceiling,
        );
        if median > ceiling {
            failures.push(format!(
                "{} median {median:?} exceeded ceiling {ceiling:?} (p90 {:?})",
                bench.label,
                stats.p90(),
            ));
        }
    }

    scenario.shutdown().await;
    assert!(failures.is_empty(), "{}", failures.join("; "));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "spawns real node processes; heavy — run via the dev->main latency.yml workflow"]
async fn rust_roundtrip_latency_under_threshold() {
    check_lang(Lang::Rust).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "spawns real node processes; heavy — run via the dev->main latency.yml workflow"]
async fn python_roundtrip_latency_under_threshold() {
    check_lang(Lang::Python).await;
}
