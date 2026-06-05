//! `peppy stack benchmark` — thin client that drives the `stack_benchmark`
//! action on the daemon and renders the per-interface latency table.
//!
//! What the numbers mean (and don't):
//! - **service / action** rows are messaging-path *round-trips* to the endpoint,
//!   excluding the handler's own execution time. Clock-independent.
//! - **topic (delivery)** rows are the real producer→consumer one-way latency on
//!   live traffic. Exact on a single host; cross-host needs PTP or NTP (the row's
//!   `clock` column says how it was treated).
//! - **topic (synthetic)** rows are a transport-path proxy on a reserved key —
//!   not a real edge measurement. Clock-independent.
//!
//! Benchmarking never triggers a real handler or creates a goal.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use core_node_api::encoding::{
    InterfaceLatency, MeasurementKind, StackBenchmarkFeedback, StackBenchmarkGoal,
    StackBenchmarkGoalResponse, StackBenchmarkResult,
};
use latency_report::baseline::{self, StoredStats};
use latency_report::environment::CpuEnvironment;
use latency_report::format::{self, fmt_duration};
use peppylib::ActionMessenger;
use peppylib::core_node::transport::send_stack_benchmark;
use peppylib::messaging::ResultStatus;
use tracing::info;

use crate::commands::{CALLER_INSTANCE_ID, GOAL_TIMEOUT, SCROLLING_OUTPUT_LINES};
use crate::context::AppContext;
use crate::error::{Error, Result};
use crate::terminal::ScrollingOutput;

/// Subdirectory under `target/` for this command's same-machine baseline.
const BASELINE_SUBDIR: &str = "stack-benchmark";
/// Idle watchdog: the daemon emits feedback per edge; a long gap means a hang.
/// Generous because a single dead edge can take a few probe timeouts to skip.
const CLI_IDLE_TIMEOUT: Duration = Duration::from_secs(300);
/// Absolute ceiling so the command can never wedge forever.
const CLI_MAX_TIMEOUT: Duration = Duration::from_secs(3600);
const FEEDBACK_DRAIN_TIMEOUT: Duration = Duration::from_millis(50);

pub fn benchmark(
    ctx: &Arc<AppContext>,
    samples: u32,
    warmup: u32,
    include_synthetic_baseline: bool,
    per_sample_timeout_ms: u64,
) -> Result<()> {
    crate::commands::block_on(benchmark_async(
        ctx,
        samples,
        warmup,
        include_synthetic_baseline,
        per_sample_timeout_ms,
    ))
}

async fn benchmark_async(
    ctx: &Arc<AppContext>,
    samples: u32,
    warmup: u32,
    include_synthetic_baseline: bool,
    per_sample_timeout_ms: u64,
) -> Result<()> {
    let conn = ctx.connect_to_daemon().await?;

    let goal = StackBenchmarkGoal::new(
        samples,
        warmup,
        include_synthetic_baseline,
        per_sample_timeout_ms,
    );

    info!(
        "Benchmarking stack on daemon '{}' ({} samples, {} warmup per interface)",
        conn.core_node_name, samples, warmup
    );

    let mut action_handle = send_stack_benchmark(
        &goal,
        conn.messenger,
        &conn.core_node_name,
        CALLER_INSTANCE_ID,
        None,
        None,
        GOAL_TIMEOUT,
    )
    .await
    .map_err(|e| Error::ExecutionFailed(format!("Failed to send benchmark goal: {}", e)))?;

    let goal_response = StackBenchmarkGoalResponse::decode(
        &action_handle.goal_response().payload(),
    )
    .map_err(|e| Error::ExecutionFailed(format!("Failed to decode goal response: {}", e)))?;

    if !goal_response.accepted {
        let reason = goal_response
            .rejection_reason
            .unwrap_or_else(|| "unknown reason".to_string());
        return Err(Error::ExecutionFailed(format!(
            "Benchmark goal rejected: {}",
            reason
        )));
    }

    // Drain feedback (progress lines) until the daemon closes the stream.
    let absolute_deadline = tokio::time::Instant::now() + CLI_MAX_TIMEOUT;
    let mut last_activity = tokio::time::Instant::now();
    let mut scrolling = ScrollingOutput::new(SCROLLING_OUTPUT_LINES);

    loop {
        let now = tokio::time::Instant::now();
        if now >= absolute_deadline {
            scrolling.clear();
            return Err(Error::ExecutionFailed(
                "Benchmark timed out: max timeout exceeded".to_string(),
            ));
        }
        if now.duration_since(last_activity) >= CLI_IDLE_TIMEOUT {
            scrolling.clear();
            return Err(Error::ExecutionFailed(format!(
                "Benchmark timed out: no progress for {}s",
                CLI_IDLE_TIMEOUT.as_secs()
            )));
        }

        match tokio::time::timeout(FEEDBACK_DRAIN_TIMEOUT, action_handle.on_next_feedback()).await {
            Ok(Ok(msg)) => {
                last_activity = tokio::time::Instant::now();
                if let Ok(feedback) = StackBenchmarkFeedback::decode(&msg.payload()) {
                    scrolling.add_line(&feedback.line, feedback.is_stderr());
                }
            }
            Ok(Err(_)) => break, // end-of-stream: the goal has completed
            Err(_) => {}         // drain slice elapsed; re-check timeouts
        }
    }

    let result_timeout = absolute_deadline
        .saturating_duration_since(tokio::time::Instant::now())
        .max(Duration::from_secs(5));
    let reply = ActionMessenger::request_result(conn.messenger, &action_handle, result_timeout)
        .await
        .map_err(|e| Error::ExecutionFailed(format!("Failed to get benchmark result: {}", e)))?;

    scrolling.clear();

    let body = match reply.status {
        ResultStatus::Completed | ResultStatus::Cancelled => reply.body,
        ResultStatus::Abandoned => {
            return Err(Error::ExecutionFailed(
                "the benchmark goal was abandoned before producing a result".to_string(),
            ));
        }
        ResultStatus::Expired => {
            return Err(Error::ExecutionFailed(
                "the benchmark result expired before it could be fetched".to_string(),
            ));
        }
    };

    let result = StackBenchmarkResult::decode(body.as_ref())
        .map_err(|e| Error::ExecutionFailed(format!("Failed to decode benchmark result: {}", e)))?;

    if !result.success {
        let msg = result
            .error_message
            .unwrap_or_else(|| "unknown error".to_string());
        return Err(Error::ExecutionFailed(format!("Benchmark failed: {}", msg)));
    }

    render_report(&result, samples);
    Ok(())
}

