//! Roundtrip latency threshold guard.
//!
//! Spins up two real peppy nodes (a Rust driver + a Rust or Python responder),
//! measures topic and service roundtrip latency, and asserts the p90 stays
//! under a generous, order-of-magnitude ceiling — so a regression that makes
//! messaging ~10x slower fails CI, while CI jitter does not.
//!
//! These tests are `#[ignore]` by default (they spawn node processes and build
//! Rust/Python projects); they run only in the dedicated `dev` -> `main`
//! `latency.yml` workflow. Run locally with:
//!     cargo test -p generator --test latency -- --ignored --test-threads 1
//!
//! Ceilings are env-overridable (`PEPPY_LATENCY_MAX_MS_<LANG>_<TRANSPORT>`) and
//! the Python scenarios can be skipped where `uv` is unavailable
//! (`PEPPY_LATENCY_SKIP_PYTHON=1`). Tighten the defaults to ~3x observed p90
//! once baselines from the first runs are known.

#[path = "latency/harness.rs"]
mod harness;
mod helpers;

use std::time::Duration;

use harness::{DEFAULT_SAMPLES, DEFAULT_WARMUP, Lang, Transport, ceiling_ms};

fn python_skipped() -> bool {
    std::env::var("PEPPY_LATENCY_SKIP_PYTHON").is_ok()
}

async fn check(lang: Lang, transport: Transport) {
    if lang == Lang::Python && python_skipped() {
        eprintln!(
            "skipping {}/{}: PEPPY_LATENCY_SKIP_PYTHON set",
            lang.as_str(),
            transport.as_str()
        );
        return;
    }
    let stats = harness::run_once(lang, transport, DEFAULT_WARMUP, DEFAULT_SAMPLES).await;
    let p90 = stats.p90();
    let ceiling = Duration::from_millis(ceiling_ms(lang, transport));
    eprintln!(
        "{}/{}: p50={:?} p90={:?} mean={:?} n={} (ceiling {:?})",
        lang.as_str(),
        transport.as_str(),
        stats.p50(),
        p90,
        stats.mean(),
        stats.count(),
        ceiling,
    );
    assert!(
        p90 <= ceiling,
        "{}/{} roundtrip p90 {:?} exceeded threshold {:?}",
        lang.as_str(),
        transport.as_str(),
        p90,
        ceiling,
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "spawns real node processes; heavy — run via the dev->main latency.yml workflow"]
async fn rust_topic_roundtrip_under_threshold() {
    check(Lang::Rust, Transport::Topic).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "spawns real node processes; heavy — run via the dev->main latency.yml workflow"]
async fn rust_service_roundtrip_under_threshold() {
    check(Lang::Rust, Transport::Service).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "spawns real node processes; heavy — run via the dev->main latency.yml workflow"]
async fn python_topic_roundtrip_under_threshold() {
    check(Lang::Python, Transport::Topic).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "spawns real node processes; heavy — run via the dev->main latency.yml workflow"]
async fn python_service_roundtrip_under_threshold() {
    check(Lang::Python, Transport::Service).await;
}
