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
use latency_report::format::{fmt_delta, fmt_duration};
use peppylib::ActionMessenger;
use peppylib::core_node::transport::send_stack_benchmark;
use peppylib::messaging::ResultStatus;
use tracing::info;

use super::colors::{BINDING_COLOR, NODE_COLOR, paint};
use super::table::{render_table, wrap_ansi};
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

/// How wide the `note` column may grow before its text wraps to a new physical
/// line within the box cell. Keeps a long note (the synthetic baseline detail)
/// from widening the whole table on a narrow terminal.
const NOTE_WRAP_COLS: usize = 18;

/// The `edge` cell, tinted with the shared `stack` palette and wrapped onto
/// three lines so the (often long) node identities never force a wide column:
/// the consumer, then the kind arrow + producer (`➔` for an interface-
/// conformance edge, `→` for a direct one), then the consumed interface indented
/// beneath the producer. The column then only needs to fit the widest single
/// node label rather than the whole `from → to/iface` string. Node labels are
/// cyan, as `stack list` colors them. Mirrors [`InterfaceLatency::edge_label`]
/// but colored and wrapped; the plain `edge_label` still backs the baseline key.
fn edge_cell(row: &InterfaceLatency, colorize: bool) -> String {
    let arrow = if row.via_interface.is_some() {
        "➔"
    } else {
        "→"
    };
    format!(
        "{}\n{arrow} {}\n  /{}",
        paint(
            colorize,
            NODE_COLOR,
            &format!("{}:{}", row.from_node, row.from_tag)
        ),
        paint(
            colorize,
            NODE_COLOR,
            &format!("{}:{}", row.to_node, row.to_tag)
        ),
        row.interface_name,
    )
}

fn render_report(result: &StackBenchmarkResult, samples: u32) {
    let colorize = crate::terminal::colors_enabled();
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

    let rows = display_rows(&result.rows, &previous, colorize);

    println!(
        "\nInterface latency against the running stack ({} samples/interface)",
        samples
    );
    let mut table = String::new();
    render_table(&mut table, &BENCHMARK_HEADERS, &[rows]);
    print!("{table}");
    println!(
        "edge: `→` a direct `depends_on.nodes` dependency; `➔` resolved through interface \
         conformance — the note names the interface.\n\
         measure: svc/act-probe = messaging round-trip (handler NOT run); \
         delivery = real producer→consumer latency; synthetic = transport proxy.\n\
         binding: the dependency binding this edge was measured through — a node can consume the \
         same interface from the same producer via multiple bindings.\n\
         clock: same-host = exact; corrected = cross-host adjusted via the producer's \
         measured offset; flagged = implausible, suppressed (deploy PTP/NTP).\n\
         Δp50 = median vs the previous run on this machine. Benchmarking never triggers \
         a real handler or creates a goal."
    );

    // Persist this run's stats as the same-machine baseline for the next run's
    // Δp50 column. Only rows that actually measured (count > 0) update it.
    let mut current: BTreeMap<String, StoredStats> = previous;
    for row in &result.rows {
        if row.count > 0 {
            current.insert(
                row_key(row),
                StoredStats {
                    p50_ns: row.p50_ns,
                    p90_ns: row.p90_ns,
                    mean_ns: row.mean_ns,
                },
            );
        }
    }
    baseline::save(&baseline_path, &current);
}

/// Column headers for the benchmark table.
const BENCHMARK_HEADERS: [&str; 10] = [
    "edge", "binding", "measure", "clock", "p50", "p90", "mean", "n", "Δp50", "note",
];

