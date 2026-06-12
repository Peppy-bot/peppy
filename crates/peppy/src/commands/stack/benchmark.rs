//! `peppy stack benchmark` — thin client that drives the `stack_benchmark`
//! action on the daemon and renders the per-interface latency report as two
//! tables, so synthetic plumbing numbers are never read side by side with real
//! payload numbers:
//!
//! - **Synthetic probes** (svc-probe / act-probe / node-probe): messaging-path
//!   *round-trips* carrying schema-sized payloads, excluding any handler
//!   execution. Clock-independent. A topic edge's node-probe targets the
//!   producer node's framework, not the topic itself.
//! - **Real traffic** (delivery): the real producer→consumer one-way latency of
//!   live topic messages, full payload included. Exact on a single host;
//!   cross-host needs PTP or NTP (the row's `clock` column says how it was
//!   treated).
//!
//! Benchmarking never triggers a real handler, never publishes onto a real
//! topic, and never creates a goal.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use core_node_api::encoding::{
    InterfaceLatency, MeasurementKind, StackBenchmarkFeedback, StackBenchmarkGoal,
    StackBenchmarkGoalResponse, StackBenchmarkResult, Transport,
};
use latency_report::baseline::{self, StoredStats};
use latency_report::environment::CpuEnvironment;
use latency_report::format::{fmt_delta, fmt_duration};
use peppylib::ActionMessenger;
use peppylib::core_node::transport::send_stack_benchmark;
use peppylib::messaging::ResultStatus;
use tracing::info;

use super::colors::{
    BINDING_COLOR, MEASURE_ACTION_COLOR, MEASURE_DELIVERY_COLOR, MEASURE_NODE_COLOR,
    MEASURE_SERVICE_COLOR, NODE_COLOR, TRANSPORT_MIXED_COLOR, TRANSPORT_SHM_COLOR, paint,
};
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
    per_sample_timeout_ms: u64,
) -> Result<()> {
    crate::commands::block_on(benchmark_async(ctx, samples, warmup, per_sample_timeout_ms))
}

async fn benchmark_async(
    ctx: &Arc<AppContext>,
    samples: u32,
    warmup: u32,
    per_sample_timeout_ms: u64,
) -> Result<()> {
    let conn = ctx.connect_to_daemon().await?;

    let goal = StackBenchmarkGoal::new(samples, warmup, per_sample_timeout_ms);

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
        MeasurementKind::NodeProbe => "node-probe",
    }
}

/// Distinct color per measurement kind so the `measure` column reads at a glance:
/// blue service-probe, magenta action-probe, cyan node-probe, green live
/// delivery. The legend paints the same labels the same way as a key.
fn measurement_color(kind: MeasurementKind) -> &'static str {
    match kind {
        MeasurementKind::ServiceProbe => MEASURE_SERVICE_COLOR,
        MeasurementKind::ActionProbe => MEASURE_ACTION_COLOR,
        MeasurementKind::TopicDelivery => MEASURE_DELIVERY_COLOR,
        MeasurementKind::NodeProbe => MEASURE_NODE_COLOR,
    }
}

/// The `transport` cell: green `shm` (the fast path worth spotting at a
/// glance), yellow `mixed` (worth a look — the note carries the shm share),
/// and the plain network protocol name (`net_label`, e.g. `tcp`) for the
/// normal network path. `—` for rows that measured nothing.
fn transport_cell(row: &InterfaceLatency, net_label: &str, colorize: bool) -> String {
    match row.transport {
        Transport::Shm => paint(colorize, TRANSPORT_SHM_COLOR, "shm"),
        Transport::Mixed => paint(colorize, TRANSPORT_MIXED_COLOR, "mixed"),
        Transport::Network => net_label.to_string(),
        Transport::NotMeasured => "—".to_string(),
    }
}