/// Stable per-row key for the same-machine baseline.
fn row_key(row: &InterfaceLatency) -> String {
    format!(
        "{}|{}|{}",
        row.edge_label(),
        row.link_id,
        row.measurement.as_str()
    )
}

fn measurement_label(kind: MeasurementKind) -> &'static str {
    match kind {
        MeasurementKind::ServiceProbe => "svc-probe",
        MeasurementKind::ActionProbe => "act-probe",
        MeasurementKind::TopicDelivery => "delivery",
        MeasurementKind::TopicSynthetic => "synthetic",
    }
}

fn render_report(result: &StackBenchmarkResult, samples: u32) {
    let env = CpuEnvironment::detect();
    println!("\nenv: {}", env.summary_line());
    if env.is_noisy() {
        println!(
            "note: turbo and/or shared-host load make absolute latency swing run-to-run. \
             For stable comparisons use a quiet host with turbo disabled."
        );
    }

    if result.rows.is_empty() {
        println!("\nNo interface edges found in the running stack — nothing to benchmark.");
        return;
    }

    let baseline_path = baseline::baseline_path(BASELINE_SUBDIR);
    let previous = baseline::load(&baseline_path);
    let mut current: BTreeMap<String, StoredStats> = previous.clone();

    let headers = [
        "edge", "binding", "measure", "clock", "p50", "p90", "mean", "n", "Δp50", "note",
    ];
    let mut rows: Vec<Vec<String>> = Vec::with_capacity(result.rows.len());
    for row in &result.rows {
        let key = row_key(row);
        let prev_p50 = previous.get(&key).map(|s| s.p50_ns);
        let delta = match prev_p50 {
            Some(prev) if prev > 0 => format::fmt_delta(row.p50_ns, prev),
            _ => "—".to_string(),
        };
        if row.count > 0 {
            current.insert(
                key,
                StoredStats {
                    p50_ns: row.p50_ns,
                    p90_ns: row.p90_ns,
                    mean_ns: row.mean_ns,
                },
            );
        }
        let dur = |ns: u64| {
            if row.count == 0 {
                "—".to_string()
            } else {
                fmt_duration(Duration::from_nanos(ns))
            }
        };
        rows.push(vec![
            row.edge_label(),
            row.link_id.clone(),
            measurement_label(row.measurement).to_string(),
            row.clock_confidence.as_str().to_string(),
            dur(row.p50_ns),
            dur(row.p90_ns),
            dur(row.mean_ns),
            row.count.to_string(),
            delta,
            row.note.clone().unwrap_or_default(),
        ]);
    }

    println!(
        "\nInterface latency against the running stack ({} samples/interface)",
        samples
    );
    println!("{}", format::render_table(&headers, &rows));
    println!(
        "\nmeasure: svc/act-probe = messaging round-trip (handler NOT run); \
         delivery = real producer→consumer latency; synthetic = transport proxy.\n\
         binding: the dependency binding this edge was measured through — a node can consume the \
         same interface from the same producer via multiple bindings.\n\
         clock: same-host = exact; corrected = cross-host adjusted via the producer's \
         measured offset; flagged = implausible, suppressed (deploy PTP/NTP).\n\
         Δp50 = median vs the previous run on this machine. Benchmarking never triggers \
         a real handler or creates a goal."
    );

    baseline::save(&baseline_path, &current);
}