/// Build the box-table data rows (one per measured row), tinted with the shared
/// `stack` palette and with the wide `edge`/`note` cells wrapped. `previous`
/// supplies the Δp50 baseline. Pure (no IO) so it can be unit-tested.
fn display_rows(
    rows: &[InterfaceLatency],
    previous: &BTreeMap<String, StoredStats>,
    colorize: bool,
) -> Vec<Vec<String>> {
    rows.iter()
        .map(|row| {
            let delta = match previous.get(&row_key(row)).map(|s| s.p50_ns) {
                Some(prev) if prev > 0 => fmt_delta(row.p50_ns, prev),
                _ => "—".to_string(),
            };
            let dur = |ns: u64| {
                if row.count == 0 {
                    "—".to_string()
                } else {
                    fmt_duration(Duration::from_nanos(ns))
                }
            };
            // The `➔` arrow + legend already say "via interface conformance",
            // so the note only names the interface (tinted like the labels it
            // relates), followed by any measurement diagnostic. Wrapped so a long
            // note (the synthetic baseline detail) doesn't widen the whole table.
            let iface = row
                .via_interface
                .as_deref()
                .map(|i| paint(colorize, NODE_COLOR, i));
            let note = match (&iface, &row.note) {
                (Some(iface), Some(n)) => format!("{iface}; {n}"),
                (Some(iface), None) => iface.clone(),
                (None, Some(n)) => n.clone(),
                (None, None) => String::new(),
            };
            vec![
                edge_cell(row, colorize),
                paint(colorize, BINDING_COLOR, &row.link_id),
                measurement_label(row.measurement).to_string(),
                row.clock_confidence.as_str().to_string(),
                dur(row.p50_ns),
                dur(row.p90_ns),
                dur(row.mean_ns),
                row.count.to_string(),
                delta,
                wrap_ansi(&note, NOTE_WRAP_COLS),
            ]
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use core_node_api::encoding::{ClockConfidence, InterfaceKind};
    use unicode_width::UnicodeWidthStr;

    #[allow(clippy::too_many_arguments)]
    fn row(
        from: &str,
        to: &str,
        interface: &str,
        link_id: &str,
        via: Option<&str>,
        kind: InterfaceKind,
        measurement: MeasurementKind,
        clock: ClockConfidence,
        note: Option<&str>,
    ) -> InterfaceLatency {
        InterfaceLatency {
            from_node: from.to_string(),
            from_tag: "v1".to_string(),
            to_node: to.to_string(),
            to_tag: "v1".to_string(),
            interface_name: interface.to_string(),
            link_id: link_id.to_string(),
            via_interface: via.map(str::to_string),
            kind,
            measurement,
            clock_confidence: clock,
            p50_ns: 5_500_000,
            p90_ns: 7_550_000,
            mean_ns: 5_610_000,
            count: 200,
            samples_ns: vec![],
            note: note.map(str::to_string),
        }
    }

    /// Rows mirroring the real stack: a direct action edge, an interface-
    /// conformance delivery edge, and its synthetic baseline (longest note).
    fn sample_rows() -> Vec<InterfaceLatency> {
        vec![
            row(
                "my_python_robot_backbone",
                "my_python_robot_arm",
                "move_arm",
                "right_robot_arm",
                None,
                InterfaceKind::Action,
                MeasurementKind::ActionProbe,
                ClockConfidence::NotApplicable,
                None,
            ),
            row(
                "uvc_camera_video_reconstruction",
                "uvc_camera_python_mock",
                "video_stream",
                "camera",
                Some("uvc_camera:v1"),
                InterfaceKind::Topic,
                MeasurementKind::TopicDelivery,
                ClockConfidence::CrossHostCorrected,
                None,
            ),
            row(
                "uvc_camera_video_reconstruction",
                "uvc_camera_python_mock",
                "video_stream",
                "camera",
                Some("uvc_camera:v1"),
                InterfaceKind::Topic,
                MeasurementKind::TopicSynthetic,
                ClockConfidence::NotApplicable,
                Some("256B fixed payload, sensor_data QoS"),
            ),
        ]
    }

    #[test]
    fn edge_cell_wraps_onto_three_lines_with_kind_arrow() {
        let rows = sample_rows();
        // Direct edge uses the light arrow; consumer / arrow + producer /
        // indented interface, so no single line carries the full path.
        let direct = edge_cell(&rows[0], false);
        assert_eq!(
            direct,
            "my_python_robot_backbone:v1\n→ my_python_robot_arm:v1\n  /move_arm"
        );
        // Interface-conformance edge uses the heavy arrow.
        let conformance = edge_cell(&rows[1], false);
        assert_eq!(
            conformance,
            "uvc_camera_video_reconstruction:v1\n➔ uvc_camera_python_mock:v1\n  /video_stream"
        );
    }

    #[test]
    fn note_names_interface_and_wraps_long_synthetic_detail() {
        let rows = display_rows(&sample_rows(), &BTreeMap::new(), false);
        // Delivery row's note is just the interface name.
        assert_eq!(rows[1][9], "uvc_camera:v1");
        // Synthetic row's note leads with the interface, then the wrapped detail.
        let synthetic_note = &rows[2][9];
        assert!(synthetic_note.starts_with("uvc_camera:v1;"));
        assert!(synthetic_note.contains('\n'), "long note should wrap");
        assert!(
            synthetic_note
                .lines()
                .all(|l| UnicodeWidthStr::width(l) <= NOTE_WRAP_COLS),
            "every note line within wrap width:\n{synthetic_note}"
        );
    }

    #[test]
    fn table_renders_box_and_stays_narrow() {
        let rows = display_rows(&sample_rows(), &BTreeMap::new(), false);
        let mut out = String::new();
        render_table(&mut out, &BENCHMARK_HEADERS, &[rows]);

        // Box-drawing borders like `stack list`.
        assert!(out.contains('┌') && out.contains('│') && out.contains('└'));
        // Headers present.
        for h in BENCHMARK_HEADERS {
            assert!(out.contains(h), "missing header {h}");
        }
        // Every rendered line shares one display width (aligned box) and the
        // wrapped layout keeps the whole table comfortably under a wide column.
        let widths: Vec<usize> = out
            .lines()
            .filter(|l| l.starts_with(['┌', '├', '└', '│']))
            .map(UnicodeWidthStr::width)
            .collect();
        assert!(
            widths.windows(2).all(|w| w[0] == w[1]),
            "box lines misaligned: {widths:?}"
        );
        let table_width = widths.first().copied().unwrap_or(0);
        assert!(
            table_width <= 145,
            "table too wide ({table_width} cols) for a small terminal"
        );
    }
}