/// How wide the `note` column may grow before its text wraps to a new physical
/// line within the box cell. Keeps a long note (e.g. a probe's payload summary)
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

    // The network-path transport label: the daemon stamps its session's
    // connect protocol ("tcp"); an empty stamp (a messenger with no network
    // link to name) falls back to the generic label.
    let net_label = if result.net_protocol.is_empty() {
        "net"
    } else {
        result.net_protocol.as_str()
    };

    // Synthetic probes and real-traffic measurements answer different
    // questions with payloads that can differ by orders of magnitude, so they
    // render as separate tables instead of adjacent rows inviting comparison.
    let (synthetic, real): (Vec<&InterfaceLatency>, Vec<&InterfaceLatency>) = result
        .rows
        .iter()
        .partition(|r| r.measurement.is_synthetic_probe());

    if !synthetic.is_empty() {
        println!(
            "\nSynthetic probes: handler-free round-trips, schema-sized payloads \
             ({samples} samples/interface)"
        );
        let rows = display_rows(
            &synthetic,
            &previous,
            colorize,
            ReportTable::Synthetic,
            net_label,
        );
        let mut table = String::new();
        render_table(&mut table, &SYNTHETIC_HEADERS, &[rows]);
        print!("{table}");
    }

    if !real.is_empty() {
        println!(
            "\nReal traffic: observe-only one-way delivery of live topic messages \
             ({samples} samples/interface)"
        );
        let rows = display_rows(&real, &previous, colorize, ReportTable::Real, net_label);
        let mut table = String::new();
        render_table(&mut table, &REAL_HEADERS, &[rows]);
        print!("{table}");
    }

    println!("{}", benchmark_legend(colorize, net_label));

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

/// Which report table rows are being rendered for. The synthetic table shows
/// the `measure` column (its rows are all clock-independent round-trips, so a
/// `clock` column would be pure noise); the real-traffic table shows the
/// `clock` column (its rows are all `delivery`, so a `measure` column would be).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ReportTable {
    Synthetic,
    Real,
}

/// Column headers for the synthetic-probe table.
const SYNTHETIC_HEADERS: [&str; 10] = [
    "edge",
    "binding",
    "measure",
    "transport",
    "p50",
    "p90",
    "mean",
    "n",
    "Δp50",
    "note",
];

/// Column headers for the real-traffic table.
const REAL_HEADERS: [&str; 10] = [
    "edge",
    "binding",
    "clock",
    "transport",
    "p50",
    "p90",
    "mean",
    "n",
    "Δp50",
    "note",
];

/// Footnote legend beneath the tables — one category per block, its variants
/// aligned on their own lines so a reader can scan each meaning instead of
/// parsing a run-on sentence. The `measure` and `transport` labels are painted
/// in the same colors as their columns so the legend doubles as a color key;
/// `net_label` is the network protocol name the `transport` column shows
/// (e.g. `tcp`). The leading `\` swallows the newline after the opening quote
/// so the text starts at `Legend:`.
fn benchmark_legend(colorize: bool, net_label: &str) -> String {
    let svc = paint(colorize, MEASURE_SERVICE_COLOR, "svc-probe ");
    let act = paint(colorize, MEASURE_ACTION_COLOR, "act-probe ");
    let node = paint(colorize, MEASURE_NODE_COLOR, "node-probe");
    let delivery = paint(colorize, MEASURE_DELIVERY_COLOR, "delivery");
    // Variant labels padded to the clock block's 9-column field, padding
    // inside the painted string like the measure labels, so the escape codes
    // stay width-neutral.
    let shm = paint(colorize, TRANSPORT_SHM_COLOR, "shm      ");
    let mixed = paint(colorize, TRANSPORT_MIXED_COLOR, "mixed    ");
    let net = format!("{net_label:<9}");
    format!(
        "\
Legend:
  edge       →  direct dependency (depends_on.nodes)
             ➔  resolved through interface conformance (the note names the interface)
  synthetic  round-trips on a single clock; the producer's framework replies and
             handlers never run, with payloads sized from the message schema
             {svc}  round-trip to the service
             {act}  round-trip to the action's goal service (no goal is created)
             {node}  topic edge: round-trip to the producer node's framework,
                         reply sized from the topic schema (the topic itself is
                         never published; topic QoS does not apply)
  real       observe-only: {delivery} is the one-way receive−source latency of the
             topic's own live messages, full payload included
  binding    the dependency binding this edge was measured through; a node can
             consume the same interface from one producer via several bindings
  clock      same-host  exact (producer shares this host's clock)
             corrected  cross-host, adjusted via the producer's measured offset
             flagged    implausible delta, suppressed (deploy PTP/NTP)
  transport  how each measured payload physically arrived at this core node,
             observed per sample: probe rows observe the reply leg (the request
             leg can differ when the sizes straddle the 4 KiB shm threshold);
             delivery rows observe this host's subscriber, not each consumer
             {shm}  arrived through a shared-memory segment (read in place,
                        no receive-side copy)
             {net}  crossed the network stack — expected when the payload is
                        under the 4 KiB shm publish threshold, the producer runs
                        on another host or in an isolated container namespace,
                        shm is disabled in peppy_config, or the shm segment was
                        unavailable, full, or too small for the payload (the
                        producer node's log records segment problems)
             {mixed}  both paths within one run (e.g. the shm pool filled
                        mid-run); the note carries the shm share (shm k/n)
             —          not measured (unreachable edge / no live traffic)
  note       the interface (➔ edges) and, for probe rows, the measured payload
             sizes (request → response; `≥` = schema lower bound)
  Δp50       median vs the previous run on this machine

Benchmarking never triggers a real handler, never publishes onto a real topic,
and never creates a goal."
    )
}

/// Build the box-table data rows (one per measured row), tinted with the shared
/// `stack` palette and with the wide `edge`/`note` cells wrapped. `previous`
/// supplies the Δp50 baseline. The third cell is the `measure` label for the
/// synthetic table and the `clock` confidence for the real-traffic table,
/// matching [`SYNTHETIC_HEADERS`] / [`REAL_HEADERS`]; `net_label` names the
/// network protocol in the `transport` cell. Pure (no IO) so it can be
/// unit-tested.
fn display_rows(
    rows: &[&InterfaceLatency],
    previous: &BTreeMap<String, StoredStats>,
    colorize: bool,
    table: ReportTable,
    net_label: &str,
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
            // relates), followed by any measurement diagnostic / payload summary.
            // Wrapped so a long note doesn't widen the whole table.
            let iface = row
                .via_interface
                .as_deref()
                .map(|i| paint(colorize, NODE_COLOR, i));
            let mut note = match (iface, &row.note) {
                (Some(iface), Some(n)) => format!("{iface}; {n}"),
                (Some(iface), None) => iface,
                (None, Some(n)) => n.clone(),
                (None, None) => String::new(),
            };
            // A mixed row carries its shm share in the note, so the transport
            // column itself stays narrow.
            if row.transport == Transport::Mixed {
                let share = format!("shm {}/{}", row.shm_samples, row.count);
                if note.is_empty() {
                    note = share;
                } else {
                    note = format!("{note}; {share}");
                }
            }
            let measure_or_clock = match table {
                ReportTable::Synthetic => paint(
                    colorize,
                    measurement_color(row.measurement),
                    measurement_label(row.measurement),
                ),
                ReportTable::Real => row.clock_confidence.as_str().to_string(),
            };
            vec![
                edge_cell(row, colorize),
                paint(colorize, BINDING_COLOR, &row.link_id),
                measure_or_clock,
                transport_cell(row, net_label, colorize),
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
        transport: Transport,
        shm_samples: u64,
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
            transport,
            shm_samples,
            p50_ns: 5_500_000,
            p90_ns: 7_550_000,
            mean_ns: 5_610_000,
            count: 200,
            samples_ns: vec![],
            note: note.map(str::to_string),
        }
    }

    /// Rows mirroring the real stack: a direct action edge, an interface-
    /// conformance topic edge measured both ways (node-probe + delivery), and an
    /// interface-conformance service-probe edge whose payload note is long
    /// enough to exercise wrapping. Transports cover network (action probe),
    /// shm (delivery), and mixed (svc-probe — its note also gains the share).
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
                Transport::Network,
                0,
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
                Transport::Shm,
                200,
                None,
            ),
            row(
                "my_python_robot_brain",
                "uvc_camera_python_mock",
                "video_stream_info",
                "camera",
                Some("uvc_camera:v1"),
                InterfaceKind::Service,
                MeasurementKind::ServiceProbe,
                ClockConfidence::NotApplicable,
                Transport::Mixed,
                150,
                Some("payload 64B → 4.0KB (rebuild producer for sized replies)"),
            ),
            row(
                "uvc_camera_video_reconstruction",
                "uvc_camera_python_mock",
                "video_stream",
                "camera",
                Some("uvc_camera:v1"),
                InterfaceKind::Topic,
                MeasurementKind::NodeProbe,
                ClockConfidence::NotApplicable,
                Transport::Network,
                0,
                Some("payload 0B → ≥56B"),
            ),
        ]
    }

    /// `sample_rows` split the way `render_report` splits them.
    fn split_rows(rows: &[InterfaceLatency]) -> (Vec<&InterfaceLatency>, Vec<&InterfaceLatency>) {
        rows.iter()
            .partition(|r| r.measurement.is_synthetic_probe())
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
    fn rows_partition_into_synthetic_and_real_tables() {
        let rows = sample_rows();
        let (synthetic, real) = split_rows(&rows);
        // act-probe + svc-probe + node-probe are synthetic; delivery is real.
        assert_eq!(synthetic.len(), 3);
        assert_eq!(real.len(), 1);
        assert!(
            synthetic
                .iter()
                .all(|r| r.measurement != MeasurementKind::TopicDelivery)
        );
        assert_eq!(real[0].measurement, MeasurementKind::TopicDelivery);
    }

    #[test]
    fn note_names_interface_and_wraps_long_payload_note() {
        let rows = sample_rows();
        let (synthetic, real) = split_rows(&rows);
        // Delivery row's note is just the interface name.
        let real_rows = display_rows(&real, &BTreeMap::new(), false, ReportTable::Real, "tcp");
        assert_eq!(real_rows[0][9], "uvc_camera:v1");
        // The svc-probe row's note leads with the interface, then the wrapped
        // payload summary.
        let synth_rows = display_rows(
            &synthetic,
            &BTreeMap::new(),
            false,
            ReportTable::Synthetic,
            "tcp",
        );
        let note = &synth_rows[1][9];
        assert!(note.starts_with("uvc_camera:v1;"));
        assert!(note.contains('\n'), "long note should wrap");
        assert!(
            note.lines()
                .all(|l| UnicodeWidthStr::width(l) <= NOTE_WRAP_COLS),
            "every note line within wrap width:\n{note}"
        );
    }

    #[test]
    fn transport_cell_renders_each_value() {
        let rows = sample_rows();
        let mut shm = rows[1].clone();
        shm.transport = Transport::Shm;
        let mut net = rows[0].clone();
        net.transport = Transport::Network;
        let mut mixed = rows[2].clone();
        mixed.transport = Transport::Mixed;
        let mut unmeasured = rows[3].clone();
        unmeasured.transport = Transport::NotMeasured;

        // Plain cells carry the bare labels; the network label is the
        // daemon-stamped protocol name.
        assert_eq!(transport_cell(&shm, "tcp", false), "shm");
        assert_eq!(transport_cell(&net, "tcp", false), "tcp");
        assert_eq!(transport_cell(&mixed, "tcp", false), "mixed");
        assert_eq!(transport_cell(&unmeasured, "tcp", false), "—");
        // An unstamped protocol (e.g. mock messenger) falls back to "net".
        assert_eq!(transport_cell(&net, "net", false), "net");
        // Colored: shm green, mixed yellow; the network path is normal, not a
        // fault, so it stays unpainted (as does the unmeasured dash).
        assert!(transport_cell(&shm, "tcp", true).starts_with(TRANSPORT_SHM_COLOR));
        assert!(transport_cell(&mixed, "tcp", true).starts_with(TRANSPORT_MIXED_COLOR));
        assert!(!transport_cell(&net, "tcp", true).contains('\x1b'));
        assert!(!transport_cell(&unmeasured, "tcp", true).contains('\x1b'));
        assert_ne!(
            TRANSPORT_SHM_COLOR, TRANSPORT_MIXED_COLOR,
            "shm and mixed must be visually distinct"
        );
    }

    #[test]
    fn mixed_transport_appends_shm_share_to_note() {
        let rows = sample_rows();
        let (synthetic, _) = split_rows(&rows);
        let synth_rows = display_rows(
            &synthetic,
            &BTreeMap::new(),
            false,
            ReportTable::Synthetic,
            "tcp",
        );
        // The mixed svc-probe row's note ends with its shm share, still
        // within the wrap width.
        let note = &synth_rows[1][9];
        let unwrapped = note.replace('\n', " ");
        assert!(
            unwrapped.contains("shm 150/200"),
            "mixed note should carry the shm share: {note}"
        );
        assert!(
            note.lines()
                .all(|l| UnicodeWidthStr::width(l) <= NOTE_WRAP_COLS),
            "every note line within wrap width:\n{note}"
        );
        // Non-mixed rows carry no share.
        assert!(!synth_rows[0][9].contains("shm "), "{}", synth_rows[0][9]);
    }

    #[test]
    fn synthetic_table_shows_measure_and_real_table_shows_clock() {
        let rows = sample_rows();
        let (synthetic, real) = split_rows(&rows);
        // Without color the synthetic third cell is the plain measure label,
        // and the fourth is the transport.
        let plain = display_rows(
            &synthetic,
            &BTreeMap::new(),
            false,
            ReportTable::Synthetic,
            "tcp",
        );
        assert_eq!(plain[0][2], "act-probe");
        assert_eq!(plain[1][2], "svc-probe");
        assert_eq!(plain[2][2], "node-probe");
        assert_eq!(plain[0][3], "tcp");
        assert_eq!(plain[1][3], "mixed");
        assert_eq!(plain[2][3], "tcp");
        // With color each kind carries its own distinct code.
        let colored = display_rows(
            &synthetic,
            &BTreeMap::new(),
            true,
            ReportTable::Synthetic,
            "tcp",
        );
        assert!(colored[0][2].starts_with(MEASURE_ACTION_COLOR));
        assert!(colored[1][2].starts_with(MEASURE_SERVICE_COLOR));
        assert!(colored[2][2].starts_with(MEASURE_NODE_COLOR));
        // The probe colors are mutually distinct (delivery has its own legend
        // color, also distinct from all three).
        let colors = [
            MEASURE_ACTION_COLOR,
            MEASURE_SERVICE_COLOR,
            MEASURE_NODE_COLOR,
            MEASURE_DELIVERY_COLOR,
        ];
        for (i, a) in colors.iter().enumerate() {
            for b in &colors[i + 1..] {
                assert_ne!(a, b, "measure colors must be pairwise distinct");
            }
        }
        // The real table's third cell is the clock confidence, uncolored; its
        // fourth is the transport (the delivery fixture rode shm, painted).
        let real_rows = display_rows(&real, &BTreeMap::new(), true, ReportTable::Real, "tcp");
        assert_eq!(real_rows[0][2], "corrected");
        assert!(real_rows[0][3].starts_with(TRANSPORT_SHM_COLOR));
    }

    #[test]
    fn legend_color_key_stays_aligned_and_plain_when_uncolored() {
        // Painting the labels must not shift their printed width: the colored and
        // plain legends agree once escape codes are stripped, and the plain legend
        // carries none. (The renderer strips ANSI before measuring, so equal
        // visible width = aligned.)
        let plain = benchmark_legend(false, "tcp");
        assert!(
            !plain.contains('\x1b'),
            "no-color legend must be escape-free"
        );
        let stripped = benchmark_legend(true, "tcp")
            .replace(MEASURE_SERVICE_COLOR, "")
            .replace(MEASURE_ACTION_COLOR, "")
            .replace(MEASURE_NODE_COLOR, "")
            .replace(MEASURE_DELIVERY_COLOR, "")
            .replace(TRANSPORT_SHM_COLOR, "")
            .replace(TRANSPORT_MIXED_COLOR, "")
            .replace(super::super::colors::RESET, "");
        assert_eq!(stripped, plain, "color codes must be width-neutral");
        // The legend names all four measurement labels and the safety guarantee.
        for label in ["svc-probe", "act-probe", "node-probe", "delivery"] {
            assert!(plain.contains(label), "legend missing {label}");
        }
        // The transport block names its labels, the observation point, and
        // every code-grounded reason a payload takes the network path.
        for needle in [
            "transport",
            "shm",
            "mixed",
            "tcp",
            "reply leg",
            "4 KiB",
            "another host",
            "namespace",
            "disabled",
        ] {
            assert!(plain.contains(needle), "legend missing {needle}");
        }
        assert!(plain.contains("never publishes onto a real topic"));
    }

    #[test]
    fn both_tables_render_boxes_and_stay_narrow() {
        let rows = sample_rows();
        let (synthetic, real) = split_rows(&rows);
        let tables = [
            (
                display_rows(
                    &synthetic,
                    &BTreeMap::new(),
                    false,
                    ReportTable::Synthetic,
                    "tcp",
                ),
                &SYNTHETIC_HEADERS,
            ),
            (
                display_rows(&real, &BTreeMap::new(), false, ReportTable::Real, "tcp"),
                &REAL_HEADERS,
            ),
        ];
        for (rows, headers) in tables {
            let mut out = String::new();
            render_table(&mut out, headers.as_slice(), &[rows]);

            // Box-drawing borders like `stack list`.
            assert!(out.contains('┌') && out.contains('│') && out.contains('└'));
            // Headers present.
            for h in *headers {
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
}
